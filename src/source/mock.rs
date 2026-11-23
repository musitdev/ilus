//! Mock source: items from memory, for tests and examples (spec F1).
//!
//! - [`MockSource::replay`]: the first items are *bootstrap* (as many as the
//!   graph needs, or [`with_bootstrap`](MockSource::with_bootstrap)), the rest
//!   *replay*, one per tick; then the source ends.
//! - [`MockSource::live`]: after the bootstrap items, each item is delivered
//!   in *normal* mode once it is closed (`time + timeframe <= now`), on the
//!   aligned schedule of [`next_aligned_call`]; the source ends after the last.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};

use crate::source::{
    CLOSE_DELAY, Mode, Source, SourceData, SourceInfo, SourcePoll, Timeframe, next_aligned_call,
};

/// Source delivering a fixed list of items.
pub struct MockSource<D> {
    info: SourceInfo,
    timeframe: Timeframe,
    items: VecDeque<D>,
    bootstrap: usize,
    delivered: usize,
    live: bool,
    last_time: Option<DateTime<Utc>>,
}

impl<D: SourceData> MockSource<D> {
    fn new(asset: &str, timeframe: Timeframe, items: Vec<D>, live: bool) -> Self {
        Self {
            info: SourceInfo::Provider {
                provider: "mock".into(),
                asset: asset.into(),
                timeframe,
            },
            timeframe,
            items: items.into(),
            bootstrap: 0,
            delivered: 0,
            live,
            last_time: None,
        }
    }

    /// Bootstrap items, then replay items, one per global tick.
    pub fn replay(asset: &str, timeframe: Timeframe, items: Vec<D>) -> Self {
        Self::new(asset, timeframe, items, false)
    }

    /// Bootstrap items, then normal (live) items, delivered once closed.
    pub fn live(asset: &str, timeframe: Timeframe, items: Vec<D>) -> Self {
        Self::new(asset, timeframe, items, true)
    }

    /// At least `n` bootstrap items (the graph may ask for more).
    pub fn with_bootstrap(mut self, n: usize) -> Self {
        self.bootstrap = self.bootstrap.max(n);
        self
    }

    fn closed(&self, item: &D, now: DateTime<Utc>) -> bool {
        item.time() + self.timeframe.duration() <= now
    }
}

impl<D: SourceData> Source for MockSource<D> {
    type Data = D;

    fn info(&self) -> SourceInfo {
        self.info.clone()
    }

    fn set_bootstrap(&mut self, items: usize) {
        self.bootstrap = self.bootstrap.max(items);
    }

    fn mode(&self) -> Mode {
        if self.delivered < self.bootstrap && !self.items.is_empty() {
            Mode::Bootstrap
        } else if self.live {
            Mode::Normal
        } else {
            Mode::Replay
        }
    }

    fn poll(&mut self, now: DateTime<Utc>) -> SourcePoll<D> {
        let mode = self.mode();
        let Some(next) = self.items.front() else {
            return SourcePoll::Ended;
        };
        if mode == Mode::Normal && !self.closed(next, now) {
            return SourcePoll::Pending;
        }
        let Some(item) = self.items.pop_front() else {
            return SourcePoll::Ended;
        };
        self.delivered += 1;
        self.last_time = Some(item.time());
        SourcePoll::Ready(item, mode)
    }

    fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self.items.front() {
            Some(next) if self.closed(next, now) => now,
            // First live item: just after its close.
            Some(next) if self.last_time.is_none() => {
                next.time() + self.timeframe.duration() + CLOSE_DELAY
            }
            Some(_) => next_aligned_call(self.last_time, self.timeframe, now, CLOSE_DELAY),
            None => now,
        }
    }
}
