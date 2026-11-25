//! `simulate` (spec F10) on two months of real SOL/USDT 15m candles
//! (tests/data, no network).

use std::str::FromStr;

use agent::Side;
use agent::account::{AccountParams, CloseReason, EventKind};
use agent::genome::{AgentParams, GENOME_LEN};
use agent::simulate::{SimConfig, SimError, history_needed, simulate};
use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use trading::candle::Candle;

const DATA: &str = include_str!("data/solusdt_15m_2026-09-24_2026-11-24.csv");

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

fn config(leverage: Decimal, stop: Option<Decimal>, fee: Decimal) -> SimConfig {
    SimConfig {
        from: Utc.with_ymd_and_hms(2026, 9, 28, 0, 0, 0).unwrap(),
        to: Utc.with_ymd_and_hms(2026, 11, 24, 0, 0, 0).unwrap(),
        account: AccountParams {
            capital: dec!(1000),
            leverage,
            stop_pct: stop,
            fee_pct: fee,
        },
    }
}

/// A genome that trades: moderate weights and thresholds.
fn trading_genome() -> Vec<f64> {
    let mut g = vec![0.5; GENOME_LEN];
    g[14] = 0.2; // open threshold ~2.25
    g[15] = 0.1; // exit threshold ~1.25
    g
}

#[test]
fn every_trade_is_accounted_for() {
    let candles = load();
    let cfg = config(dec!(2), Some(dec!(3)), dec!(0.05));
    let r = simulate(&trading_genome(), &candles, &cfg).unwrap();
    let a = &r.account;
    assert!(a.trades >= 20, "the genome trades: {} events", a.trades);
    assert_eq!(a.trades, a.events.len());

    // Opens and closes alternate, on the same side, inside the period.
    let mut open: Option<Side> = None;
    for e in &a.events {
        assert!(e.time >= cfg.from && e.time < cfg.to, "{e:?}");
        match e.kind {
            EventKind::Open(s) => assert_eq!(open.replace(s), None, "{e:?}"),
            EventKind::Close(s, _) => assert_eq!(open.take(), Some(s), "{e:?}"),
        }
    }
    let stops = a
        .events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Close(_, CloseReason::Stop)))
        .count();
    assert_eq!(stops as u32, a.stops);

    // Recompute the equity from the events alone (2x, fee 0.05 %).
    let (lev, fee) = (dec!(2), dec!(0.0005));
    let mut equity = dec!(1000);
    let mut pos: Option<(Side, Decimal, Decimal, Decimal)> = None; // side, entry, qty, margin
    for e in &a.events {
        match e.kind {
            EventKind::Open(s) => {
                let margin = equity / (Decimal::ONE + lev * fee);
                pos = Some((s, e.price, margin * lev / e.price, margin));
                assert_eq!(e.equity, margin);
            }
            EventKind::Close(_, _) => {
                let (s, entry, qty, margin) = pos.take().unwrap();
                let pnl = match s {
                    Side::Long => qty * (e.price - entry),
                    Side::Short => qty * (entry - e.price),
                };
                equity = (margin + pnl - qty * e.price * fee).max(Decimal::ZERO);
                assert_eq!(e.equity, equity, "{e:?}");
            }
        }
    }
    if pos.is_none() {
        assert_eq!(a.final_equity, equity);
    }
    assert_eq!(
        r.gain_pct(),
        (a.final_equity - dec!(1000)) / dec!(1000) * dec!(100)
    );
    println!(
        "{}\n{} events, gain {}%",
        r.params,
        a.trades,
        r.gain_pct().round_dp(2)
    );
}

#[test]
fn deterministic() {
    let candles = load();
    let cfg = config(dec!(3), Some(dec!(2)), dec!(0.05));
    let a = simulate(&trading_genome(), &candles, &cfg).unwrap();
    let b = simulate(&trading_genome(), &candles, &cfg).unwrap();
    assert_eq!(a, b);
}

#[test]
fn zero_weights_never_trade() {
    let candles = load();
    let mut g = vec![0.5; GENOME_LEN];
    for w in g.iter_mut().take(14) {
        *w = 0.0;
    }
    let r = simulate(&g, &candles, &config(dec!(2), None, dec!(0.05))).unwrap();
    assert_eq!(r.account.trades, 0);
    assert_eq!(r.gain_pct(), dec!(0));
}

#[test]
fn errors_on_short_history_and_bad_genome() {
    let candles = load();
    let g = vec![1.0; GENOME_LEN]; // trend EMA 300: 301 candles needed
    assert_eq!(history_needed(&AgentParams::decode(&g).unwrap()), 301);
    let mut cfg = config(dec!(2), None, dec!(0));
    cfg.from = Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0).unwrap(); // 96 candles before
    assert!(matches!(
        simulate(&g, &candles, &cfg),
        Err(SimError::History {
            needed: 301,
            got: 96
        })
    ));
    assert!(matches!(
        simulate(&[0.5; 3], &candles, &config(dec!(2), None, dec!(0))),
        Err(SimError::Genome(_))
    ));
    let mut cfg = config(dec!(2), None, dec!(0));
    cfg.account.leverage = dec!(0);
    assert!(matches!(
        simulate(&trading_genome(), &candles, &cfg),
        Err(SimError::Invalid(_))
    ));
}
