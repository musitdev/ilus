//! Binance public market-data source, REST only (spec F1).
//!
//! - **Bootstrap**: the last closed candles before the start (the replay start,
//!   or now), as many as the graph needs.
//! - **Replay** (`replay(from, to)`): every closed candle of the period, then
//!   the source ends.
//! - **Normal** (live): one request per call, at the aligned schedule of
//!   [`next_aligned_call`]; candles missed since the last one are included.
//!
//! History is fetched in the background by pages of 1000 candles; each call
//! takes one candle from the buffer. A call never blocks.
//!
//! Failures, at two levels:
//! - each REST request is retried up to [`MAX_RETRIES`] times (backoff 1 s →
//!   60 s, `Retry-After` honoured);
//! - every background task is supervised: its handle is checked at each call
//!   (without blocking). If it fails (error or panic), the source reconnects
//!   up to [`MAX_RECONNECTS`] times, waiting half its period between attempts
//!   (30 s for 1m), then ends (at once for an unknown symbol). History
//!   resumes after the last candle delivered (nothing is delivered twice).
//!   Running tasks are cancelled when the source is dropped.

use std::collections::VecDeque;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::candle::Candle;
use crate::error::SourceError;
use crate::source::{
    CLOSE_DELAY, Mode, Source, SourceInfo, SourcePoll, Timeframe, next_aligned_call,
};

/// Default REST endpoint.
pub const REST_URL: &str = "https://api.binance.com";
/// Fallback REST endpoint (market data only).
pub const REST_URL_FALLBACK: &str = "https://data-api.binance.vision";

/// Max retries of a failing REST request (inside a task).
pub const MAX_RETRIES: u32 = 5;
/// Max reconnections after a failed task (half the period between attempts).
pub const MAX_RECONNECTS: u32 = 5;
/// Max candles per REST request (Binance limit).
const PAGE_LIMIT: usize = 1000;
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Delay before calling again while a live request is in flight.
const IN_FLIGHT_RETRY: TimeDelta = TimeDelta::milliseconds(500);

/// Binance spot candle source.
pub struct BinanceSource {
    info: SourceInfo,
    asset: String,
    timeframe: Timeframe,
    rest_url: String,
    bootstrap: usize,
    replay: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// Open time of the last candle delivered.
    last_open: Option<DateTime<Utc>>,
    /// HTTP client, created at the first call.
    fetcher: Option<Fetcher>,
    /// Retries of each REST request.
    request_retries: u32,
    /// Consecutive task failures (reset on success).
    failures: u32,
    state: State,
}

/// A background task, supervised by the source: its result is read at each
/// call without blocking, and it is cancelled on drop.
struct Task<T> {
    handle: JoinHandle<Result<T, SourceError>>,
}

impl<T: Send + 'static> Task<T> {
    fn spawn(work: impl Future<Output = Result<T, SourceError>> + Send + 'static) -> Self {
        Self {
            handle: tokio::spawn(work),
        }
    }

    /// The task result if it has finished (an error if it panicked or was
    /// cancelled); `None` while it runs. Never blocks.
    fn try_result(&mut self) -> Option<Result<T, SourceError>> {
        if !self.handle.is_finished() {
            return None;
        }
        let mut cx = Context::from_waker(Waker::noop());
        match Pin::new(&mut self.handle).poll(&mut cx) {
            Poll::Ready(Ok(result)) => Some(result),
            Poll::Ready(Err(e)) => Some(Err(SourceError::Provider(format!("task failed: {e}")))),
            Poll::Pending => None,
        }
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// What to restart after a reconnection delay.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    History,
    Live,
}

enum State {
    Start,
    History {
        rx: mpsc::UnboundedReceiver<(Candle, Mode)>,
        task: Task<()>,
        /// Result of the task, once finished (the buffer may still hold candles).
        finished: Option<Result<(), SourceError>>,
    },
    Live {
        queue: VecDeque<Candle>,
        request: Option<Task<Vec<Candle>>>,
    },
    /// Waiting before a reconnection.
    Waiting {
        until: DateTime<Utc>,
        resume: Phase,
    },
    Ended,
}

