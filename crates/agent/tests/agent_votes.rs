//! Integration test of the multi-rule agent (spec F9) on two months of real
//! SOL/USDT 15m candles (tests/data, no network).
//!
//! The agent graph (15m and 1h divergences, EMA 9/21 cross, trend EMA 50,
//! candlestick patterns) runs with a spy around the decision condition that
//! records every vote modifier it receives. The expected votes are computed
//! a second way, directly on the candle vectors (RSI, EMA, 1h aggregation,
//! pivots, divergence search and the voter rules are re-implemented here,
//! without the library's transformers and conditions). Only the shape
//! detection of the candlestick patterns reuses `PatternCondition` (tested
//! on its own); what the voter does with the patterns is checked here.
//!
//! Both lists must be identical, rule by rule, vote by vote.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use agent::decision::{Decision, DecisionEvent, DecisionParams};
use agent::voters::{DivergenceVoter, EmaCrossVoter, PatternVoter, TrendVoter};
use agent::{Ballot, DecisionModifier, SendVotes, Side, Weights};
use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use trading::candle::Candle;
use trading::condition::divergence::DivergenceParams;
use trading::condition::patterns::{PatternCondition, ThresholdSet};
use trading::condition::projected::{Projected, aggregated};
use trading::condition::{Condition, Signal};
use trading::error::ModifierError;
use trading::executor::{Clock, Executor};
use trading::graph::Graph;
use trading::modifier::ModifierHandle;
use trading::source::mock::MockSource;
use trading::source::{Item, Mode, Timeframe};
use trading::transformer::aggregate::Aggregate;
use trading::transformer::combine::{Combine, OnValue};
use trading::transformer::ema::Ema;
use trading::transformer::rsi::Rsi;
use trading::transformer::trend::Trend;
use trading::trigger::Detection;

const DATA: &str = include_str!("data/solusdt_15m_2026-09-24_2026-11-24.csv");
const BOOTSTRAP: usize = 104;
const EXIT_HIGH: Decimal = dec!(75);
const EXIT_LOW: Decimal = dec!(30);

/// One recorded vote: (candle time, ballot, weight).
type Votes = BTreeMap<String, Vec<(DateTime<Utc>, Ballot, Decimal)>>;

fn load() -> Vec<Candle> {
    DATA.lines()
        .skip(1)
        .map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            let d = |i: usize| Decimal::from_str(f[i]).unwrap();
            Candle {
                open_time: DateTime::from_timestamp_millis(f[0].parse().unwrap()).unwrap(),
                open: d(1),
                high: d(2),
                low: d(3),
                close: d(4),
                volume: d(5),
            }
        })
        .collect()
}

fn weights(rule: &str) -> Weights {
    let (open, exit) = match rule {
        "div-15m" => (dec!(2), dec!(1)),
        "div-1h" => (dec!(3), dec!(1.5)),
        "ema-9-21" => (dec!(1), dec!(0.75)),
        "trend-50" => (dec!(1.25), dec!(0.5)),
        _ => (dec!(0.8), dec!(0.4)),
    };
    Weights { open, exit }
}

fn div_params(timeframe: Timeframe) -> DivergenceParams {
    DivergenceParams::for_timeframe(timeframe) // 3 / 3, RSI 30 / 70, lookahead 3
}

// ------------------------------------------------------------ graph + spy

/// Wraps the decision: records every vote with the time of its candle.
struct Spy {
    inner: Decision<Candle>,
    current: Option<DateTime<Utc>>,
    votes: Arc<Mutex<Votes>>,
    first_replay: Arc<Mutex<Option<DateTime<Utc>>>>,
}

impl Condition for Spy {
    type Input = Candle;
    type Value = DecisionEvent;
    type Modifier = DecisionModifier;

