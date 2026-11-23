//! Graph executor tests (spec §5.7), with a simulated clock: deterministic
//! and instant.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeDelta, Utc};
use ilus::action::{Action, ActionContext};
use ilus::condition::Condition;
use ilus::error::{ActionError, ModifierError};
use ilus::executor::{Clock, Executor};
use ilus::graph::Graph;
use ilus::modifier::{ModifierHandle, ModifierValue};
use ilus::source::mock::MockSource;
use ilus::source::{Item, SourceData, Timeframe};
use ilus::transformer::{Derived, IndicatorPoint, Transformer};
use ilus::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ----------------------------------------------------------------- helpers

type Log = Arc<Mutex<Vec<String>>>;

fn log(l: &Log, s: String) {
    l.lock().unwrap().push(s);
}

fn entries(l: &Log) -> Vec<String> {
    l.lock().unwrap().clone()
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::UNIX_EPOCH
}

fn points(values: &[i64]) -> Vec<IndicatorPoint> {
    values
        .iter()
        .zip(0..)
        .map(|(&v, i)| IndicatorPoint {
            time: epoch() + TimeDelta::minutes(i),
            value: Decimal::from(v),
        })
        .collect()
}

fn replay(values: &[i64]) -> MockSource<IndicatorPoint> {
    MockSource::replay("T", Timeframe::M1, points(values))
}

async fn run(g: Graph) {
    Executor::new(g)
        .with_clock(Clock::Simulated(epoch()))
        .run()
        .await;
}

/// Items carrying a value.
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

/// Multiplies values by a factor (changeable by modifier); logs each call.
struct Scale<I> {
    name: &'static str,
    factor: Decimal,
    warm_up: usize,
    log: Log,
    input: PhantomData<fn(I)>,
}

fn scale<I>(name: &'static str, warm_up: usize, log: &Log) -> Scale<I> {
    Scale {
        name,
        factor: dec!(1),
        warm_up,
        log: log.clone(),
        input: PhantomData,
    }
}

#[derive(Debug)]
struct SetFactor(Decimal);
impl ModifierValue for SetFactor {}

impl<I: Val> Transformer for Scale<I> {
    type Input = I;
    type Value = Decimal;
    type Modifier = SetFactor;

    fn name(&self) -> String {
        self.name.into()
    }
    fn warm_up(&self) -> usize {
        self.warm_up
    }
    fn on_data(&mut self, item: Item<'_, I>) -> Option<Decimal> {
        let v = item.data.val()? * self.factor;
        log(&self.log, format!("{}:{v}", self.name));
        Some(v)
    }
    fn on_modifier(&mut self, m: SetFactor) -> Result<(), ModifierError> {
        self.factor = m.0;
        Ok(())
    }
}

/// Triggers when value >= threshold; logs each call with the item mode.
struct AtLeast<I> {
    name: &'static str,
    threshold: Decimal,
    warm_up: usize,
    log: Log,
    input: PhantomData<fn(I)>,
}

