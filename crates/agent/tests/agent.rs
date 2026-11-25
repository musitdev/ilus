//! Multi-rule agent wired in a graph (spec F9): a rule's votes trigger the
//! decision in the same tick.

use std::sync::{Arc, Mutex};

use agent::decision::{Decision, DecisionEvent, DecisionKind, DecisionParams};
use agent::voters::TrendVoter;
use agent::{SendVotes, Side, Weights};
use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use trading::action::{Action, ActionContext};
use trading::candle::Candle;
use trading::error::ActionError;
use trading::executor::{Clock, Executor};
use trading::graph::Graph;
use trading::source::Timeframe;
use trading::source::mock::MockSource;
use trading::transformer::trend::Trend;
use trading::trigger::TriggerEvent;

type Log = Arc<Mutex<Vec<(i64, DecisionKind)>>>;

/// Records the decisions (minute, kind), not the ticks.
struct Record(Log);

impl Action for Record {
    type Value = DecisionEvent;
    type Input = Candle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, DecisionEvent, Candle>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        if event.value.kind != DecisionKind::Tick
            && let Ok(mut l) = self.0.lock()
        {
            let minute = (event.data.open_time - DateTime::<Utc>::UNIX_EPOCH).num_minutes();
            l.push((minute, event.value.kind));
        }
        Ok(())
    }
}

fn candles(closes: &[i64]) -> Vec<Candle> {
    closes
        .iter()
        .zip(0..)
        .map(|(&c, i)| {
            let c = Decimal::from(c);
            Candle {
                open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
                open: c,
                high: c,
                low: c,
                close: c,
                volume: Decimal::ONE,
            }
        })
        .collect()
}

#[test]
fn votes_trigger_the_decision_in_the_same_tick() {
    // Trend EMA 2: flat (10, 10), up at minute 4 (13), down at minute 6 (5).
    let closes = [10, 10, 10, 10, 13, 14, 5, 4];
    let log: Log = Arc::default();
    let mut g = Graph::new();
    let src = g.source(MockSource::replay("T", Timeframe::M1, candles(&closes)));
    let decision = g.condition(
        "decision",
        Decision::new(DecisionParams {
            open_threshold: dec!(1),
            exit_threshold: dec!(1),
        })
        .unwrap(),
        &src,
    );
    g.action(Record(log.clone()), &decision);
    let to = g.modifier_handle(&decision);
    let trend = g.transform(Trend::<Candle>::new(2).unwrap(), &src);
    let voter = g.condition("trend", TrendVoter::default(), &trend);
    let weights = Weights {
        open: dec!(1),
        exit: dec!(1),
    };
    g.action(SendVotes::new("trend", weights, to), &voter);
    Executor::new(g)
        .with_clock(Clock::Simulated(DateTime::<Utc>::UNIX_EPOCH))
        .run()
        .unwrap();
    // Minute 4: trend up -> open long; minute 6: trend down -> exit long
    // (the short ballot of the same candle is ignored: in position).
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            (4, DecisionKind::Open(Side::Long)),
            (6, DecisionKind::Exit(Side::Long)),
        ]
    );
}
