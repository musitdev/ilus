//! Sources: typed streams of closed data items (spec F1).
//!
//! Each source owns its own [`SourceChannel`] with exactly **one** consumer.
//! A transformer or a rule calls [`Source::get_source_stream`] to get the
//! [`SourceStream`] built from that channel. It can be taken only once.
//!
//! Channels are **bounded**: when the consumer stops reading (backpressure),
//! the channel fills up and [`SourceSender::send`] waits, slowing the source.

pub mod binance;
pub mod mock;

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::SourceError;

/// Default number of items buffered in a source channel.
pub const DEFAULT_CAPACITY: usize = 256;

/// One closed, timestamped element of a source stream.
pub trait SourceData: fmt::Debug + Send + Sync + 'static {
    /// Time of the item (for candles: open time).
    fn time(&self) -> DateTime<Utc>;
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

/// A producer of data items.
pub trait Source: Send + 'static {
    /// Type of data item produced.
    type Data: SourceData;

    /// The channel of this source.
    fn channel(&mut self) -> &mut SourceChannel<Self::Data>;

    /// Take the stream of this source. Only one consumer per source:
    /// a second call returns [`SourceError::StreamTaken`].
    fn get_source_stream(&mut self) -> Result<SourceStream<Self::Data>, SourceError> {
        self.channel().take_stream()
    }

    /// Produce items until the feed ends or fails. Only closed items must be
    /// sent. When it returns, the sender is dropped and the stream ends.
    fn run(self) -> impl Future<Output = Result<(), SourceError>> + Send;
}

/// The channel owned by a source: one sender, one receiver, bounded.
pub struct SourceChannel<D> {
    info: SourceInfo,
    tx: mpsc::Sender<D>,
    rx: Option<mpsc::Receiver<D>>,
}

impl<D: SourceData> SourceChannel<D> {
    /// Create the channel of a source, buffering [`DEFAULT_CAPACITY`] items.
    pub fn new(info: SourceInfo) -> Self {
        Self::with_capacity(info, DEFAULT_CAPACITY)
    }

    /// Create the channel of a source, buffering `capacity` items (at least 1).
    pub fn with_capacity(info: SourceInfo, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        Self {
            info,
            tx,
            rx: Some(rx),
        }
    }

    /// Description of the source.
    pub fn info(&self) -> &SourceInfo {
        &self.info
    }

    /// Take the receiving end as a stream (once).
    pub fn take_stream(&mut self) -> Result<SourceStream<D>, SourceError> {
        let rx = self
            .rx
            .take()
            .ok_or_else(|| SourceError::StreamTaken(self.info.to_string()))?;
        Ok(SourceStream {
            info: self.info.clone(),
            inner: ReceiverStream::new(rx),
        })
    }

    /// Turn the channel into its sending end, for use in [`Source::run`].
    /// If the stream was never taken, it is dropped here so nothing piles up.
    pub fn into_sender(self) -> SourceSender<D> {
        SourceSender {
            info: self.info,
            tx: self.tx,
        }
    }
}

/// Sending end of a source channel.
pub struct SourceSender<D> {
    info: SourceInfo,
    tx: mpsc::Sender<D>,
}

impl<D> SourceSender<D> {
    /// Description of the source.
    pub fn info(&self) -> &SourceInfo {
        &self.info
    }

    /// Send a closed item to the consumer. Waits while the channel is full
    /// (backpressure). Returns `false` if there is no consumer (anymore):
    /// the source may stop.
    pub async fn send(&self, item: D) -> bool {
        self.tx.send(item).await.is_ok()
    }
}

/// Stream of a source's items, built from the source channel.
pub struct SourceStream<D> {
    info: SourceInfo,
    inner: ReceiverStream<D>,
}

impl<D> SourceStream<D> {
    /// Description of the source this stream comes from.
    pub fn info(&self) -> &SourceInfo {
        &self.info
    }
}

impl<D> Stream for SourceStream<D> {
    type Item = D;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<D>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}