fn at_least<I>(name: &'static str, threshold: Decimal, warm_up: usize, log: &Log) -> AtLeast<I> {
    AtLeast {
        name,
        threshold,
        warm_up,
        log: log.clone(),
        input: PhantomData,
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

    fn warm_up(&self) -> usize {
        self.warm_up
    }
    fn on_data(&mut self, item: Item<'_, I>) -> Option<Detection<Hit>> {
        let v = item.data.val()?;
        log(&self.log, format!("{}:{v}:{:?}", self.name, item.mode));
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

/// Logs each event; optionally sends modifiers on the first event, awaits,
/// or fails.
struct Record<I> {
    name: &'static str,
    log: Log,
    with_time: bool,
    to_condition: Option<(ModifierHandle<SetThreshold>, Vec<SetThreshold>)>,
    to_transformer: Option<(ModifierHandle<SetFactor>, Decimal)>,
    awaits: bool,
    fail: bool,
    input: PhantomData<fn(I)>,
}

fn record<I>(name: &'static str, log: &Log) -> Record<I> {
    Record {
        name,
        log: log.clone(),
        with_time: false,
        to_condition: None,
        to_transformer: None,
        awaits: false,
        fail: false,
        input: PhantomData,
    }
}

impl<I: Val> Action for Record<I> {
    type Value = Hit;
    type Input = I;

    async fn run(
        &mut self,
        event: TriggerEvent<'_, Hit, I>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let mut entry = format!("{}:{}:{:?}", self.name, event.value.0, event.mode);
        if self.with_time {
            entry += &format!(":{}", ctx.now().format("%H:%M:%S"));
        }
        if self.awaits {
            log(&self.log, format!("{}:start", self.name));
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        log(&self.log, entry);
        if let Some((to, modifiers)) = self.to_condition.take() {
            for m in modifiers {
                ctx.send(&to, m);
            }
        }
        if let Some((to, f)) = self.to_transformer.take() {
            ctx.send(&to, SetFactor(f));
        }
        if self.fail {
            return Err(ActionError::Failed("simulated".into()));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn nodes_run_in_graph_order_with_fan_out() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(replay(&[1, 2]));
    let t = g.transform(scale("t", 0, &l), &src);
    let c1 = g.condition("c1", at_least("c1", dec!(0), 0, &l), &t);
    let c2 = g.condition("c2", at_least("c2", dec!(0), 0, &l), &t);
    g.action(record("a1", &l), &c1);
    g.action(record("a2", &l), &c2);
    run(g).await;
    // Per tick: transformer once, both conditions, then the actions.
    assert_eq!(
        entries(&l),
        [
            "t:1",
            "c1:1:Replay",
            "c2:1:Replay",
            "a1:1:Replay",
            "a2:1:Replay", //
            "t:2",
            "c1:2:Replay",
            "c2:2:Replay",
            "a1:2:Replay",
            "a2:2:Replay",
        ]
    );
}

#[tokio::test]
async fn bootstrap_items_warm_up_without_action_events() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(replay(&[0, 1, 2, 3, 4]));
    let c = g.condition("c", at_least("c", dec!(0), 2, &l), &src); // warm-up 2
    g.action(record("a", &l), &c);
    run(g).await;
    assert_eq!(
        entries(&l),
        [
            "c:0:Bootstrap",
            "c:1:Bootstrap",
            "c:2:Replay",
            "a:2:Replay",
            "c:3:Replay",
            "a:3:Replay",
            "c:4:Replay",
            "a:4:Replay",
        ]
    );
}

#[tokio::test]
async fn bootstrap_length_is_the_max_over_paths_of_summed_warm_ups() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(replay(&[0, 1, 2, 3, 4, 5, 6, 7]));
    // Path 1: transformer (3) -> condition (2) = 5. Path 2: condition (4) = 4.
    let t = g.transform(scale("t", 3, &l), &src);
    let c1 = g.condition("c1", at_least("c1", dec!(0), 2, &l), &t);
    let c2 = g.condition("c2", at_least("c2", dec!(0), 4, &l), &src);
    g.action(record("a1", &l), &c1);
    g.action(record("a2", &l), &c2);
    run(g).await;
    let first_event = |a: &str| entries(&l).into_iter().find(|e| e.starts_with(a)).unwrap();
    assert_eq!(first_event("a1:"), "a1:5:Replay");
    assert_eq!(first_event("a2:"), "a2:5:Replay");
}

#[tokio::test]
async fn modifiers_are_applied_at_the_end_of_the_tick_in_send_order() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(replay(&[1, 2, 3, 4, 5]));
    let c = g.condition("c", at_least("c", dec!(0), 0, &l), &src);
    let mut a = record("a", &l);
    // On the first event: 100 then 4 (the last one wins), and an invalid one.
    a.to_condition = Some((
        g.modifier_handle(&c),
        vec![
            SetThreshold::To(dec!(100)),
            SetThreshold::Invalid,
            SetThreshold::To(dec!(4)),
        ],
    ));
    g.action(a, &c);
    run(g).await;
    let events: Vec<String> = entries(&l)
        .into_iter()
        .filter(|e| e.starts_with("a:"))
        .collect();
    // Item 1 triggers; from item 2 (next tick) the threshold is 4.
    assert_eq!(events, ["a:1:Replay", "a:4:Replay", "a:5:Replay"]);
}

#[tokio::test]
async fn actions_can_modify_a_transformer() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(replay(&[1, 2, 3]));
    let t = g.transform(scale("t", 0, &l), &src);
    let c = g.condition("c", at_least("c", dec!(0), 0, &l), &t);
    let mut a = record("a", &l);
    a.to_transformer = Some((g.modifier_handle(&t), dec!(10)));
    g.action(a, &c);
    run(g).await;
    let t_values: Vec<String> = entries(&l)
        .into_iter()
        .filter(|e| e.starts_with("t:"))
        .collect();
    assert_eq!(t_values, ["t:1", "t:20", "t:30"]);
}

#[tokio::test]
async fn sources_in_replay_are_called_one_after_the_other_each_tick() {
    let l = Log::default();
    let mut g = Graph::new();
    let a = g.source(replay(&[1, 2, 3]));
    let b = g.source(replay(&[10]));
    g.condition("ca", at_least("ca", dec!(1000), 0, &l), &a);
    g.condition("cb", at_least("cb", dec!(1000), 0, &l), &b);
    run(g).await;
    assert_eq!(
        entries(&l),
        ["ca:1:Replay", "cb:10:Replay", "ca:2:Replay", "ca:3:Replay"]
    );
}

#[tokio::test]
async fn actions_are_awaited_within_the_tick() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(replay(&[1, 2]));
    let c = g.condition("c", at_least("c", dec!(0), 0, &l), &src);
    let mut a = record("a", &l);
    a.awaits = true;
    g.action(a, &c);
    run(g).await;
    assert_eq!(
        entries(&l),
        [
            "c:1:Replay",
            "a:start",
            "a:1:Replay",
            "c:2:Replay",
            "a:start",
            "a:2:Replay",
        ]
    );
}

#[tokio::test]
async fn failing_action_does_not_stop_the_tick() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(replay(&[1, 2]));
    let c = g.condition("c", at_least("c", dec!(0), 0, &l), &src);
    let mut a1 = record("a1", &l);
    a1.fail = true;
    g.action(a1, &c);
    g.action(record("a2", &l), &c);
    run(g).await;
    let events: Vec<String> = entries(&l)
        .into_iter()
        .filter(|e| e.starts_with('a'))
        .collect();
    assert_eq!(
        events,
        ["a1:1:Replay", "a2:1:Replay", "a1:2:Replay", "a2:2:Replay"]
    );
}