    fn warm_up(&self) -> usize {
        self.inner.warm_up()
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Detection<DecisionEvent>> {
        self.current = Some(item.data.open_time);
        if item.mode == Mode::Replay
            && let Ok(mut first) = self.first_replay.lock()
        {
            first.get_or_insert(item.data.open_time);
        }
        self.inner.on_data(item)
    }

    fn on_modifier(
        &mut self,
        m: DecisionModifier,
    ) -> Result<Option<Detection<DecisionEvent>>, ModifierError> {
        if let (DecisionModifier::Vote(v), Some(t)) = (&m, self.current) {
            self.votes
                .lock()
                .unwrap()
                .entry(v.rule.clone())
                .or_default()
                .push((t, v.ballot, v.weight));
        }
        self.inner.on_modifier(m)
    }
}

fn send<I>(to: ModifierHandle<DecisionModifier>, rule: &str) -> SendVotes<I> {
    SendVotes::new(rule, weights(rule), to)
}

/// Run the agent graph; the votes received by the decision, and the time
/// of the first replay (non-bootstrap) candle.
fn run_graph(candles: Vec<Candle>) -> (Votes, DateTime<Utc>) {
    let votes = Arc::new(Mutex::new(Votes::new()));
    let first_replay = Arc::new(Mutex::new(None));
    let m15 = Timeframe::M15;
    let mut g = Graph::new();
    let sol = g.source(MockSource::replay("SOLUSDT", m15, candles).with_bootstrap(BOOTSTRAP));
    let spy = Spy {
        inner: Decision::new(DecisionParams {
            open_threshold: dec!(3),
            exit_threshold: dec!(2),
        })
        .unwrap(),
        current: None,
        votes: votes.clone(),
        first_replay: first_replay.clone(),
    };
    let decision = g.condition("decision", spy, &sol);
    let to = g.modifier_handle(&decision);

    let rsi = g.transform(Rsi::new(14).unwrap(), &sol);
    let voter = DivergenceVoter::new(div_params(m15), EXIT_HIGH, EXIT_LOW).unwrap();
    let v = g.condition("div-15m", voter, &rsi);
    g.action(send(to, "div-15m"), &v);

    let h1 = g.transform(Aggregate::new(m15, Timeframe::H1).unwrap(), &sol);
    let h1_rsi = g.transform(OnValue::new(Rsi::new(14).unwrap()), &h1);
    let voter = DivergenceVoter::new(div_params(Timeframe::H1), EXIT_HIGH, EXIT_LOW).unwrap();
    let v = g.condition(
        "div-1h",
        Projected::new(voter, aggregated::<Decimal>),
        &h1_rsi,
    );
    g.action(send(to, "div-1h"), &v);

    let emas = g.transform(
        Combine::new((Ema::new(9).unwrap(), Ema::new(21).unwrap())),
        &sol,
    );
    let v = g.condition("ema-9-21", EmaCrossVoter::default(), &emas);
    g.action(send(to, "ema-9-21"), &v);

    let trend = g.transform(Trend::<Candle>::new(50).unwrap(), &sol);
    let v = g.condition("trend-50", TrendVoter::default(), &trend);
    g.action(send(to, "trend-50"), &v);

    let patterns = PatternVoter::new(PatternCondition::new(ThresholdSet::default()).unwrap());
    let v = g.condition("patterns", patterns, &sol);
    g.action(send(to, "patterns"), &v);

    Executor::new(g)
        .with_clock(Clock::Simulated(DateTime::<Utc>::UNIX_EPOCH))
        .run()
        .unwrap();
    let votes = votes.lock().unwrap().clone();
    let first = first_replay.lock().unwrap().unwrap();
    (votes, first)
}

// ------------------------------------------------------ reference (vectors)

/// Wilder RSI(14) of the closes; `None` for the first 14.
fn ref_rsi(closes: &[Decimal]) -> Vec<Option<Decimal>> {
    let n = 14usize;
    let nd = Decimal::from(n as u64);
    let mut out = vec![None; closes.len()];
    if closes.len() <= n {
        return out;
    }
    let up = |i: usize| (closes[i] - closes[i - 1]).max(Decimal::ZERO);
    let down = |i: usize| (closes[i - 1] - closes[i]).max(Decimal::ZERO);
    let mut g: Decimal = (1..=n).map(up).sum::<Decimal>() / nd;
    let mut l: Decimal = (1..=n).map(down).sum::<Decimal>() / nd;
    let value = |g: Decimal, l: Decimal| {
        if g + l == Decimal::ZERO {
            dec!(50)
        } else {
            dec!(100) * g / (g + l)
        }
    };
    out[n] = Some(value(g, l));
    for (i, slot) in out.iter_mut().enumerate().skip(n + 1) {
        g = (g * (nd - Decimal::ONE) + up(i)) / nd;
        l = (l * (nd - Decimal::ONE) + down(i)) / nd;
        *slot = Some(value(g, l));
    }
    out
}

/// EMA(n) seeded with the mean of the first n values; `None` before.
fn ref_ema(values: &[Decimal], n: usize) -> Vec<Option<Decimal>> {
    let k = Decimal::TWO / Decimal::from(n as u64 + 1);
    let mut out = vec![None; values.len()];
    if values.len() < n {
        return out;
    }
    let mut e = values[..n].iter().sum::<Decimal>() / Decimal::from(n as u64);
    out[n - 1] = Some(e);
    for (x, slot) in values.iter().zip(out.iter_mut()).skip(n) {
        e = *x * k + e * (Decimal::ONE - k);
        *slot = Some(e);
    }
    out
}

/// A bar of a series analysed for pivots.
#[derive(Clone, Copy)]
struct Bar {
    time: DateTime<Utc>,
    low: Decimal,
    high: Decimal,
    close: Decimal,
    rsi: Option<Decimal>,
}

#[derive(Clone, Copy)]
struct P {
    time: DateTime<Utc>,
    price: Decimal,
    rsi: Decimal,
}

/// Divergence search on one side (spec F6).
#[derive(Default)]
struct Search {
    a: Option<P>,
    seen: usize,
}

impl Search {
    /// `lows`: bullish side. Returns B if `b` diverges from A.
    fn pivot(&mut self, b: P, lows: bool, p: &DivergenceParams) -> Option<P> {
        let valid = if lows {
            b.rsi <= p.rsi_low
        } else {
            b.rsi >= p.rsi_high
        };
        if let Some(a) = self.a {
            self.seen += 1;
            let diverges = if lows {
                b.price < a.price && b.rsi > a.rsi
            } else {
                b.price > a.price && b.rsi < a.rsi
            };
            if diverges && b.time - a.time >= p.min_gap {
                *self = Search::default();
                return Some(b);
            }
            let more_extreme = if lows { b.rsi < a.rsi } else { b.rsi > a.rsi };
            if more_extreme {
                *self = Search {
                    a: Some(b),
                    seen: 0,
                };
                return None;
            }
            if self.seen < p.lookahead {
                return None;
            }
            *self = Search::default();
        }
        if valid {
            self.a = Some(b);
        }
        None
    }
}

/// Ballots of the divergence voter for each bar of `bars` (index = bar).
fn ref_divergence(bars: &[Bar], p: &DivergenceParams) -> Vec<Vec<Ballot>> {
    let (l, r) = (p.left, p.right);
    let mut out = vec![Vec::new(); bars.len()];
    let (mut lows, mut highs) = (Search::default(), Search::default());
    let mut active: Option<(Side, Decimal)> = None;
    for at in 0..bars.len() {
        // Pivot candidate confirmed at `at`: bar j = at - r, with l bars before.
        let (mut low, mut high) = (None, None);
        if at >= l + r {
            let j = at - r;
            if let Some(rsi) = bars[j].rsi {
                let left = &bars[j - l..j];
                let right = &bars[j + 1..=at];
                let c = bars[j];
                if left.iter().all(|b| b.low > c.low) && right.iter().all(|b| b.low >= c.low) {
                    low = Some(P {
                        time: c.time,
                        price: c.low,
                        rsi,
                    });
                }
                if left.iter().all(|b| b.high < c.high) && right.iter().all(|b| b.high <= c.high) {
                    high = Some(P {
                        time: c.time,
                        price: c.high,
                        rsi,
                    });
                }
            }
        }
        let bull = low.and_then(|b| lows.pivot(b, true, p));
        let bear = high.and_then(|b| highs.pivot(b, false, p));
        let ballots = &mut out[at];
        match (bull, bear) {
            (Some(b), _) => {
                active = Some((Side::Long, b.price));
                ballots.push(Ballot::Open(Side::Long));
            }
            (None, Some(b)) => {
                active = Some((Side::Short, b.price));
                ballots.push(Ballot::Open(Side::Short));
            }
            (None, None) => {
                let close = bars[at].close;
                let broken = match active {
                    Some((Side::Long, level)) => close < level,
                    Some((Side::Short, level)) => close > level,
                    None => false,
                };
                if broken {
                    active = None;
                    ballots.push(Ballot::Revoke);
                }
            }
        }
        if high.is_some_and(|h| h.rsi > EXIT_HIGH) {
            ballots.push(Ballot::Exit(Side::Long));
        }
        if low.is_some_and(|h| h.rsi < EXIT_LOW) {
            ballots.push(Ballot::Exit(Side::Short));
        }
    }
    out
}

/// 1h candles aggregated from complete hours of 4 consecutive 15m candles,
/// with the index of the 15m candle closing the hour (when it is known).
fn ref_hours(candles: &[Candle]) -> Vec<(usize, Candle)> {
    let mut out = Vec::new();
    for (i, c) in candles.iter().enumerate() {
        if c.open_time.timestamp() % 3600 != 45 * 60 || i < 3 {
            continue;
        }
        let part = &candles[i - 3..=i];
        let start = part[0].open_time;
        let complete = start.timestamp() % 3600 == 0
            && part
                .iter()
                .enumerate()
                .all(|(k, c)| c.open_time == start + TimeDelta::minutes(15 * k as i64));
        if complete {
            out.push((
                i,
                Candle {
                    open_time: start,
                    open: part[0].open,
                    high: part.iter().map(|c| c.high).max().unwrap(),
                    low: part.iter().map(|c| c.low).min().unwrap(),
                    close: part[3].close,
                    volume: part.iter().map(|c| c.volume).sum(),
                },
            ));
        }
    }
    out
}

/// Ballots on a crossing of `a` over `b` (touching keeps the side).
fn ref_cross(a: &[Option<Decimal>], b: &[Option<Decimal>]) -> Vec<Vec<Ballot>> {
    let mut side: Option<bool> = None; // true: above
    a.iter()
        .zip(b)
        .map(|(a, b)| {
            let (Some(a), Some(b)) = (a, b) else {
                return Vec::new();
            };
            if a == b {
                return Vec::new();
            }
            let above = a > b;
            let before = side.replace(above);
            match (before, above) {
                (Some(false), true) => vec![Ballot::Open(Side::Long), Ballot::Exit(Side::Short)],
                (Some(true), false) => vec![Ballot::Open(Side::Short), Ballot::Exit(Side::Long)],
                _ => Vec::new(),
            }
        })
        .collect()
}

/// Ballots of the trend voter: on each change of the close's side of its EMA.
fn ref_trend(closes: &[Decimal], ema: &[Option<Decimal>]) -> Vec<Vec<Ballot>> {
    let mut last: Option<Option<bool>> = None; // Some(true): up, Some(None): flat
    closes
        .iter()
        .zip(ema)
        .map(|(c, e)| {
            let Some(e) = e else {
                return Vec::new();
            };
            let now = (c != e).then_some(c > e);
            if last == Some(now) {
                return Vec::new();
            }
            last = Some(now);
            match now {
                Some(true) => vec![Ballot::Open(Side::Long), Ballot::Exit(Side::Short)],
                Some(false) => vec![Ballot::Open(Side::Short), Ballot::Exit(Side::Long)],
                None => vec![Ballot::Revoke],
            }
        })
        .collect()
}

/// Ballots of the pattern voter (shapes detected by `PatternCondition`).
fn ref_patterns(candles: &[Candle]) -> Vec<Vec<Ballot>> {
    let mut shapes = PatternCondition::new(ThresholdSet::default()).unwrap();
    let mut active: Option<(Side, Decimal)> = None;
    candles
        .iter()
        .map(|c| {
            let found = shapes.on_data(Item {
                data: c,
                mode: Mode::Replay,
            });
            if let Some(d) = found {
                let bull = d.value.0.iter().any(|m| m.signal == Signal::Bullish);
                let bear = d.value.0.iter().any(|m| m.signal == Signal::Bearish);
                return match (bull, bear) {
                    (true, false) => {
                        active = Some((Side::Long, c.low));
                        vec![Ballot::Open(Side::Long), Ballot::Exit(Side::Short)]
                    }
                    (false, true) => {
                        active = Some((Side::Short, c.high));
                        vec![Ballot::Open(Side::Short), Ballot::Exit(Side::Long)]
                    }
                    _ => Vec::new(),
                };
            }
            let broken = match active {
                Some((Side::Long, low)) => c.close < low,
                Some((Side::Short, high)) => c.close > high,
                None => false,
            };
            if broken {
                active = None;
                return vec![Ballot::Revoke];
            }
            Vec::new()
        })
        .collect()
}

/// The expected votes of every rule, from the 15m candle `from` on (the
/// earlier ones are bootstrap candles: no action, no vote).
fn reference(candles: &[Candle], from: usize) -> Votes {
    let closes: Vec<Decimal> = candles.iter().map(|c| c.close).collect();
    let rsi = ref_rsi(&closes);
    let bars: Vec<Bar> = candles
        .iter()
        .zip(&rsi)
        .map(|(c, &rsi)| Bar {
            time: c.open_time,
            low: c.low,
            high: c.high,
            close: c.close,
            rsi,
        })
        .collect();

    // Per 15m candle index, the ballots of each rule.
    let mut per_rule: BTreeMap<&str, Vec<Vec<Ballot>>> = BTreeMap::new();
    per_rule.insert(
        "div-15m",
        ref_divergence(&bars, &div_params(Timeframe::M15)),
    );

    // 1h: the hours with an RSI (the voter only sees those), each one at
    // the 15m candle closing it.
    let hours = ref_hours(candles);
    let h_closes: Vec<Decimal> = hours.iter().map(|(_, h)| h.close).collect();
    let h_rsi = ref_rsi(&h_closes);
    let (h_at, h_bars): (Vec<usize>, Vec<Bar>) = hours
        .iter()
        .zip(&h_rsi)
        .filter_map(|((i, h), rsi)| {
            rsi.map(|r| {
                (
                    *i,
                    Bar {
                        time: h.open_time,
                        low: h.low,
                        high: h.high,
                        close: h.close,
                        rsi: Some(r),
                    },
                )
            })
        })
        .unzip();
    let mut h1 = vec![Vec::new(); candles.len()];
    for (k, ballots) in ref_divergence(&h_bars, &div_params(Timeframe::H1))
        .into_iter()
        .enumerate()
    {
        h1[h_at[k]] = ballots;
    }
    per_rule.insert("div-1h", h1);

    per_rule.insert(
        "ema-9-21",
        ref_cross(&ref_ema(&closes, 9), &ref_ema(&closes, 21)),
    );
    per_rule.insert("trend-50", ref_trend(&closes, &ref_ema(&closes, 50)));
    per_rule.insert("patterns", ref_patterns(candles));

    let mut votes = Votes::new();
    for (rule, per_candle) in per_rule {
        let w = weights(rule);
        let list = votes.entry(rule.to_string()).or_default();
        for (i, ballots) in per_candle.into_iter().enumerate().skip(from) {
            for b in ballots {
                let weight = match b {
                    Ballot::Open(_) => w.open,
                    Ballot::Exit(_) => w.exit,
                    Ballot::Revoke => Decimal::ZERO,
                };
                list.push((candles[i].open_time, b, weight));
            }
        }
    }
    votes
}

// ------------------------------------------------------------------ test

#[test]
fn the_decision_receives_exactly_the_votes_of_every_rule() {
    let candles = load();
    assert_eq!(candles.len(), 5856, "61 days of 15m candles");
    let (got, first_replay) = run_graph(candles.clone());
    let from = candles
        .iter()
        .position(|c| c.open_time == first_replay)
        .unwrap();
    assert_eq!(from, BOOTSTRAP, "bootstrap length");
    let want = reference(&candles, from);

    for (rule, want) in &want {
        let empty = Vec::new();
        let got = got.get(rule).unwrap_or(&empty);
        // Every rule must have voted (opens, exits and revokes where possible).
        assert!(!want.is_empty(), "{rule}: no expected vote");
        let opens = want
            .iter()
            .filter(|v| matches!(v.1, Ballot::Open(_)))
            .count();
        let exits = want
            .iter()
            .filter(|v| matches!(v.1, Ballot::Exit(_)))
            .count();
        let revokes = want.iter().filter(|v| v.1 == Ballot::Revoke).count();
        println!(
            "{rule}: {} votes ({opens} open, {exits} exit, {revokes} revoke)",
            want.len()
        );
        if let Some(i) = (0..want.len().max(got.len())).find(|&i| want.get(i) != got.get(i)) {
            panic!(
                "{rule}: first difference at vote {i}: expected {:?}, received {:?}",
                want.get(i),
                got.get(i)
            );
        }
    }
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "no vote from an unknown rule"
    );
}