impl BinanceSource {
    /// Live source for `asset` (Binance symbol, e.g. `SOLUSDT`): bootstrap,
    /// then normal mode. See [`replay`](Self::replay) for a backtest.
    pub fn new(asset: impl Into<String>, timeframe: Timeframe) -> Self {
        let asset = asset.into().to_uppercase();
        Self {
            info: SourceInfo::Provider {
                provider: "binance".into(),
                asset: asset.clone(),
                timeframe,
            },
            asset,
            timeframe,
            rest_url: REST_URL.into(),
            bootstrap: 0,
            replay: None,
            last_open: None,
            fetcher: None,
            request_retries: MAX_RETRIES,
            failures: 0,
            state: State::Start,
        }
    }

    /// Replay every closed candle from `from` to `to` (after the bootstrap,
    /// taken before `from`), then end.
    pub fn replay(mut self, from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        self.replay = Some((from, to));
        self
    }

    /// At least `n` bootstrap candles (the graph may ask for more).
    pub fn with_bootstrap(mut self, n: usize) -> Self {
        self.bootstrap = self.bootstrap.max(n);
        self
    }

    /// Retries of each REST request inside a task (default [`MAX_RETRIES`]).
    pub fn with_request_retries(mut self, retries: u32) -> Self {
        self.request_retries = retries;
        self
    }

    /// Use another REST endpoint (e.g. [`REST_URL_FALLBACK`]).
    pub fn with_endpoint(mut self, rest_url: impl Into<String>) -> Self {
        self.rest_url = rest_url.into();
        self
    }

    fn make_fetcher(&self) -> Result<Fetcher, SourceError> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| SourceError::Provider(e.to_string()))?;
        Ok(Fetcher {
            client,
            rest_url: self.rest_url.clone(),
            asset: self.asset.clone(),
            timeframe: self.timeframe,
            name: self.info.to_string(),
            retries: self.request_retries,
        })
    }

    /// Start (or restart) the history task.
    fn start_history(&mut self) {
        let Some(fetcher) = self.fetcher.clone() else {
            self.state = State::Ended;
            return;
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let task = Task::spawn(history(fetcher, self.bootstrap, self.replay, tx));
        self.state = State::History {
            rx,
            task,
            finished: None,
        };
    }

    fn start_live(&mut self) {
        self.state = State::Live {
            queue: VecDeque::new(),
            request: None,
        };
    }

    /// A task failed: wait half a period and reconnect, or end after
    /// [`MAX_RECONNECTS`] attempts.
    fn reconnect(&mut self, now: DateTime<Utc>, e: &SourceError, resume: Phase) {
        if matches!(e, SourceError::Unknown(_)) {
            // Configuration error (e.g. unknown symbol): reconnecting cannot help.
            eprintln!("[{}] {e}", self.info);
            self.state = State::Ended;
            return;
        }
        self.failures += 1;
        if self.failures > MAX_RECONNECTS {
            eprintln!(
                "[{}] {e}; giving up after {MAX_RECONNECTS} reconnections",
                self.info
            );
            self.state = State::Ended;
            return;
        }
        let wait = self.timeframe.duration() / 2;
        eprintln!(
            "[{}] {e}; reconnection {}/{MAX_RECONNECTS} in {}s",
            self.info,
            self.failures,
            wait.num_seconds()
        );
        self.state = State::Waiting {
            until: now + wait,
            resume,
        };
    }

    fn fail(&mut self, e: &SourceError) -> SourcePoll<Candle> {
        eprintln!("[{}] {e}", self.info);
        self.state = State::Ended;
        SourcePoll::Ended
    }
}

impl Source for BinanceSource {
    type Data = Candle;

    fn info(&self) -> SourceInfo {
        self.info.clone()
    }

    fn set_bootstrap(&mut self, items: usize) {
        self.bootstrap = self.bootstrap.max(items);
    }

    fn bootstrap(&self) -> usize {
        self.bootstrap
    }

    fn mode(&self) -> Mode {
        let history = match &self.state {
            State::Start => self.bootstrap > 0 || self.replay.is_some(),
            State::History { .. } => true,
            State::Waiting { resume, .. } => *resume == Phase::History,
            State::Live { .. } | State::Ended => false,
        };
        match (history, self.replay.is_some()) {
            (true, true) => Mode::Replay,
            (true, false) => Mode::Bootstrap,
            (false, _) => Mode::Normal,
        }
    }

