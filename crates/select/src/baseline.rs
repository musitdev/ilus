//! Random trader (spec F11): the baseline the agents must beat.
//!
//! On each candle of `[from, to)`: when flat, opens with probability
//! `p_open` on a random side (at the close); when in position, closes with
//! probability `p_close` (at the close). Same account as the agents
//! (leverage, fees, stop-loss, liquidation). Deterministic for its seed.

use agent::Side;
use agent::account::{Account, AccountParams, AccountSummary};
use chrono::{DateTime, Utc};
use trading::candle::Candle;

use crate::rng::Rng;

/// Settings of the random trader.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RandomTrader {
    /// Probability to open, per candle, when flat.
    pub p_open: f64,
    /// Probability to close, per candle, when in position.
    pub p_close: f64,
    /// The account (its stop-loss is used as is).
    pub account: AccountParams,
}

impl RandomTrader {
    /// Trade the candles of `[from, to)` with the random generator `seed`.
    pub fn run(
        &self,
        candles: &[Candle],
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        seed: u64,
    ) -> Result<AccountSummary, String> {
        let mut rng = Rng::new(seed);
        let mut account = Account::new(self.account)?;
        for c in candles
            .iter()
            .filter(|c| c.open_time >= from && c.open_time < to)
        {
            account.candle(c);
            match account.position() {
                None if rng.unit() < self.p_open => {
                    let side = if rng.unit() < 0.5 {
                        Side::Long
                    } else {
                        Side::Short
                    };
                    account.open(side, c);
                }
                Some(_) if rng.unit() < self.p_close => account.exit(c),
                _ => {}
            }
        }
        Ok(account.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::account::EventKind;
    use chrono::TimeDelta;
    use rust_decimal_macros::dec;

    #[test]
    fn trades_at_the_expected_pace_and_is_deterministic() {
        // 30 days of flat 15m candles: no gain, only the pace matters.
        let t0 = DateTime::<Utc>::UNIX_EPOCH;
        let candles: Vec<Candle> = (0..30 * 96)
            .map(|i| Candle {
                open_time: t0 + TimeDelta::minutes(15 * i),
                open: dec!(100),
                high: dec!(100),
                low: dec!(100),
                close: dec!(100),
                volume: dec!(1),
            })
            .collect();
        let trader = RandomTrader {
            p_open: 1.0 / 32.0,
            p_close: 1.0 / 32.0,
            account: AccountParams {
                capital: dec!(1000),
                leverage: dec!(2),
                stop_pct: Some(dec!(3)),
                fee_pct: dec!(0),
            },
        };
        let to = t0 + TimeDelta::days(30);
        let a = trader.run(&candles, t0, to, 5).unwrap();
        let opens = a
            .events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Open(_)))
            .count();
        // Cycle of ~64 candles: ~45 positions in 30 days (1.5 per day).
        assert!((30..=60).contains(&opens), "{opens} positions");
        assert_eq!(a.final_equity, dec!(1000), "flat prices, no fee");
        assert_eq!(a, trader.run(&candles, t0, to, 5).unwrap());
        assert_ne!(a.events, trader.run(&candles, t0, to, 6).unwrap().events);
    }
}
