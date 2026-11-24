//! Candle: the OHLCV data item delivered by provider sources.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::source::SourceData;

/// One closed OHLCV bar for an asset over a fixed timeframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candle {
    /// Start of the candle period.
    pub open_time: DateTime<Utc>,
    /// Open price.
    pub open: Decimal,
    /// Highest price.
    pub high: Decimal,
    /// Lowest price.
    pub low: Decimal,
    /// Close price.
    pub close: Decimal,
    /// Traded volume.
    pub volume: Decimal,
}

impl SourceData for Candle {
    fn time(&self) -> DateTime<Utc> {
        self.open_time
    }
}

/// Items that contain a candle: a candle, or the output of transformers
/// built on candles (e.g. `Derived<Candle, rsi>`, `Derived<Derived<Candle, rsi>, trend>`).
pub trait CandleData: SourceData {
    /// The candle of the item.
    fn candle(&self) -> &Candle;
}

impl CandleData for Candle {
    fn candle(&self) -> &Candle {
        self
    }
}

impl<D: CandleData, V: std::fmt::Debug + Clone + Send + Sync + 'static> CandleData
    for crate::transformer::Derived<D, V>
{
    fn candle(&self) -> &Candle {
        self.data.candle()
    }
}
