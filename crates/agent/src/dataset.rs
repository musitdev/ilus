//! Training data (spec F12): the candles of a period labelled with the
//! trades of an agent.
//!
//! Each candle gets the actions taken at its close (`buy`: open long,
//! `sell`: open short, `close`, with the reason) and the position after it
//! (`long`, `short`, `flat`). Several actions on one candle (e.g. a
//! stop-loss, then a new open) are kept in order.

use std::fmt::Write as _;

use crate::Side;
use crate::account::{CloseReason, EventKind, TradeEvent};
use trading::candle::Candle;

/// A candle with its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledCandle {
    /// The candle.
    pub candle: Candle,
    /// Actions at this candle, in order.
    pub actions: Vec<EventKind>,
    /// Position after the candle.
    pub position: Option<Side>,
}

/// Label the candles with `events` (sorted, from one simulation). Candles
/// before the first event are flat.
pub fn label(candles: &[Candle], events: &[TradeEvent]) -> Vec<LabeledCandle> {
    let mut position = None;
    let mut next = events.iter().peekable();
    candles
        .iter()
        .map(|c| {
            let mut actions = Vec::new();
            while let Some(e) = next.next_if(|e| e.time <= c.open_time) {
                if e.time == c.open_time {
                    actions.push(e.kind);
                }
                position = match e.kind {
                    EventKind::Open(s) => Some(s),
                    EventKind::Close(..) => None,
                };
            }
            LabeledCandle {
                candle: c.clone(),
                actions,
                position,
            }
        })
        .collect()
}

fn action_text(k: &EventKind) -> String {
    match k {
        EventKind::Open(Side::Long) => "buy".into(),
        EventKind::Open(Side::Short) => "sell".into(),
        EventKind::Close(_, reason) => format!(
            "close_{}",
            match reason {
                CloseReason::Exit => "exit",
                CloseReason::Stop => "stop",
                CloseReason::Liquidation => "liquidation",
            }
        ),
    }
}

/// Labelled candles as CSV: `open_time_ms,open,high,low,close,volume,action,position`
/// (`action`: empty, or the actions joined with `+`).
pub fn to_csv(rows: &[LabeledCandle]) -> String {
    let mut s = String::from("open_time_ms,open,high,low,close,volume,action,position\n");
    for r in rows {
        let c = &r.candle;
        let actions: Vec<String> = r.actions.iter().map(action_text).collect();
        let _ = writeln!(
            s,
            "{},{},{},{},{},{},{},{}",
            c.open_time.timestamp_millis(),
            c.open,
            c.high,
            c.low,
            c.close,
            c.volume,
            actions.join("+"),
            match r.position {
                Some(Side::Long) => "long",
                Some(Side::Short) => "short",
                None => "flat",
            }
        );
    }
    s
}

/// Trade events as CSV: `time_ms,action,side,price,equity`.
pub fn events_csv(events: &[TradeEvent]) -> String {
    let mut s = String::from("time_ms,action,side,price,equity\n");
    for e in events {
        let side = match e.kind {
            EventKind::Open(s) | EventKind::Close(s, _) => s,
        };
        let _ = writeln!(
            s,
            "{},{},{},{},{}",
            e.time.timestamp_millis(),
            action_text(&e.kind),
            match side {
                Side::Long => "long",
                Side::Short => "short",
            },
            e.price,
            e.equity.round_dp(4)
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeDelta, Utc};
    use rust_decimal_macros::dec;

    fn t(i: i64) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(15 * i)
    }

    fn candle(i: i64) -> Candle {
        Candle {
            open_time: t(i),
            open: dec!(1),
            high: dec!(2),
            low: dec!(0.5),
            close: dec!(1.5),
            volume: dec!(10),
        }
    }

    fn ev(i: i64, kind: EventKind) -> TradeEvent {
        TradeEvent {
            time: t(i),
            kind,
            price: dec!(1.5),
            equity: dec!(100),
        }
    }

    #[test]
    fn candles_get_their_actions_and_the_position_after_them() {
        let candles: Vec<Candle> = (0..6).map(candle).collect();
        let events = [
            ev(1, EventKind::Open(Side::Long)),
            ev(3, EventKind::Close(Side::Long, CloseReason::Stop)),
            ev(3, EventKind::Open(Side::Short)),
            ev(4, EventKind::Close(Side::Short, CloseReason::Exit)),
        ];
        let rows = label(&candles, &events);
        let summary: Vec<(usize, Option<Side>)> =
            rows.iter().map(|r| (r.actions.len(), r.position)).collect();
        assert_eq!(
            summary,
            vec![
                (0, None),
                (1, Some(Side::Long)),
                (0, Some(Side::Long)),
                (2, Some(Side::Short)),
                (1, None),
                (0, None),
            ]
        );
        let csv = to_csv(&rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 7);
        assert!(lines[2].ends_with(",buy,long"));
        assert!(lines[4].ends_with(",close_stop+sell,short"));
        assert!(lines[5].ends_with(",close_exit,flat"));
        assert!(lines[6].ends_with(",,flat"));
        let ev = events_csv(&events);
        assert_eq!(ev.lines().count(), 5);
        assert!(ev.lines().nth(2).unwrap().contains(",close_stop,long,"));
    }
}