    fn poll(&mut self, now: DateTime<Utc>) -> SourcePoll<Candle> {
        match &mut self.state {
            State::Start => {
                match self.make_fetcher() {
                    Ok(f) => self.fetcher = Some(f),
                    Err(e) => return self.fail(&e),
                }
                if self.bootstrap == 0 && self.replay.is_none() {
                    self.start_live();
                } else {
                    self.start_history();
                }
                SourcePoll::Pending
            }
            State::History { rx, task, finished } => {
                // Buffered candles first (skipping those already delivered
                // before a reconnection).
                while let Ok((candle, mode)) = rx.try_recv() {
                    if self.last_open.is_some_and(|l| candle.open_time <= l) {
                        continue;
                    }
                    self.last_open = Some(candle.open_time);
                    return SourcePoll::Ready(candle, mode);
                }
                if finished.is_none() {
                    *finished = task.try_result();
                    if finished.is_some() {
                        // Candles sent just before the end may be buffered.
                        return self.poll(now);
                    }
                    return SourcePoll::Pending;
                }
                match finished.take() {
                    Some(Ok(())) if self.replay.is_some() => {
                        self.state = State::Ended;
                        SourcePoll::Ended
                    }
                    Some(Ok(())) => {
                        self.failures = 0;
                        self.start_live();
                        SourcePoll::Pending
                    }
                    Some(Err(e)) => {
                        self.reconnect(now, &e, Phase::History);
                        SourcePoll::Pending
                    }
                    None => SourcePoll::Pending,
                }
            }
            State::Live { queue, request } => {
                if let Some(task) = request
                    && let Some(result) = task.try_result()
                {
                    *request = None;
                    match result {
                        Ok(candles) => {
                            queue.extend(new_candles(candles, self.last_open));
                            self.failures = 0;
                        }
                        Err(e) => {
                            // Missed candles come with the next request.
                            self.reconnect(now, &e, Phase::Live);
                            return SourcePoll::Pending;
                        }
                    }
                }
                if let Some(candle) = queue.pop_front() {
                    self.last_open = Some(candle.open_time);
                    return SourcePoll::Ready(candle, Mode::Normal);
                }
                if request.is_none()
                    && let Some(fetcher) = self.fetcher.clone()
                {
                    let last_open = self.last_open;
                    *request = Some(Task::spawn(
                        async move { latest(&fetcher, last_open).await },
                    ));
                }
                SourcePoll::Pending
            }
            State::Waiting { until, resume } => {
                if now < *until {
                    return SourcePoll::Pending;
                }
                match *resume {
                    Phase::History => self.start_history(),
                    Phase::Live => self.start_live(),
                }
                self.poll(now)
            }
            State::Ended => SourcePoll::Ended,
        }
    }

    fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        match &self.state {
            State::Waiting { until, .. } => *until,
            State::Live { queue, .. } if !queue.is_empty() => now,
            State::Live {
                request: Some(_), ..
            } => now + IN_FLIGHT_RETRY,
            _ => next_aligned_call(self.last_open, self.timeframe, now, CLOSE_DELAY),
        }
    }
}

// ------------------------------------------------------- background work

/// Bootstrap (candles before the start), then the replay period. Stops
/// early (successfully) if the source is gone.
async fn history(
    f: Fetcher,
    bootstrap: usize,
    replay: Option<(DateTime<Utc>, DateTime<Utc>)>,
    tx: mpsc::UnboundedSender<(Candle, Mode)>,
) -> Result<(), SourceError> {
    let now = Utc::now();
    let start = replay.map_or(now, |(from, _)| from);
    if bootstrap > 0 {
        let mut candles = Vec::new();
        let from = bootstrap_from(start, f.timeframe, bootstrap);
        f.range(from, start, now, |c| {
            candles.push(c);
            true
        })
        .await?;
        let skip = candles.len().saturating_sub(bootstrap);
        for c in candles.into_iter().skip(skip) {
            if tx.send((c, Mode::Bootstrap)).is_err() {
                return Ok(());
            }
        }
    }
    if let Some((from, to)) = replay {
        f.range(from, to.min(now), now, |c| {
            tx.send((c, Mode::Replay)).is_ok()
        })
        .await?;
    }
    Ok(())
}

