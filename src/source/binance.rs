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
//! takes one candle from the buffer. A call never blocks. REST errors are
//! retried up to [`MAX_RETRIES`] times.

use std::collections::VecDeque;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::candle::Candle;
use crate::error::SourceError;
use crate::source::{
    CLOSE_DELAY, Mode, Source, SourceInfo, SourcePoll, Timeframe, next_aligned_call,
};

/// Default REST endpoint.
pub const REST_URL: &str = "https://api.binance.com";
/// Fallback REST endpoint (market data only).
pub const REST_URL_FALLBACK: &str = "https://data-api.binance.vision";

/// Max retries of a failing REST request.
pub const MAX_RETRIES: u32 = 5;
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
    state: State,
}

enum HistMsg {
    Candle(Candle, Mode),
    Done,
    Failed(SourceError),
}

enum State {
    Start,
    History(mpsc::UnboundedReceiver<HistMsg>),
    Live {
        queue: VecDeque<Candle>,
        in_flight: Option<oneshot::Receiver<Result<Vec<Candle>, SourceError>>>,
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
        })
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
        match &self.state {
            State::Start | State::History(_) if self.replay.is_some() => Mode::Replay,
            State::Start if self.bootstrap == 0 => Mode::Normal,
            State::Start | State::History(_) => Mode::Bootstrap,
            State::Live { .. } | State::Ended => Mode::Normal,
        }
    }

    fn poll(&mut self, _now: DateTime<Utc>) -> SourcePoll<Candle> {
        match &mut self.state {
            State::Start => {
                let fetcher = match self.make_fetcher() {
                    Ok(f) => f,
                    Err(e) => return self.fail(&e),
                };
                self.fetcher = Some(fetcher.clone());
                if self.bootstrap == 0 && self.replay.is_none() {
                    self.state = State::Live {
                        queue: VecDeque::new(),
                        in_flight: None,
                    };
                } else {
                    let (tx, rx) = mpsc::unbounded_channel();
                    tokio::spawn(history(fetcher, self.bootstrap, self.replay, tx));
                    self.state = State::History(rx);
                }
                SourcePoll::Pending
            }
            State::History(rx) => match rx.try_recv() {
                Ok(HistMsg::Candle(candle, mode)) => {
                    self.last_open = Some(candle.open_time);
                    SourcePoll::Ready(candle, mode)
                }
                Ok(HistMsg::Done) if self.replay.is_some() => {
                    self.state = State::Ended;
                    SourcePoll::Ended
                }
                Ok(HistMsg::Done) => {
                    self.state = State::Live {
                        queue: VecDeque::new(),
                        in_flight: None,
                    };
                    SourcePoll::Pending
                }
                Ok(HistMsg::Failed(e)) => self.fail(&e),
                Err(mpsc::error::TryRecvError::Empty) => SourcePoll::Pending,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.fail(&SourceError::Provider("history task stopped".into()))
                }
            },
            State::Live { queue, in_flight } => {
                if let Some(rx) = in_flight {
                    match rx.try_recv() {
                        Ok(Ok(candles)) => {
                            queue.extend(new_candles(candles, self.last_open));
                            *in_flight = None;
                        }
                        Ok(Err(e)) => {
                            // Retried at the next call; missed candles come with it.
                            eprintln!("[{}] {e}", self.info);
                            *in_flight = None;
                        }
                        Err(oneshot::error::TryRecvError::Empty) => {}
                        Err(oneshot::error::TryRecvError::Closed) => *in_flight = None,
                    }
                }
                if let Some(candle) = queue.pop_front() {
                    self.last_open = Some(candle.open_time);
                    return SourcePoll::Ready(candle, Mode::Normal);
                }
                if in_flight.is_none()
                    && let Some(fetcher) = self.fetcher.clone()
                {
                    let (tx, rx) = oneshot::channel();
                    let last_open = self.last_open;
                    tokio::spawn(async move {
                        let _ = tx.send(latest(&fetcher, last_open).await);
                    });
                    *in_flight = Some(rx);
                }
                SourcePoll::Pending
            }
            State::Ended => SourcePoll::Ended,
        }
    }

    fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        match &self.state {
            State::Live { queue, .. } if !queue.is_empty() => now,
            State::Live {
                in_flight: Some(_), ..
            } => now + IN_FLIGHT_RETRY,
            _ => next_aligned_call(self.last_open, self.timeframe, now, CLOSE_DELAY),
        }
    }
}

// ------------------------------------------------------- background work

/// Bootstrap (candles before the start), then the replay period.
async fn history(
    f: Fetcher,
    bootstrap: usize,
    replay: Option<(DateTime<Utc>, DateTime<Utc>)>,
    tx: mpsc::UnboundedSender<HistMsg>,
) {
    let now = Utc::now();
    let start = replay.map_or(now, |(from, _)| from);
    let result = async {
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
                if tx.send(HistMsg::Candle(c, Mode::Bootstrap)).is_err() {
                    return Ok(());
                }
            }
        }
        if let Some((from, to)) = replay {
            f.range(from, to.min(now), now, |c| {
                tx.send(HistMsg::Candle(c, Mode::Replay)).is_ok()
            })
            .await?;
        }
        Ok::<(), SourceError>(())
    }
    .await;
    let _ = tx.send(match result {
        Ok(()) => HistMsg::Done,
        Err(e) => HistMsg::Failed(e),
    });
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
                    if attempt > MAX_RETRIES {
                        return Err(SourceError::Provider(format!(
                            "giving up after {MAX_RETRIES} retries: {reason}"
                        )));
                    }
                    let wait = wait.unwrap_or(backoff);
                    self.log(&format!(
                        "{reason}; retry {attempt}/{MAX_RETRIES} in {}s",
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
}
