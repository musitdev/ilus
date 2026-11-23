//! Merge source: combines a driving source and a slower one (spec F1).
//!
//! Built from two sources **before** the graph; the graph sees one source.
//! It emits on each item of the driving source, with the last item of the
//! slower source **closed at or before** the driving item's close (no
//! look-ahead). Driving items are accumulated until the slower source has
//! delivered the expected item, so pairing is correct even when the sources
//! are not time-aligned (bootstrap, replay, background fetch delays).

use std::collections::VecDeque;

use chrono::{DateTime, TimeDelta, Utc};

use crate::source::{Mode, Source, SourceData, SourceInfo, SourcePoll, Timeframe};

/// Item of a merged source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged<A, B> {
    /// Item of the driving source.
    pub main: A,
    /// Last item of the slower source closed at or before `main` closed
    /// (repeated until a newer one is available); `None` before the first.
    pub other: Option<B>,
}

impl<A: SourceData, B: SourceData> SourceData for Merged<A, B> {
    fn time(&self) -> DateTime<Utc> {
        self.main.time()
    }
}

/// Calls an inner source when it is due (as the executor would).
struct Inner<S: Source> {
    source: S,
    timeframe: Option<Timeframe>,
    next_due: Option<DateTime<Utc>>,
    ended: bool,
}

impl<S: Source> Inner<S> {
    fn new(source: S) -> Self {
        Self {
            timeframe: source.info().timeframe(),
            source,
            next_due: None,
            ended: false,
        }
    }

    fn poll(&mut self, now: DateTime<Utc>) -> Option<(S::Data, Mode)> {
        if self.ended {
            return None;
        }
        let due = self.source.mode() != Mode::Normal || self.next_due.is_none_or(|t| t <= now);
        if !due {
            return None;
        }
        let result = match self.source.poll(now) {
            SourcePoll::Ready(data, mode) => Some((data, mode)),
            SourcePoll::Pending => None,
            SourcePoll::Ended => {
                self.ended = true;
                None
            }
        };
        self.next_due = Some(self.source.next_call(now));
        result
    }

    /// Close time of an item of this source.
    fn close(&self, item: &S::Data) -> DateTime<Utc> {
        item.time()
            + self
                .timeframe
                .map_or(TimeDelta::zero(), Timeframe::duration)
    }

    fn is_fast(&self) -> bool {
        !self.ended && self.source.mode() != Mode::Normal
    }
}

/// `ceil(n * from / to)`: how many items of duration `to` cover `n` items of
/// duration `from` (durations in seconds).
fn ceil_div(n: usize, from: i64, to: i64) -> usize {
    let total = i64::try_from(n)
        .unwrap_or(i64::MAX / 2)
        .saturating_mul(from);
    usize::try_from((total + to - 1) / to.max(1)).unwrap_or(n)
}

/// Start of the period of `timeframe` containing `t` (aligned on the epoch).
fn floor(t: DateTime<Utc>, timeframe: Timeframe) -> DateTime<Utc> {
    let period = timeframe.duration().num_seconds().max(1);
    let secs = t.timestamp();
    DateTime::from_timestamp(secs - secs.rem_euclid(period), 0).unwrap_or(t)
}

/// A source made of a driving source `main` (e.g. 15m) and a slower source
/// `other` (e.g. 1h).
pub struct MergeSource<A: Source, B: Source> {
    main: Inner<A>,
    other: Inner<B>,
    /// Driving items waiting for the slower source.
    queue: VecDeque<(A::Data, Mode)>,
    /// Slower items received, not yet needed.
    pending_other: VecDeque<B::Data>,
    /// Last slower item closed at or before the last emitted driving item.
    current: Option<B::Data>,
}

impl<A: Source, B: Source> MergeSource<A, B> {
    /// Merge `main` (driving) and `other` (slower).
    pub fn new(main: A, other: B) -> Self {
        Self {
            main: Inner::new(main),
            other: Inner::new(other),
            queue: VecDeque::new(),
            pending_other: VecDeque::new(),
            current: None,
        }
    }