/// Live request: the closed candles since the last one, or the most recent
/// closed candle if none was delivered yet.
async fn latest(f: &Fetcher, last_open: Option<DateTime<Utc>>) -> Result<Vec<Candle>, SourceError> {
    let now = Utc::now();
    let mut candles = Vec::new();
    match last_open {
        Some(last) => {
            f.range(last + f.timeframe.duration(), now, now, |c| {
                candles.push(c);
                true
            })
            .await?;
        }
        None => {
            let rows = f.fetch_klines(None, 2).await?;
            candles.extend(
                rows.into_iter()
                    .rev()
                    .find(|k| k.close_time < now)
                    .map(|k| k.candle),
            );
        }
    }
    Ok(candles)
}

/// Start of the bootstrap window: one candle more than needed, so that
/// exactly `n` closed candles are found when `start` is inside a candle.
fn bootstrap_from(start: DateTime<Utc>, timeframe: Timeframe, n: usize) -> DateTime<Utc> {
    let n = i32::try_from(n).unwrap_or(i32::MAX - 1);
    start - timeframe.duration() * (n + 1)
}

/// Candles newer than `last_open`, in order, without duplicates.
fn new_candles(candles: Vec<Candle>, last_open: Option<DateTime<Utc>>) -> Vec<Candle> {
    let mut last = last_open;
    let mut out = Vec::new();
    for c in candles {
        if last.is_none_or(|l| c.open_time > l) {
            last = Some(c.open_time);
            out.push(c);
        }
    }
    out
}

#[derive(Clone)]
struct Fetcher {
    client: reqwest::Client,
    rest_url: String,
    asset: String,
    timeframe: Timeframe,
    name: String,
    retries: u32,
}

impl Fetcher {
    fn log(&self, msg: &str) {
        eprintln!("[{}] {msg}", self.name);
    }

    /// Closed candles (closed before `now`) opening in `[from, until)`,
    /// page by page, given to `each` (which returns `false` to stop).
    async fn range(
        &self,
        mut from: DateTime<Utc>,
        until: DateTime<Utc>,
        now: DateTime<Utc>,
        mut each: impl FnMut(Candle) -> bool + Send,
    ) -> Result<(), SourceError> {
        loop {
            let rows = self.fetch_klines(Some(from), PAGE_LIMIT).await?;
            let full = rows.len() == PAGE_LIMIT;
            let Some(last) = rows.last().map(|k| k.candle.open_time) else {
                return Ok(());
            };
            for k in rows {
                if k.candle.open_time >= until || k.close_time >= now {
                    return Ok(());
                }
                if !each(k.candle) {
                    return Ok(());
                }
            }
            if !full {
                return Ok(());
            }
            from = last + self.timeframe.duration();
        }
    }

