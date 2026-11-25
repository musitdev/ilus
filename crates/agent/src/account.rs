//! Simulated futures account (spec F10): one position at a time, the whole
//! equity as margin, leverage, fees on the position size, optional
//! stop-loss, liquidation. Funding is ignored.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::Side;
use trading::candle::Candle;

/// Maintenance margin rate (share of the notional).
const MAINTENANCE: Decimal = dec!(0.005);

/// Settings of the account (not genes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountParams {
    /// Starting equity, in quote currency (e.g. USDT); > 0.
    pub capital: Decimal,
    /// Leverage; >= 1.
    pub leverage: Decimal,
    /// Stop-loss in % of the entry price against the position (e.g. 3);
    /// `None`: no stop.
    pub stop_pct: Option<Decimal>,
    /// Fee in % of the position size, per open and per close (e.g. 0.05).
    pub fee_pct: Decimal,
}

/// Why a position was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The agent's exit decision.
    Exit,
    /// The stop-loss price was reached.
    Stop,
    /// The margin was lost.
    Liquidation,
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A position was opened (buy for long, sell for short).
    Open(Side),
    /// The position on this side was closed.
    Close(Side, CloseReason),
}

/// One trade event (training data for the network, later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeEvent {
    /// Open time of the candle on which it happened.
    pub time: DateTime<Utc>,
    /// Open or close.
    pub kind: EventKind,
    /// Fill price.
    pub price: Decimal,
    /// Equity after the event (margin of the position after an open).
    pub equity: Decimal,
}

#[derive(Debug, Clone, Copy)]
struct Position {
    side: Side,
    entry: Decimal,
    qty: Decimal,
    margin: Decimal,
}

impl Position {
    fn pnl(&self, price: Decimal) -> Decimal {
        match self.side {
            Side::Long => self.qty * (price - self.entry),
            Side::Short => self.qty * (self.entry - price),
        }
    }
}

/// The simulated account.
#[derive(Debug, Clone)]
pub struct Account {
    leverage: Decimal,
    stop: Option<Decimal>,
    fee: Decimal,
    capital: Decimal,
    /// Equity when flat.
    equity: Decimal,
    position: Option<Position>,
    last_price: Option<Decimal>,
    min_equity: Decimal,
    fees_paid: Decimal,
    stops: u32,
    liquidations: u32,
    events: Vec<TradeEvent>,
}

/// Summary of an account after a simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// Equity at the end, the open position marked at the last close (no
    /// closing fee).
    pub final_equity: Decimal,
    /// Gain in % of the capital.
    pub gain_pct: Decimal,
    /// Lowest equity seen (worst price inside candles).
    pub min_equity: Decimal,
    /// Number of opens and closes.
    pub trades: usize,
    /// Positions closed by the stop-loss.
    pub stops: u32,
    /// Positions liquidated.
    pub liquidations: u32,
    /// Fees paid.
    pub fees_paid: Decimal,
    /// Every open and close, in order.
    pub events: Vec<TradeEvent>,
}

impl Account {
    /// A flat account. Rejects capital <= 0, leverage < 1, a negative fee
    /// and a stop <= 0.
    pub fn new(p: AccountParams) -> Result<Self, String> {
        if p.capital <= Decimal::ZERO {
            return Err("capital must be > 0".into());
        }
        if p.leverage < Decimal::ONE {
            return Err("leverage must be >= 1".into());
        }
        if p.fee_pct < Decimal::ZERO {
            return Err("fee must be >= 0".into());
        }
        if p.stop_pct.is_some_and(|s| s <= Decimal::ZERO) {
            return Err("stop-loss must be > 0".into());
        }
        Ok(Self {
            leverage: p.leverage,
            stop: p.stop_pct.map(|s| s / Decimal::ONE_HUNDRED),
            fee: p.fee_pct / Decimal::ONE_HUNDRED,
            capital: p.capital,
            equity: p.capital,
            position: None,
            last_price: None,
            min_equity: p.capital,
            fees_paid: Decimal::ZERO,
            stops: 0,
            liquidations: 0,
            events: Vec::new(),
        })
    }

    /// Side of the open position.
    pub fn position(&self) -> Option<Side> {
        self.position.map(|p| p.side)
    }