#[tokio::test]
async fn normal_mode_is_called_just_after_each_candle_close() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(MockSource::live("T", Timeframe::M1, points(&[1, 2, 3])));
    let c = g.condition("c", at_least("c", dec!(0), 0, &l), &src);
    let mut a = record("a", &l);
    a.with_time = true;
    g.action(a, &c);
    run(g).await;
    let events: Vec<String> = entries(&l)
        .into_iter()
        .filter(|e| e.starts_with("a:"))
        .collect();
    // Candles open at 00:00, 00:01, 00:02: delivered at close + 2 s.
    assert_eq!(
        events,
        [
            "a:1:Normal:00:01:02",
            "a:2:Normal:00:02:02",
            "a:3:Normal:00:03:02",
        ]
    );
}

#[tokio::test]
async fn bootstrap_then_normal_mode() {
    let l = Log::default();
    let mut g = Graph::new();
    let src = g.source(MockSource::live("T", Timeframe::M1, points(&[1, 2, 3])).with_bootstrap(2));
    let c = g.condition("c", at_least("c", dec!(0), 0, &l), &src);
    g.action(record("a", &l), &c);
    run(g).await;
    assert_eq!(
        entries(&l),
        ["c:1:Bootstrap", "c:2:Bootstrap", "c:3:Normal", "a:3:Normal"]
    );
}