    /// The front driving item can be emitted: the slower source has
    /// delivered the expected item (or moved past it, or ended).
    fn ready(&mut self) -> bool {
        let Some((front, _)) = self.queue.front() else {
            return false;
        };
        let close = self.main.close(front);
        // Advance to the last slower item closed at or before `close`.
        while let Some(next) = self.pending_other.front() {
            if self.other.close(next) > close {
                break;
            }
            self.current = self.pending_other.pop_front();
        }
        if self.other.ended || !self.pending_other.is_empty() {
            // Ended, or a later slower item exists: nothing better will come.
            return true;
        }
        match (self.other.timeframe, &self.current) {
            (Some(tf), Some(current)) => {
                let expected_open = floor(close, tf) - tf.duration();
                current.time() >= expected_open
            }
            _ => false,
        }
    }
}

impl<A: Source, B: Source> Source for MergeSource<A, B> {
    type Data = Merged<A::Data, B::Data>;

    fn info(&self) -> SourceInfo {
        SourceInfo::Merged {
            main: Box::new(self.main.source.info()),
            other: Box::new(self.other.source.info()),
        }
    }

    /// `items` (computed by the graph, in driving items) and the bootstraps
    /// set by the user on each inner source (e.g. 15 1h candles for an
    /// RSI(14) on the slower side) are reconciled so that **every** slower
    /// bootstrap item is paired with at least one driving item:
    /// - the slower source covers at least the duration of `items`;
    /// - the driving source covers the slower bootstrap except its first
    ///   item, which is paired with the first driving items (e.g. 15 x 1h:
    ///   14 h = 56 x 15m).
    fn set_bootstrap(&mut self, items: usize) {
        let (Some(m), Some(o)) = (self.main.timeframe, self.other.timeframe) else {
            self.main.source.set_bootstrap(items);
            self.other.source.set_bootstrap(items);
            return;
        };
        let (m, o) = (
            m.duration().num_seconds().max(1),
            o.duration().num_seconds().max(1),
        );
        let main_items = items.max(self.main.source.bootstrap());
        // Slower items covering the driving bootstrap, plus the one covering its start.
        let other_items = ceil_div(main_items, m, o) + 1;
        self.other.source.set_bootstrap(other_items);
        // Driving items covering the slower bootstrap (its first item closes
        // before the first driving item and is paired with it).
        let other_span = self.other.source.bootstrap().saturating_sub(1);
        let main_items = main_items.max(ceil_div(other_span, o, m));
        self.main.source.set_bootstrap(main_items);
    }

    fn bootstrap(&self) -> usize {
        self.main.source.bootstrap()
    }

    fn mode(&self) -> Mode {
        // Fast ticks while one of the inner sources is in bootstrap or replay.
        if self.main.is_fast() {
            self.main.source.mode()
        } else if self.other.is_fast() {
            self.other.source.mode()
        } else {
            Mode::Normal
        }
    }

    fn poll(&mut self, now: DateTime<Utc>) -> SourcePoll<Self::Data> {
        if let Some(item) = self.main.poll(now) {
            self.queue.push_back(item);
        }
        if let Some((item, _)) = self.other.poll(now) {
            self.pending_other.push_back(item);
        }
        if self.ready()
            && let Some((main, mode)) = self.queue.pop_front()
        {
            return SourcePoll::Ready(
                Merged {
                    main,
                    other: self.current.clone(),
                },
                mode,
            );
        }
        if self.main.ended && self.queue.is_empty() {
            SourcePoll::Ended
        } else {
            SourcePoll::Pending
        }
    }

    fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        if !self.queue.is_empty() {
            return now;
        }
        [&self.main.next_due, &self.other.next_due]
            .into_iter()
            .zip([self.main.ended, self.other.ended])
            .filter(|(_, ended)| !ended)
            .filter_map(|(due, _)| *due)
            .min()
            .unwrap_or(now)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::source::mock::MockSource;
    use crate::transformer::IndicatorPoint;
    use rust_decimal::Decimal;