    /// A new candle: the stop-loss, then liquidation (worst price of the
    /// candle). Returns `true` if they closed the position.
    pub fn candle(&mut self, c: &Candle) -> bool {
        self.last_price = Some(c.close);
        let Some(p) = self.position else {
            return false;
        };
        if let Some(stop) = self.stop {
            let fill = match p.side {
                Side::Long => {
                    let level = p.entry * (Decimal::ONE - stop);
                    (c.low <= level).then(|| level.min(c.open))
                }
                Side::Short => {
                    let level = p.entry * (Decimal::ONE + stop);
                    (c.high >= level).then(|| level.max(c.open))
                }
            };
            if let Some(price) = fill {
                self.close(c.open_time, price, CloseReason::Stop);
                return true;
            }
        }
        let worst = match p.side {
            Side::Long => c.low,
            Side::Short => c.high,
        };
        let equity = p.margin + p.pnl(worst);
        if equity <= MAINTENANCE * p.qty * worst {
            self.position = None;
            self.equity = Decimal::ZERO;
            self.min_equity = Decimal::ZERO;
            self.liquidations += 1;
            self.events.push(TradeEvent {
                time: c.open_time,
                kind: EventKind::Close(p.side, CloseReason::Liquidation),
                price: worst,
                equity: Decimal::ZERO,
            });
            return true;
        }
        self.min_equity = self.min_equity.min(equity);
        false
    }

    /// Open `side` at the close of `c` with the whole equity (if flat and
    /// with equity). Returns `true` if a position was opened.
    pub fn open(&mut self, side: Side, c: &Candle) -> bool {
        if self.position.is_some() || self.equity <= Decimal::ZERO || c.close <= Decimal::ZERO {
            return false;
        }
        // equity = margin + fee, fee = margin * leverage * fee rate.
        let margin = self.equity / (Decimal::ONE + self.leverage * self.fee);
        self.fees_paid += self.equity - margin;
        self.position = Some(Position {
            side,
            entry: c.close,
            qty: margin * self.leverage / c.close,
            margin,
        });
        self.events.push(TradeEvent {
            time: c.open_time,
            kind: EventKind::Open(side),
            price: c.close,
            equity: margin,
        });
        true
    }

    /// Close the position (if any) at the close of `c`.
    pub fn exit(&mut self, c: &Candle) {
        self.close(c.open_time, c.close, CloseReason::Exit);
    }

    fn close(&mut self, time: DateTime<Utc>, price: Decimal, reason: CloseReason) {
        let Some(p) = self.position.take() else {
            return;
        };
        let fee = p.qty * price * self.fee;
        self.fees_paid += fee;
        self.equity = (p.margin + p.pnl(price) - fee).max(Decimal::ZERO);
        self.min_equity = self.min_equity.min(self.equity);
        if reason == CloseReason::Stop {
            self.stops += 1;
        }
        self.events.push(TradeEvent {
            time,
            kind: EventKind::Close(p.side, reason),
            price,
            equity: self.equity,
        });
    }

    /// Equity now: the open position marked at the last close.
    pub fn mark(&self) -> Decimal {
        match (self.position, self.last_price) {
            (Some(p), Some(price)) => p.margin + p.pnl(price),
            _ => self.equity,
        }
    }

    /// Summary of the account.
    pub fn summary(&self) -> AccountSummary {
        let final_equity = self.mark();
        AccountSummary {
            final_equity,
            gain_pct: (final_equity - self.capital) / self.capital * Decimal::ONE_HUNDRED,
            min_equity: self.min_equity,
            trades: self.events.len(),
            stops: self.stops,
            liquidations: self.liquidations,
            fees_paid: self.fees_paid,
            events: self.events.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn c(i: i64, open: Decimal, high: Decimal, low: Decimal, close: Decimal) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(15 * i),
            open,
            high,
            low,
            close,
            volume: dec!(1),
        }
    }

    fn flat(i: i64, price: Decimal) -> Candle {
        c(i, price, price, price, price)
    }

    fn account(leverage: Decimal, stop: Option<Decimal>, fee: Decimal) -> Account {
        Account::new(AccountParams {
            capital: dec!(1000),
            leverage,
            stop_pct: stop,
            fee_pct: fee,
        })
        .unwrap()
    }

