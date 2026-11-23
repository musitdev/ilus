//! Mock source: delivers predefined items from memory (spec F1, test sources).

use std::time::Duration;

use crate::error::SourceError;
use crate::source::{Source, SourceChannel, SourceData, SourceInfo, Timeframe};

/// Source sending a fixed list of items, optionally spaced in time.
pub struct MockSource<D> {
    channel: SourceChannel<D>,
    items: Vec<D>,
    interval: Option<Duration>,
}

impl<D: SourceData> MockSource<D> {
    /// Create a mock source for `asset` delivering `items` in order.
    pub fn new(asset: impl Into<String>, timeframe: Timeframe, items: Vec<D>) -> Self {
        let info = SourceInfo::Provider {
            provider: "mock".into(),
            asset: asset.into(),
            timeframe,
        };
        Self {
            channel: SourceChannel::new(info),
            items,
            interval: None,
        }
    }

    /// Wait `interval` after each item (simulates a live feed).
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = Some(interval);
        self
    }
}

impl<D: SourceData> Source for MockSource<D> {
    type Data = D;

    fn channel(&mut self) -> &mut SourceChannel<D> {
        &mut self.channel
    }

    async fn run(self) -> Result<(), SourceError> {
        let sender = self.channel.into_sender();
        for item in self.items {
            if !sender.send(item).await {
                break; // no consumer
            }
            match self.interval {
                Some(interval) => tokio::time::sleep(interval).await,
                None => tokio::task::yield_now().await,
            }
        }
        Ok(())
    }
}