    fn at(min: i64) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(min)
    }

    /// Points at the given minutes, with the minute as value.
    fn points(minutes: &[i64]) -> Vec<IndicatorPoint> {
        minutes
            .iter()
            .map(|&m| IndicatorPoint {
                time: at(m),
                value: Decimal::from(m),
            })
            .collect()
    }

    fn m15(n: i64) -> MockSource<IndicatorPoint> {
        let minutes: Vec<i64> = (0..n).map(|i| i * 15).collect();
        MockSource::replay("M", Timeframe::M15, points(&minutes))
    }

    fn h1(n: i64) -> MockSource<IndicatorPoint> {
        let minutes: Vec<i64> = (0..n).map(|i| i * 60).collect();
        MockSource::replay("H", Timeframe::H1, points(&minutes))
    }

    /// Delays an inner source: returns `Pending` for the first `wait` calls.
    struct Lagging<S> {
        inner: S,
        wait: usize,
    }

    impl<S: Source> Source for Lagging<S> {
        type Data = S::Data;
        fn info(&self) -> SourceInfo {
            self.inner.info()
        }
        fn set_bootstrap(&mut self, items: usize) {
            self.inner.set_bootstrap(items);
        }
        fn bootstrap(&self) -> usize {
            self.inner.bootstrap()
        }
        fn mode(&self) -> Mode {
            self.inner.mode()
        }
        fn poll(&mut self, now: DateTime<Utc>) -> SourcePoll<S::Data> {
            if self.wait > 0 {
                self.wait -= 1;
                return SourcePoll::Pending;
            }
            self.inner.poll(now)
        }
        fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc> {
            self.inner.next_call(now)
        }
    }

    /// Records the bootstrap length given by the merge.
    struct Probe(MockSource<IndicatorPoint>, Arc<Mutex<usize>>);

    impl Source for Probe {
        type Data = IndicatorPoint;
        fn info(&self) -> SourceInfo {
            self.0.info()
        }
        fn set_bootstrap(&mut self, items: usize) {
            self.0.set_bootstrap(items);
            *self.1.lock().unwrap() = self.0.bootstrap();
        }
        fn bootstrap(&self) -> usize {
            self.0.bootstrap()
        }
        fn mode(&self) -> Mode {
            self.0.mode()
        }
        fn poll(&mut self, now: DateTime<Utc>) -> SourcePoll<IndicatorPoint> {
            self.0.poll(now)
        }
        fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc> {
            self.0.next_call(now)
        }
    }

    /// Poll until the end; returns (main minute, other minute).
    fn drain<A, B>(mut merge: MergeSource<A, B>) -> Vec<(i64, Option<i64>)>
    where
        A: Source<Data = IndicatorPoint>,
        B: Source<Data = IndicatorPoint>,
    {
        let mut out = Vec::new();
        for _ in 0..1000 {
            match merge.poll(at(0)) {
                SourcePoll::Ready(m, _) => out.push((
                    m.main.time.timestamp() / 60,
                    m.other.map(|o| o.time.timestamp() / 60),
                )),
                SourcePoll::Pending => {}
                SourcePoll::Ended => return out,
            }
        }
        panic!("merge did not end");
    }

    #[test]
    fn pairs_by_time_without_look_ahead() {
        // 15m items 00:00..02:45, 1h items 00:00, 01:00, 02:00.
        let out = drain(MergeSource::new(m15(12), h1(3)));
        let expected: Vec<(i64, Option<i64>)> = vec![
            (0, None),       // closes 00:15: no 1h candle closed yet
            (15, None),      // 00:30
            (30, None),      // 00:45
            (45, Some(0)),   // closes 01:00: 1h 00:00-01:00 is closed
            (60, Some(0)),   // 01:15: still 00:00 (01:00-02:00 not closed)
            (75, Some(0)),   //
            (90, Some(0)),   //
            (105, Some(60)), // closes 02:00
            (120, Some(60)),
            (135, Some(60)),
            (150, Some(60)),
            (165, Some(120)), // closes 03:00
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn accumulates_until_the_slower_source_catches_up() {
        // The 1h source delivers nothing for its first 20 calls.
        let lagging = Lagging {
            inner: h1(3),
            wait: 20,
        };
        let out = drain(MergeSource::new(m15(12), lagging));
        // Same pairing as without lag: nothing is paired with an old 1h value.
        assert_eq!(drain(MergeSource::new(m15(12), h1(3))), out);
    }

    #[test]
    fn slower_source_ended_uses_the_last_item() {
        // Only one 1h item: later 15m items keep it.
        let out = drain(MergeSource::new(m15(8), h1(1)));
        assert_eq!(out.last(), Some(&(105, Some(0))));
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn bootstrap_is_forwarded_in_each_timeframe() {
        let main = Arc::new(Mutex::new(0));
        let other = Arc::new(Mutex::new(0));
        let mut merge =
            MergeSource::new(Probe(m15(10), main.clone()), Probe(h1(10), other.clone()));
        merge.set_bootstrap(8); // 8 x 15m = 2 h -> 2 x 1h, + 1
        assert_eq!(*main.lock().unwrap(), 8);
        assert_eq!(*other.lock().unwrap(), 3);
    }

    #[test]
    fn driving_bootstrap_covers_the_slower_bootstrap_set_by_the_user() {
        // The user asks 15 x 1h (e.g. RSI(14) on 1h): 14 h = 56 x 15m.
        let main = Arc::new(Mutex::new(0));
        let other = Arc::new(Mutex::new(0));
        let mut merge = MergeSource::new(
            Probe(m15(100), main.clone()),
            Probe(h1(30).with_bootstrap(15), other.clone()),
        );
        merge.set_bootstrap(4);
        assert_eq!(*other.lock().unwrap(), 15);
        assert_eq!(*main.lock().unwrap(), 56);
    }

    #[test]
    fn every_slower_bootstrap_item_is_paired() {
        // 1h bootstrap of 5 set by the user; the merge extends the 15m
        // bootstrap to 4 h = 16 x 15m. As with a real provider, both
        // bootstraps end at the same time: 1h 00:00..05:00, 15m 01:00..05:00.
        let minutes: Vec<i64> = (0..40).map(|i| 60 + i * 15).collect();
        let main = MockSource::replay("M", Timeframe::M15, points(&minutes));
        let mut merge = MergeSource::new(main, h1(10).with_bootstrap(5));
        merge.set_bootstrap(1);
        let mut seen = std::collections::BTreeSet::new();
        let mut bootstrap_items = 0;
        for _ in 0..1000 {
            match merge.poll(at(0)) {
                SourcePoll::Ready(m, mode) => {
                    if mode == Mode::Bootstrap {
                        bootstrap_items += 1;
                        if let Some(o) = m.other {
                            seen.insert(o.time);
                        }
                    }
                }
                SourcePoll::Pending => {}
                SourcePoll::Ended => break,
            }
        }
        assert_eq!(bootstrap_items, 16, "4 x 1h = 16 x 15m of bootstrap");
        // All 5 bootstrap 1h candles are paired (the 5th, 04:00-05:00, closes
        // with the last bootstrap 15m candle, 04:45-05:00).
        assert_eq!(seen.len(), 5, "1h candles paired in bootstrap: {seen:?}");
    }

    #[test]
    fn info_and_time_follow_the_driving_source() {
        let merge = MergeSource::new(m15(1), h1(1));
        assert_eq!(merge.info().to_string(), "merge<mock:M:15m, mock:H:1h>");
        assert_eq!(merge.info().timeframe(), Some(Timeframe::M15));
    }
}