    #[test]
    fn long_then_exit_with_fees() {
        // 2x, fee 0.1 %: margin = 1000 / 1.002; size = 2 * margin / 100.
        let mut a = account(dec!(2), None, dec!(0.1));
        assert!(a.open(Side::Long, &flat(0, dec!(100))));
        assert!(!a.open(Side::Short, &flat(0, dec!(100))), "one position");
        let margin = dec!(1000) / dec!(1.002);
        let qty = margin * dec!(2) / dec!(100);
        a.candle(&flat(1, dec!(110)));
        assert_eq!(a.mark(), margin + qty * dec!(10));
        a.exit(&flat(1, dec!(110)));
        let s = a.summary();
        let close_fee = qty * dec!(110) * dec!(0.001);
        assert_eq!(s.final_equity, margin + qty * dec!(10) - close_fee);
        assert_eq!(s.fees_paid, dec!(1000) - margin + close_fee);
        assert_eq!(s.trades, 2);
        assert_eq!(
            s.events.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![
                EventKind::Open(Side::Long),
                EventKind::Close(Side::Long, CloseReason::Exit)
            ]
        );
        assert_eq!(s.gain_pct, (s.final_equity - dec!(1000)) / dec!(10));
    }

    #[test]
    fn short_gains_when_the_price_falls() {
        let mut a = account(dec!(1), None, dec!(0));
        a.open(Side::Short, &flat(0, dec!(100)));
        a.candle(&flat(1, dec!(90)));
        a.exit(&flat(1, dec!(90)));
        assert_eq!(a.summary().final_equity, dec!(1100));
        assert_eq!(a.summary().gain_pct, dec!(10));
    }

    #[test]
    fn stop_loss_fills_at_the_level_or_the_open_gap() {
        // Long at 100, stop 3 %: level 97.
        let mut a = account(dec!(1), Some(dec!(3)), dec!(0));
        a.open(Side::Long, &flat(0, dec!(100)));
        assert!(!a.candle(&c(1, dec!(99), dec!(100), dec!(97.5), dec!(98))));
        assert!(a.candle(&c(2, dec!(98), dec!(98), dec!(96), dec!(96.5))));
        assert_eq!(a.summary().final_equity, dec!(970));
        assert_eq!(a.summary().stops, 1);
        // Short at 100, the candle opens above the level (gap): fill at the open.
        let mut a = account(dec!(1), Some(dec!(3)), dec!(0));
        a.open(Side::Short, &flat(0, dec!(100)));
        assert!(a.candle(&c(1, dec!(105), dec!(106), dec!(104), dec!(105))));
        assert_eq!(a.summary().final_equity, dec!(950));
    }

    #[test]
    fn liquidation_loses_the_margin_and_stops_trading() {
        // 10x long at 100: margin 1000, size 100; liquidated near 90.5.
        let mut a = account(dec!(10), None, dec!(0));
        a.open(Side::Long, &flat(0, dec!(100)));
        assert!(!a.candle(&c(1, dec!(100), dec!(100), dec!(91), dec!(95))));
        assert!(a.candle(&c(2, dec!(95), dec!(95), dec!(90), dec!(91))));
        let s = a.summary();
        assert_eq!(
            (s.final_equity, s.min_equity, s.liquidations),
            (dec!(0), dec!(0), 1)
        );
        assert_eq!(s.gain_pct, dec!(-100));
        assert!(!a.open(Side::Long, &flat(3, dec!(91))), "no equity left");
    }

    #[test]
    fn invalid_settings_are_rejected() {
        let p = AccountParams {
            capital: dec!(1000),
            leverage: dec!(2),
            stop_pct: None,
            fee_pct: dec!(0),
        };
        assert!(
            Account::new(AccountParams {
                capital: dec!(0),
                ..p
            })
            .is_err()
        );
        assert!(
            Account::new(AccountParams {
                leverage: dec!(0.5),
                ..p
            })
            .is_err()
        );
        assert!(
            Account::new(AccountParams {
                fee_pct: dec!(-1),
                ..p
            })
            .is_err()
        );
        assert!(
            Account::new(AccountParams {
                stop_pct: Some(dec!(0)),
                ..p
            })
            .is_err()
        );
    }
}
