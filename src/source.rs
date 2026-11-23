//! Sources: produce closed data items, one per call at most (spec §5, F1).
//!
//! The [`Executor`](crate::executor::Executor) calls each source at its ticks:
//! - in **bootstrap** and **replay** modes, at every global tick (10 ms by default);
//! - in **normal** (live) mode, when the source asks to be called
//!   ([`Source::next_call`]): just after the expected candle close, then every
//!   half period until the candle is available ([`next_aligned_call`]).
//!
//! A call never blocks: network work runs in the background inside the source.
//! Every item carries the running [`Mode`] of the source.

pub mod binance;
pub mod mock;

use std::fmt;

use chrono::{DateTime, TimeDelta, Utc};

/// Running mode of a source, carried by every data item (spec §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// History before the start, to warm up transformers and conditions.
    /// Detections of bootstrap items are not sent to actions.
    Bootstrap,
    /// History over a chosen period (backtests). Actions are run.
    Replay,
    /// Live data. Actions are run.
    Normal,
}

/// One closed, timestamped data item.
///
/// `Clone` is required because a transformer output forwards (owns) its input.
pub trait SourceData: fmt::Debug + Clone + Send + Sync + 'static {
    /// Time of the item (for candles: open time).
    fn time(&self) -> DateTime<Utc>;
}

/// A data item with its running mode, as given to transformers and conditions.
#[derive(Debug, Clone, Copy)]
pub struct Item<'a, D> {
    /// The data.
    pub data: &'a D,
    /// Running mode of the source that produced it.
    pub mode: Mode,
}

/// Duration of one candle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timeframe {
    /// 1 minute.
    M1,
    /// 5 minutes.
    M5,
    /// 15 minutes.
    M15,
    /// 1 hour.
    H1,
    /// 4 hours.
    H4,
    /// 1 day.
    D1,
}

impl Timeframe {
    /// Every timeframe, shortest first.
    pub const ALL: [Self; 6] = [Self::M1, Self::M5, Self::M15, Self::H1, Self::H4, Self::D1];

    /// Duration of one candle.
    pub fn duration(self) -> TimeDelta {
        match self {
            Self::M1 => TimeDelta::minutes(1),
            Self::M5 => TimeDelta::minutes(5),
            Self::M15 => TimeDelta::minutes(15),
            Self::H1 => TimeDelta::hours(1),
            Self::H4 => TimeDelta::hours(4),
            Self::D1 => TimeDelta::days(1),
        }
    }

    /// Parse `1m`, `5m`, `15m`, `1h`, `4h`, `1d`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tf| tf.to_string() == s)
    }
}

/// Formats as `1m`, `5m`, `15m`, `1h`, `4h`, `1d` (also the Binance interval names).
impl fmt::Display for Timeframe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::H1 => "1h",
            Self::H4 => "4h",
            Self::D1 => "1d",
        };
        f.write_str(s)
    }
}

/// Description of where data comes from (used in logs and trigger events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceInfo {
    /// Data fetched from a provider.
    Provider {
        /// Provider name (e.g. `binance`).
        provider: String,
        /// Asset in the provider's symbol format (e.g. `BTCUSDT`).
        asset: String,
        /// Candle precision.
        timeframe: Timeframe,
    },
    /// Data produced by a transformer from another source.
    Transformed {
        /// The input source.
        input: Box<SourceInfo>,
        /// Transformer description (e.g. `rsi(14)`).
        transformer: String,
    },
}

impl fmt::Display for SourceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider {
                provider,
                asset,
                timeframe,
            } => write!(f, "{provider}:{asset}:{timeframe}"),
            Self::Transformed { input, transformer } => write!(f, "{transformer}<{input}>"),
        }
    }
}

/// Result of a source call.
#[derive(Debug, Clone, PartialEq)]
pub enum SourcePoll<D> {
    /// A new closed item, with its running mode.
    Ready(D, Mode),
    /// No new item yet.
    Pending,
    /// The source has ended (e.g. end of a replay, or a fatal error).
    Ended,
}

/// A producer of closed data items, called by the executor.
pub trait Source: Send + 'static {
    /// Type of data item produced.
    type Data: SourceData;

    /// Description of this source.
    fn info(&self) -> SourceInfo;

    /// Number of bootstrap items needed by the graph (computed from the
    /// warm-ups, spec §5.4). Called once, before the first call. A source may
    /// use more.
    fn set_bootstrap(&mut self, items: usize);

    /// Current running mode: bootstrap and replay sources are called at every
    /// global tick; normal sources when [`next_call`](Self::next_call) is due.
    fn mode(&self) -> Mode;

    /// Next item if one is ready. Must never block.
    fn poll(&mut self, now: DateTime<Utc>) -> SourcePoll<Self::Data>;

    /// Normal mode: when the executor must call this source next.
    fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc>;
}

/// Default delay after a candle close before calling a normal-mode source.
pub const CLOSE_DELAY: TimeDelta = TimeDelta::seconds(2);

/// Normal-mode schedule (spec §5.4): call just after the close of the next
/// expected candle (`close + delay`); if that time is past and the candle was
/// not available, call again every half period.
///
/// `last_open` is the open time of the last candle delivered (`None`: call now).
pub fn next_aligned_call(
    last_open: Option<DateTime<Utc>>,
    timeframe: Timeframe,
    now: DateTime<Utc>,
    delay: TimeDelta,
) -> DateTime<Utc> {
    let Some(last) = last_open else {
        return now;
    };
    let tf = timeframe.duration();
    // The next candle opens at `last + tf` and closes at `last + 2 tf`.
    let at = last + tf * 2 + delay;
    if now < at { at } else { now + tf / 2 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(min: i64, sec: i64) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(min) + TimeDelta::seconds(sec)
    }

    #[test]
    fn aligned_call_after_the_expected_close() {
        let d = CLOSE_DELAY;
        // Last candle opened at 00:00 (1m): the next one closes at 00:02.
        assert_eq!(
            next_aligned_call(Some(t(0, 0)), Timeframe::M1, t(1, 10), d),
            t(2, 2)
        );
        // Past the expected time and still missing: every half period (30 s).
        assert_eq!(
            next_aligned_call(Some(t(0, 0)), Timeframe::M1, t(2, 2), d),
            t(2, 32)
        );
        // 15m: the next candle closes 30 min after the last open.
        assert_eq!(
            next_aligned_call(Some(t(0, 0)), Timeframe::M15, t(16, 0), d),
            t(30, 2)
        );
        assert_eq!(
            next_aligned_call(Some(t(0, 0)), Timeframe::M15, t(31, 0), d),
            t(38, 30)
        );
        // Nothing delivered yet: call now.
        assert_eq!(next_aligned_call(None, Timeframe::M1, t(5, 0), d), t(5, 0));
    }

    #[test]
    fn timeframe_round_trip() {
        for tf in Timeframe::ALL {
            assert_eq!(Timeframe::parse(&tf.to_string()), Some(tf));
        }
        assert_eq!(Timeframe::parse("2m"), None);
    }
}