    /// REST klines request, retried with backoff on transient errors.
    async fn fetch_klines(
        &self,
        start: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<Kline>, SourceError> {
        let mut backoff = BACKOFF_START;
        let mut attempt = 0;
        loop {
            match self.fetch_klines_once(start, limit).await {
                Ok(rows) => return Ok(rows),
                Err(Fetch::Fatal(e)) => return Err(e),
                Err(Fetch::Retry { reason, wait }) => {
                    attempt += 1;
                    if attempt > self.retries {
                        return Err(SourceError::Provider(format!(
                            "giving up after {} retries: {reason}",
                            self.retries
                        )));
                    }
                    let wait = wait.unwrap_or(backoff);
                    self.log(&format!(
                        "{reason}; retry {attempt}/{} in {}s",
                        self.retries,
                        wait.as_secs()
                    ));
                    tokio::time::sleep(wait).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }

    async fn fetch_klines_once(
        &self,
        start: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<Kline>, Fetch> {
        let mut query = vec![
            ("symbol", self.asset.clone()),
            ("interval", self.timeframe.to_string()),
            ("limit", limit.to_string()),
        ];
        if let Some(start) = start {
            query.push(("startTime", start.timestamp_millis().to_string()));
        }
        let response = self
            .client
            .get(format!("{}/api/v3/klines", self.rest_url))
            .query(&query)
            .send()
            .await
            .map_err(|e| Fetch::retry(format!("request failed: {e}")))?;

        let status = response.status();
        if status.as_u16() == 429 || status.as_u16() == 418 {
            // Rate limited: wait as asked by the server.
            let wait = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .map(Duration::from_secs);
            return Err(Fetch::Retry {
                reason: format!("rate limited ({status})"),
                wait,
            });
        }
        let body = response
            .text()
            .await
            .map_err(|e| Fetch::retry(format!("read failed: {e}")))?;
        if status.as_u16() == 400 {
            return Err(Fetch::Fatal(SourceError::Unknown(format!(
                "{}:{} ({body})",
                self.asset, self.timeframe
            ))));
        }
        if !status.is_success() {
            return Err(Fetch::retry(format!("HTTP {status}: {body}")));
        }
        parse_rest_klines(&body).map_err(Fetch::Fatal)
    }
}

enum Fetch {
    Retry {
        reason: String,
        wait: Option<Duration>,
    },
    Fatal(SourceError),
}

impl Fetch {
    fn retry(reason: String) -> Self {
        Self::Retry { reason, wait: None }
    }
}

// ---------------------------------------------------------------- parsing

/// A candle with its close time (to tell whether it is closed).
#[derive(Debug, PartialEq)]
struct Kline {
    candle: Candle,
    close_time: DateTime<Utc>,
}

fn bad(msg: impl Into<String>) -> SourceError {
    SourceError::Provider(format!("invalid Binance data: {}", msg.into()))
}

fn millis(ms: i64) -> Result<DateTime<Utc>, SourceError> {
    DateTime::from_timestamp_millis(ms).ok_or_else(|| bad(format!("timestamp {ms}")))
}

fn decimal(s: &str) -> Result<Decimal, SourceError> {
    Decimal::from_str(s).map_err(|e| bad(format!("number '{s}': {e}")))
}

/// Parse a REST klines response:
/// `[[openTime, "open", "high", "low", "close", "volume", closeTime, ...], ...]`.
fn parse_rest_klines(body: &str) -> Result<Vec<Kline>, SourceError> {
    let rows: Vec<Vec<Value>> = serde_json::from_str(body).map_err(|e| bad(e.to_string()))?;
    rows.iter().map(|row| parse_rest_row(row)).collect()
}

fn parse_rest_row(row: &[Value]) -> Result<Kline, SourceError> {
    let int = |i: usize| {
        row.get(i)
            .and_then(Value::as_i64)
            .ok_or_else(|| bad(format!("field {i} is not an integer")))
    };
    let num = |i: usize| {
        row.get(i)
            .and_then(Value::as_str)
            .ok_or_else(|| bad(format!("field {i} is not a string")))
            .and_then(decimal)
    };
    Ok(Kline {
        candle: Candle {
            open_time: millis(int(0)?)?,
            open: num(1)?,
            high: num(2)?,
            low: num(3)?,
            close: num(4)?,
            volume: num(5)?,
        },
        close_time: millis(int(6)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    // Hand-written sample following the documented Binance format.
    const REST: &str = r#"[
        [1700000000000,"150.10","151.00","149.50","150.80","1234.5",1700000059999,"0",10,"0","0","0"],
        [1700000060000,"150.80","152.25","150.70","152.00","987.25",1700000119999,"0",12,"0","0","0"]
    ]"#;

    fn candle(min: i64) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(min),
            open: dec!(1),
            high: dec!(1),
            low: dec!(1),
            close: dec!(1),
            volume: dec!(1),
        }
    }

    #[test]
    fn parses_rest_klines_as_decimals() {
        let rows = parse_rest_klines(REST).unwrap();
        assert_eq!(rows.len(), 2);
        let k = &rows[0];
        assert_eq!(k.candle.open_time.timestamp_millis(), 1_700_000_000_000);
        assert_eq!(k.close_time.timestamp_millis(), 1_700_000_059_999);
        assert_eq!(k.candle.open, dec!(150.10));
        assert_eq!(k.candle.high, dec!(151.00));
        assert_eq!(k.candle.low, dec!(149.50));
        assert_eq!(k.candle.close, dec!(150.80));
        assert_eq!(k.candle.volume, dec!(1234.5));
        assert_eq!(rows[1].candle.close, dec!(152.00));
    }

    #[test]
    fn rejects_malformed_rest_rows() {
        assert!(parse_rest_klines(r#"[[1700000000000,"x","1","1","1","1",1]]"#).is_err());
        assert!(parse_rest_klines(r#"[[1700000000000,"1"]]"#).is_err());
        assert!(parse_rest_klines(r#"{"code":-1121,"msg":"Invalid symbol."}"#).is_err());
    }

    #[test]
    fn new_candles_skips_duplicates_and_old_ones() {
        let got = new_candles(
            vec![candle(1), candle(2), candle(2), candle(3)],
            Some(candle(1).open_time),
        );
        let times: Vec<_> = got.iter().map(|c| c.open_time).collect();
        assert_eq!(times, [candle(2).open_time, candle(3).open_time]);
        assert_eq!(new_candles(vec![candle(5)], None).len(), 1);
    }

    #[test]
    fn bootstrap_window_has_one_spare_candle() {
        let start = DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(100);
        assert_eq!(
            bootstrap_from(start, Timeframe::M15, 22),
            start - TimeDelta::minutes(15 * 23)
        );
    }

    #[test]
    fn modes_follow_the_configuration() {
        let live = BinanceSource::new("solusdt", Timeframe::M1);
        assert_eq!(live.info().to_string(), "binance:SOLUSDT:1m");
        assert_eq!(live.mode(), Mode::Normal);
        assert_eq!(live.with_bootstrap(10).mode(), Mode::Bootstrap);
        let t = DateTime::<Utc>::UNIX_EPOCH;
        let replay = BinanceSource::new("SOLUSDT", Timeframe::M15).replay(t, t);
        assert_eq!(replay.mode(), Mode::Replay);
    }

    /// Drive a source pointed to a closed local port (no network needed);
    /// returns the reconnection waits (seconds) and whether it ended.
    fn drive(mut src: BinanceSource) -> (Vec<i64>, bool) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let mut now = DateTime::<Utc>::UNIX_EPOCH;
        let mut waits = Vec::new();
        for _ in 0..5000 {
            if src.poll(now) == SourcePoll::Ended {
                return (waits, true);
            }
            match &src.state {
                State::Waiting { until, .. } => {
                    waits.push((*until - now).num_seconds());
                    now = *until; // jump to the reconnection
                }
                _ => {
                    now += TimeDelta::milliseconds(10);
                    // Let the background task run (connection refused at once).
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        (waits, false)
    }

    fn unreachable(timeframe: Timeframe) -> BinanceSource {
        BinanceSource::new("SOLUSDT", timeframe)
            .with_endpoint("http://127.0.0.1:9")
            .with_request_retries(0)
    }

    #[test]
    fn live_requests_reconnect_every_half_period_then_end() {
        let (waits, ended) = drive(unreachable(Timeframe::M1));
        assert!(ended);
        assert_eq!(waits, vec![30; MAX_RECONNECTS as usize]);
    }

    #[test]
    fn history_task_reconnects_every_half_period_then_ends() {
        let t = DateTime::<Utc>::UNIX_EPOCH;
        let src = unreachable(Timeframe::M15).replay(t, t + TimeDelta::days(1));
        let (waits, ended) = drive(src);
        assert!(ended);
        assert_eq!(waits, vec![450; MAX_RECONNECTS as usize]); // 15m / 2
    }

    #[test]
    fn a_panicking_task_is_reported_as_a_failure() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let _guard = rt.enter();
        let mut task: Task<()> = Task::spawn(async { panic!("boom") });
        let result = loop {
            if let Some(r) = task.try_result() {
                break r;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(result, Err(SourceError::Provider(m)) if m.contains("task failed")));
    }

    #[test]
    fn unknown_symbol_ends_without_reconnecting() {
        let mut src = BinanceSource::new("NOPE", Timeframe::M1);
        src.reconnect(
            DateTime::<Utc>::UNIX_EPOCH,
            &SourceError::Unknown("NOPE:1m".into()),
            Phase::Live,
        );
        assert!(matches!(src.state, State::Ended));
        assert_eq!(src.failures, 0);
    }
}
