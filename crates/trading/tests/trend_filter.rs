//! Trend filter wired in a graph: candles -> EMA -> Trend -> TrendFilter(rule 1).

use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;
use trading::action::{Action, ActionContext};
use trading::candle::Candle;
use trading::condition::Signal;
use trading::condition::ema_rules::{CrossSignal, PriceCrossEma};
use trading::condition::filter::TrendFilter;
use trading::error::ActionError;
use trading::executor::{Clock, Executor};
use trading::graph::Graph;
use trading::source::Timeframe;
use trading::source::mock::MockSource;
use trading::transformer::Derived;
use trading::transformer::ema::Ema;
use trading::transformer::trend::{Trend, TrendValue};
use trading::trigger::TriggerEvent;

type Log = Arc<Mutex<Vec<(usize, Signal)>>>;

struct Record(Log);

impl Action for Record {
    type Value = CrossSignal;
    type Input = Derived<Derived<Candle, Decimal>, TrendValue>;

    fn run(
        &mut self,
        event: TriggerEvent<'_, CrossSignal, Self::Input>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let minute = (event.time - DateTime::<Utc>::UNIX_EPOCH).num_minutes() as usize;
        if let Ok(mut l) = self.0.lock() {
            l.push((minute, event.value.signal));
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

fn run(enabled: bool) -> Vec<(usize, Signal)> {
    // Long uptrend, then a zigzag above the slow EMA: the fast rule (close x
    // EMA 2) gives buys and sells; the trend (EMA 8) stays up.
    let closes = [
        10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 30, 26, 31, 27, 32,
    ];
    let log: Log = Arc::default();
    let mut g = Graph::new();
    let src = g.source(MockSource::replay("T", Timeframe::M1, candles(&closes)));
    let ema = g.transform(Ema::new(2).unwrap(), &src);
    let trend = g.transform(Trend::new(8).unwrap(), &ema);
    let rule = if enabled {
        TrendFilter::new(PriceCrossEma::default())
    } else {
        TrendFilter::disabled(PriceCrossEma::default())
    };
    let c = g.condition("price-x-ema2", rule, &trend);
    g.action(Record(log.clone()), &c);
    Executor::new(g)
        .with_clock(Clock::Simulated(DateTime::<Utc>::UNIX_EPOCH))
        .run()
        .unwrap();
    log.lock().unwrap().clone()
}

#[test]
fn only_buys_pass_in_an_uptrend() {
    let all = run(false);
    let kept = run(true);
    assert!(
        all.iter().any(|&(_, s)| s == Signal::Bearish),
        "unfiltered: some sells {all:?}"
    );
    assert!(!kept.is_empty(), "filtered: some buys");
    assert!(kept.iter().all(|&(_, s)| s == Signal::Bullish), "{kept:?}");
    let all_buys: Vec<_> = all
        .into_iter()
        .filter(|&(_, s)| s == Signal::Bullish)
        .collect();
    assert_eq!(kept, all_buys, "every buy passes, at the same candles");
}
