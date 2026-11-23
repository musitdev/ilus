//! Runtime behavior tests (spec F1–F5), using a paused tokio clock so timing
//! is deterministic.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use ilus::action::{Action, EventOf};
use ilus::bot::Bot;
use ilus::condition::Condition;
use ilus::error::SourceError;
use ilus::error::{ActionError, ModifierError};
use ilus::modifier::{ModifierSender, ModifierValue, NoModifier};
use ilus::rule::Rule;
use ilus::source::SourceData;
use ilus::source::mock::MockSource;
use ilus::source::{Source, Timeframe};
use ilus::transformer::{Derived, IndicatorPoint, Transformed, Transformer};
use ilus::trigger::{Detection, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ----------------------------------------------------------------- helpers

fn points(values: &[Decimal]) -> Vec<IndicatorPoint> {
    values
        .iter()
        .zip(0..)
        .map(|(&value, i)| IndicatorPoint {
            time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
            value,
        })
        .collect()
}

fn mock(values: &[Decimal]) -> MockSource<IndicatorPoint> {
    MockSource::new("TEST", Timeframe::M1, points(values)).with_interval(Duration::from_secs(1))
}

/// Items carrying an optional value to compare.
trait Val: SourceData {
    fn val(&self) -> Option<Decimal>;
}
impl Val for IndicatorPoint {
    fn val(&self) -> Option<Decimal> {
        Some(self.value)
    }
}
impl<D: SourceData> Val for Derived<D, Decimal> {
    fn val(&self) -> Option<Decimal> {
        self.value
    }
}

/// Times at which the condition read each item (to observe backpressure).
type ReadLog = Arc<Mutex<Vec<(Decimal, Duration)>>>;

/// Triggers when value >= threshold.
struct AtLeast<I = IndicatorPoint> {
    threshold: Decimal,
    input: PhantomData<fn(I)>,
    reads: Option<(ReadLog, tokio::time::Instant)>,
}

fn at_least<I>(threshold: Decimal) -> AtLeast<I> {
    AtLeast {
        threshold,
        input: PhantomData,
        reads: None,
    }
}

#[derive(Debug)]
struct Hit(Decimal);
impl TriggerValue for Hit {}

#[derive(Debug)]
enum SetThreshold {
    To(Decimal),
    Invalid,
}
impl ModifierValue for SetThreshold {}

impl<I: Val> Condition for AtLeast<I> {
    type Input = I;
    type Value = Hit;
    type Modifier = SetThreshold;

    fn on_data(&mut self, item: &I) -> Option<Detection<Hit>> {
        let v = item.val()?;
        if let Some((log, start)) = &self.reads {
            log.lock().unwrap().push((v, start.elapsed()));
        }
        (v >= self.threshold).then_some(Detection {
            event_type: TriggerEventType::Other("at_least"),
            value: Hit(v),
        })
    }

    fn on_modifier(&mut self, m: SetThreshold) -> Result<(), ModifierError> {
        match m {
            SetThreshold::To(t) => {
                self.threshold = t;
                Ok(())
            }
            SetThreshold::Invalid => Err(ModifierError::Rejected("invalid".into())),
        }
    }
}

/// Records triggering values, optionally sends modifiers, then waits `hold`.
#[derive(Clone, Default)]
struct Record {
    seen: Arc<Mutex<Vec<Decimal>>>,
    hold: Duration,
    send: Vec<Decimal>,
    send_invalid: bool,
}

impl Record {
    fn seen(&self) -> Vec<Decimal> {
        self.seen.lock().unwrap().clone()
    }
}

impl<I: Val> Action<AtLeast<I>> for Record {
    async fn run(
        &mut self,
        event: EventOf<AtLeast<I>>,
        modifiers: &ModifierSender<SetThreshold>,
    ) -> Result<(), ActionError> {
        self.seen.lock().unwrap().push(event.value.0);
        if self.send_invalid {
            modifiers.send(SetThreshold::Invalid).unwrap();
        }
        for &t in &self.send {
            modifiers.send(SetThreshold::To(t)).unwrap();
        }
        if !self.hold.is_zero() {
            tokio::time::sleep(self.hold).await;
        }
        Ok(())
    }
}

fn values(range: std::ops::Range<i64>) -> Vec<Decimal> {
    range.map(Decimal::from).collect()
}

/// Run one mock source feeding one `AtLeast` rule recorded by `rec`.
async fn run_one(values: &[Decimal], threshold: Decimal, rec: &Record) {
    let mut bot = Bot::new();
    let mut src = mock(values);
    bot.rule(Rule::new(
        "r",
        src.get_source_stream().unwrap(),
        at_least(threshold),
        rec.clone(),
    ));
    bot.source(src);
    bot.run().await;
}

// ------------------------------------------------------------------- tests

#[tokio::test(start_paused = true)]
async fn no_event_is_lost_while_action_runs() {
    // Items at t=0..5s; every item matches; action holds 2.5s (slower than the feed).
    let rec = Record {
        hold: Duration::from_millis(2500),
        ..Default::default()
    };
    run_one(&values(0..6), dec!(0), &rec).await;
    assert_eq!(rec.seen(), values(0..6));
}

#[tokio::test(start_paused = true)]
async fn burst_is_processed_in_order_without_loss() {
    // 500 items sent as fast as possible, slow action: nothing is dropped.
    let mut bot = Bot::new();
    let mut src = MockSource::new("TEST", Timeframe::M1, points(&values(0..500)));
    let rec = Record {
        hold: Duration::from_millis(10),
        ..Default::default()
    };
    bot.rule(Rule::new(
        "r",
        src.get_source_stream().unwrap(),
        at_least(dec!(0)),
        rec.clone(),
    ));
    bot.source(src);
    bot.run().await;
    assert_eq!(rec.seen(), values(0..500));
}

#[tokio::test(start_paused = true)]
async fn input_is_paused_until_event_queue_is_empty() {
    // 6 items available at once, max 2 pending events, action takes 1s.
    let mut bot = Bot::new();
    let mut src = MockSource::new("TEST", Timeframe::M1, points(&values(0..6)));
    let reads: ReadLog = Arc::default();
    let mut cond = at_least(dec!(0));
    cond.reads = Some((reads.clone(), tokio::time::Instant::now()));
    let rec = Record {
        hold: Duration::from_secs(1),
        ..Default::default()
    };
    bot.rule(
        Rule::new("r", src.get_source_stream().unwrap(), cond, rec.clone()).with_max_pending(2),
    );
    bot.source(src);
    bot.run().await;

    assert_eq!(rec.seen(), values(0..6), "no event lost");
    // Reads by pairs: queue full after 2 events, resumed when empty (2 s later).
    let secs: Vec<u64> = reads
        .lock()
        .unwrap()
        .iter()
        .map(|(_, t)| t.as_secs())
        .collect();
    assert_eq!(secs, vec![0, 0, 2, 2, 4, 4]);
}

#[tokio::test(start_paused = true)]
async fn modifier_affects_next_items() {
    let rec = Record {
        send: vec![dec!(100)],
        ..Default::default()
    };
    run_one(&values(1..6), dec!(0), &rec).await;
    // After the first event the threshold is 100: nothing else matches.
    assert_eq!(rec.seen(), vec![dec!(1)]);
}

#[tokio::test(start_paused = true)]
async fn modifiers_are_applied_in_send_order() {
    let rec = Record {
        send: vec![dec!(100), dec!(4)], // last one wins
        ..Default::default()
    };
    run_one(&values(1..6), dec!(0), &rec).await;
    assert_eq!(rec.seen(), vec![dec!(1), dec!(4), dec!(5)]);
}

#[tokio::test(start_paused = true)]
async fn invalid_modifier_leaves_condition_unchanged() {
    let rec = Record {
        send_invalid: true,
        ..Default::default()
    };
    run_one(&values(1..4), dec!(0), &rec).await;
    assert_eq!(rec.seen(), values(1..4));
}

#[test]
fn source_stream_can_be_taken_only_once() {
    let mut src = mock(&values(0..3));
    assert!(src.get_source_stream().is_ok());
    assert!(matches!(
        src.get_source_stream(),
        Err(SourceError::StreamTaken(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn source_without_consumer_ends() {
    // The stream is never taken: the source must still terminate.
    let mut bot = Bot::new();
    bot.source(mock(&values(0..3)));
    bot.run().await;
}

/// Sum of the last `n` values.
struct Sum<I> {
    n: usize,
    window: Vec<Decimal>,
    input: PhantomData<fn(I)>,
}

impl<I> Sum<I> {
    fn new(n: usize) -> Self {
        Self {
            n,
            window: vec![],
            input: PhantomData,
        }
    }
}

impl<I: Val> Transformer for Sum<I> {
    type Input = I;
    type Value = Decimal;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("sum({})", self.n)
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }

    fn warm_up(&self) -> usize {
        self.n
    }

    fn on_data(&mut self, item: &I) -> Option<Decimal> {
        let v = item.val()?;
        self.window.push(v);
        if self.window.len() > self.n {
            self.window.remove(0);
        }
        (self.window.len() == self.n).then(|| self.window.iter().sum())
    }
}

#[tokio::test(start_paused = true)]
async fn transformer_warms_up() {
    let mut bot = Bot::new();
    let mut src = mock(&values(1..6)); // 1 2 3 4 5
    let mut sum2 = Transformed::new(Sum::new(2), src.get_source_stream().unwrap()); // 3 5 7 9
    let rec = Record::default();
    bot.rule(Rule::new(
        "r",
        sum2.get_source_stream().unwrap(),
        at_least(dec!(0)),
        rec.clone(),
    ));
    bot.source(src);
    bot.source(sum2);
    bot.run().await;
    assert_eq!(rec.seen(), vec![dec!(3), dec!(5), dec!(7), dec!(9)]);
}

#[tokio::test(start_paused = true)]
async fn transformers_chain() {
    let mut bot = Bot::new();
    let mut src = mock(&values(1..6)); // 1 2 3 4 5
    let mut sum2 = Transformed::new(Sum::new(2), src.get_source_stream().unwrap()); // 3 5 7 9
    let mut sum2x2 = Transformed::new(Sum::new(2), sum2.get_source_stream().unwrap()); // 8 12 16
    let stream = sum2x2.get_source_stream().unwrap();
    assert_eq!(stream.info().to_string(), "sum(2)<sum(2)<mock:TEST:1m>>");

    let rec = Record::default();
    bot.rule(Rule::new("r", stream, at_least(dec!(0)), rec.clone()));
    bot.source(src);
    bot.source(sum2);
    bot.source(sum2x2);
    bot.run().await;
    assert_eq!(rec.seen(), vec![dec!(8), dec!(12), dec!(16)]);
}

#[tokio::test(start_paused = true)]
async fn transformer_forwards_every_input_item() {
    use tokio_stream::StreamExt;

    let mut src = mock(&values(1..6)); // 1 2 3 4 5
    let mut sum2 = Transformed::new(Sum::new(2), src.get_source_stream().unwrap());
    let out = sum2.get_source_stream().unwrap();
    tokio::spawn(src.run());
    tokio::spawn(sum2.run());

    let items: Vec<_> = out.collect().await;
    let inputs: Vec<_> = items.iter().map(|d| d.data.value).collect();
    let sums: Vec<_> = items.iter().map(|d| d.value).collect();
    assert_eq!(inputs, values(1..6));
    assert_eq!(
        sums,
        vec![
            None,
            Some(dec!(3)),
            Some(dec!(5)),
            Some(dec!(7)),
            Some(dec!(9))
        ]
    );
}

/// Multiplies values by a factor; the factor can be changed by a modifier.
struct Scale {
    factor: Decimal,
}

#[derive(Debug)]
struct SetFactor(Decimal);
impl ModifierValue for SetFactor {}

impl Transformer for Scale {
    type Input = IndicatorPoint;
    type Value = Decimal;
    type Modifier = SetFactor;

    fn name(&self) -> String {
        "scale".into()
    }

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, p: &IndicatorPoint) -> Option<Decimal> {
        Some(p.value * self.factor)
    }

    fn on_modifier(&mut self, m: SetFactor) -> Result<(), ModifierError> {
        if m.0 < Decimal::ZERO {
            return Err(ModifierError::Rejected("negative factor".into()));
        }
        self.factor = m.0;
        Ok(())
    }
}

/// Records values and sends `SetFactor(0)` to a transformer on the first event.
#[derive(Clone)]
struct MuteTransformer {
    seen: Arc<Mutex<Vec<Decimal>>>,
    target: ModifierSender<SetFactor>,
}

impl Action<AtLeast<Derived<IndicatorPoint, Decimal>>> for MuteTransformer {
    async fn run(
        &mut self,
        event: EventOf<AtLeast<Derived<IndicatorPoint, Decimal>>>,
        _modifiers: &ModifierSender<SetThreshold>,
    ) -> Result<(), ActionError> {
        self.seen.lock().unwrap().push(event.value.0);
        self.target
            .send(SetFactor(Decimal::ZERO))
            .map_err(|e| ActionError::Failed(e.to_string()))
    }
}

#[tokio::test(start_paused = true)]
async fn transformer_applies_modifiers_in_order_before_next_items() {
    use tokio_stream::StreamExt;

    let mut src = mock(&values(1..4)); // 1 2 3
    let mut scale = Transformed::new(Scale { factor: dec!(1) }, src.get_source_stream().unwrap());
    let out = scale.get_source_stream().unwrap();
    let modifiers = scale.modifier_sender();
    modifiers.send(SetFactor(dec!(-1))).unwrap(); // rejected, factor unchanged
    modifiers.send(SetFactor(dec!(10))).unwrap(); // applied before the first item
    tokio::spawn(src.run());
    tokio::spawn(scale.run());

    let values: Vec<_> = out.map(|d| d.value).collect().await;
    assert_eq!(values, vec![Some(dec!(10)), Some(dec!(20)), Some(dec!(30))]);
}

#[tokio::test(start_paused = true)]
async fn action_can_send_modifiers_to_a_transformer() {
    let mut bot = Bot::new();
    let mut src = mock(&values(1..6)); // 1 2 3 4 5, one per second
    let mut scale = Transformed::new(Scale { factor: dec!(1) }, src.get_source_stream().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let action = MuteTransformer {
        seen: seen.clone(),
        target: scale.modifier_sender(),
    };
    bot.rule(Rule::new(
        "r",
        scale.get_source_stream().unwrap(),
        at_least(dec!(1)),
        action,
    ));
    bot.source(src);
    bot.source(scale);
    bot.run().await;
    // After the first event the factor is 0: later values are 0 < 1.
    assert_eq!(*seen.lock().unwrap(), vec![dec!(1)]);
}

/// Forwards values and records the modifiers it receives.
struct RecordModifiers(Arc<Mutex<Vec<Decimal>>>);

impl Transformer for RecordModifiers {
    type Input = IndicatorPoint;
    type Value = Decimal;
    type Modifier = SetFactor;

    fn name(&self) -> String {
        "record-modifiers".into()
    }

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, p: &IndicatorPoint) -> Option<Decimal> {
        Some(p.value)
    }

    fn on_modifier(&mut self, m: SetFactor) -> Result<(), ModifierError> {
        self.0.lock().unwrap().push(m.0);
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn transformer_applies_modifiers_sent_after_its_input_ended() {
    use tokio_stream::StreamExt;

    let applied = Arc::new(Mutex::new(vec![]));
    let mut src = mock(&values(1..4));
    let mut t = Transformed::new(
        RecordModifiers(applied.clone()),
        src.get_source_stream().unwrap(),
    );
    let out = t.get_source_stream().unwrap();
    let modifiers = t.modifier_sender();
    tokio::spawn(src.run());
    let task = tokio::spawn(t.run());

    // The output ends with the input...
    assert_eq!(out.collect::<Vec<_>>().await.len(), 3);
    // ...but a late modifier (e.g. from an action) is still applied.
    modifiers.send(SetFactor(dec!(7))).unwrap();
    drop(modifiers); // last sender gone: the transformer can stop
    task.await.unwrap().unwrap();
    assert_eq!(*applied.lock().unwrap(), vec![dec!(7)]);
}