#[tokio::test]
async fn replay_is_deterministic() {
    async fn once() -> Vec<String> {
        let l = Log::default();
        let mut g = Graph::new();
        let src = g.source(replay(&[3, 1, 4, 1, 5, 9, 2, 6]));
        let t = g.transform(scale("t", 1, &l), &src);
        let c = g.condition("c", at_least("c", dec!(3), 1, &l), &t);
        let mut a = record("a", &l);
        a.to_condition = Some((g.modifier_handle(&c), vec![SetThreshold::To(dec!(5))]));
        g.action(a, &c);
        run(g).await;
        entries(&l)
    }
    let first = once().await;
    for _ in 0..5 {
        assert_eq!(once().await, first);
    }
}

#[tokio::test]
async fn a_node_never_processes_the_same_item_twice() {
    let l = Log::default();
    let mut g = Graph::new();
    // Items at 00:00, 00:00 (same time again), 00:01.
    let mut items = points(&[1, 2, 3]);
    items[1].time = items[0].time;
    let src = g.source(MockSource::replay("T", Timeframe::M1, items));
    let t = g.transform(scale("t", 0, &l), &src);
    g.condition("c", at_least("c", dec!(1000), 0, &l), &t);
    run(g).await;
    assert_eq!(entries(&l), ["t:1", "c:1:Replay", "t:3", "c:3:Replay"]);
}

mod merge {
    use super::*;
    use ilus::source::merge::{MergeSource, Merged};
    use ilus::transformer::combine::{Combine, OnMain, OnOther};

    /// Counts the calls of its inner logic: value = number of new items seen.
    struct Counter;

    impl Transformer for Counter {
        type Input = IndicatorPoint;
        type Value = Decimal;
        type Modifier = ilus::modifier::NoModifier;
        fn name(&self) -> String {
            "counter".into()
        }
        fn warm_up(&self) -> usize {
            0
        }
        fn on_data(&mut self, item: Item<'_, IndicatorPoint>) -> Option<Decimal> {
            Some(item.data.value)
        }
        fn on_modifier(&mut self, m: ilus::modifier::NoModifier) -> Result<(), ModifierError> {
            match m {}
        }
    }

    type Pair = (Option<Decimal>, Option<Decimal>);
    type Input = Derived<Merged<IndicatorPoint, IndicatorPoint>, Pair>;

    /// Logs the pair (main value, other value) of each merged item.
    struct LogPair(Log);

    impl Condition for LogPair {
        type Input = Input;
        type Value = Hit;
        type Modifier = ilus::modifier::NoModifier;
        fn warm_up(&self) -> usize {
            0
        }
        fn on_data(&mut self, item: Item<'_, Input>) -> Option<Detection<Hit>> {
            let (main, other) = item.data.value?;
            log(&self.0, format!("{main:?}/{other:?}"));
            None
        }
        fn on_modifier(&mut self, m: ilus::modifier::NoModifier) -> Result<(), ModifierError> {
            match m {}
        }
    }

    fn series(step_min: i64, n: i64) -> Vec<IndicatorPoint> {
        (0..n)
            .map(|i| IndicatorPoint {
                time: epoch() + TimeDelta::minutes(i * step_min),
                value: Decimal::from(i * step_min),
            })
            .collect()
    }

    #[tokio::test]
    async fn merged_source_feeds_a_combined_transformer() {
        let l = Log::default();
        let mut g = Graph::new();
        let merged = g.source(MergeSource::new(
            MockSource::replay("M", Timeframe::M15, series(15, 8)),
            MockSource::replay("H", Timeframe::H1, series(60, 2)),
        ));
        let both = g.transform(
            Combine::new((OnMain::new(Counter), OnOther::new(Counter))),
            &merged,
        );
        g.condition("pairs", LogPair(l.clone()), &both);
        run(g).await;
        // Values are the minutes of each item; the 1h one changes only when
        // a new 1h candle is closed (no look-ahead).
        assert_eq!(
            entries(&l),
            [
                "Some(0)/None",
                "Some(15)/None",
                "Some(30)/None",
                "Some(45)/Some(0)",
                "Some(60)/Some(0)",
                "Some(75)/Some(0)",
                "Some(90)/Some(0)",
                "Some(105)/Some(60)",
            ]
        );
    }
}
