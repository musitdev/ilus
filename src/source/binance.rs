//! Binance public market-data source (spec F1, default source).
//!
//! - History: REST `GET /api/v3/klines` (max 1000 candles per call).
//! - Live: WebSocket `<symbol>@kline_<interval>`; a candle is closed when `k.x` is true.
//! - No account or API key needed.
//!
//! Behavior: only closed candles are sent, in order, without duplicates.
//! After a disconnection the source reconnects with backoff and fetches the
//! missing candles over REST before resuming. REST errors are retried up to
//! [`MAX_RETRIES`] times; after that the source stops with an error.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use tokio_stream::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use crate::candle::Candle;
use crate::error::SourceError;
use crate::source::{Source, SourceChannel, SourceInfo, SourceSender, Timeframe};

/// Default REST endpoint.
pub const REST_URL: &str = "https://api.binance.com";
/// Default WebSocket endpoint.
pub const WS_URL: &str = "wss://stream.binance.com:9443";
/// Fallback REST endpoint (market data only).
pub const REST_URL_FALLBACK: &str = "https://data-api.binance.vision";
/// Fallback WebSocket endpoint (market data only).
pub const WS_URL_FALLBACK: &str = "wss://data-stream.binance.vision";

/// Max retries of a failing REST request before the source stops.
pub const MAX_RETRIES: u32 = 5;
/// Max candles per REST request (Binance limit).
const PAGE_LIMIT: usize = 1000;
/// Reconnect if no WebSocket message is received for this long
/// (Binance pushes kline updates every ~2 s).
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the source starts.
#[derive(Debug, Clone, Copy)]
enum Start {
    /// Live data only.
    Now,
    /// The last `n` closed candles, then live data.
    History(u32),
    /// Every candle from this time, then live data.
    At(DateTime<Utc>),
}

/// Binance spot candle source.
pub struct BinanceSource {
    channel: SourceChannel<Candle>,
    asset: String,
    timeframe: Timeframe,
    start: Start,
    end: Option<DateTime<Utc>>,
    rest_url: String,
    ws_url: String,
}

impl BinanceSource {
    /// Source for `asset` (Binance symbol, e.g. `SOLUSDT`) and `timeframe`.
    /// Starts with live data only; see [`with_history`](Self::with_history)
    /// and [`with_start`](Self::with_start).
    pub fn new(asset: impl Into<String>, timeframe: Timeframe) -> Self {
        let asset = asset.into().to_uppercase();
        let info = SourceInfo::Provider {
            provider: "binance".into(),
            asset: asset.clone(),
            timeframe,
        };
        Self {
            channel: SourceChannel::new(info),
            asset,
            timeframe,
            start: Start::Now,
            end: None,
            rest_url: REST_URL.into(),
            ws_url: WS_URL.into(),
        }
    }

    /// First send the last `candles` closed candles (backfill), then live data.
    pub fn with_history(mut self, candles: u32) -> Self {
        self.start = Start::History(candles);
        self
    }

    /// First send every closed candle since `start`, then live data.
    pub fn with_start(mut self, start: DateTime<Utc>) -> Self {
        self.start = Start::At(start);
        self
    }

    /// Stop after the last candle closed before `end`: the source only replays
    /// history and never switches to live data (e.g. for a simulation).
    /// An `end` in the future is capped at the current time.
    pub fn with_end(mut self, end: DateTime<Utc>) -> Self {
        self.end = Some(end);
        self
    }

    /// Use other endpoints (e.g. [`REST_URL_FALLBACK`], [`WS_URL_FALLBACK`]).
    pub fn with_endpoints(
        mut self,
        rest_url: impl Into<String>,
        ws_url: impl Into<String>,
    ) -> Self {
        self.rest_url = rest_url.into();
        self.ws_url = ws_url.into();
        self
    }
}

impl Source for BinanceSource {
    type Data = Candle;

    fn channel(&mut self) -> &mut SourceChannel<Candle> {
        &mut self.channel
    }

    async fn run(self) -> Result<(), SourceError> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| SourceError::Provider(e.to_string()))?;
        let ws_url = format!(
            "{}/ws/{}@kline_{}",
            self.ws_url,
            self.asset.to_lowercase(),
            self.timeframe
        );
        let mut feed = Feed {
            client,
            rest_url: self.rest_url,
            asset: self.asset,
            timeframe: self.timeframe,
            sender: self.channel.into_sender(),
            last_open: None,
            end: self.end,
        };

        // Fail early on an unknown asset.
        feed.fetch_klines(None, 1).await?;

        let from = match self.start {
            Start::Now => None,
            // `now` falls inside the forming candle: go back one more period
            // so that exactly `n` closed candles are returned.
            Start::History(n) => Some(Utc::now() - feed.timeframe.duration() * (n as i32 + 1)),
            Start::At(t) => Some(t),
        };
        if let Some(from) = from
            && feed.backfill(from).await? == Flow::ConsumerGone
        {
            return Ok(());
        }
        if feed.end.is_some() {
            feed.log("end reached, history replay done");
            return Ok(());
        }

        let mut backoff = BACKOFF_START;
        loop {
            match tokio_tungstenite::connect_async(ws_url.as_str()).await {
                Ok((ws, _)) => {
                    backoff = BACKOFF_START;
                    feed.log("connected");
                    if feed.live(ws).await? == Flow::ConsumerGone {
                        return Ok(());
                    }
                }
                Err(e) => feed.log(&format!("connection failed: {e}")),
            }
            feed.log(&format!("reconnecting in {}s", backoff.as_secs()));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Disconnected,
    ConsumerGone,
}

/// Running state of a Binance source.
struct Feed {
    client: reqwest::Client,
    rest_url: String,
    asset: String,
    timeframe: Timeframe,
    sender: SourceSender<Candle>,
    /// Open time of the last candle sent (dedup and gap detection).
    last_open: Option<DateTime<Utc>>,
    /// Only candles closed before this time are sent (history-only mode).
    end: Option<DateTime<Utc>>,
}

impl Feed {
    fn log(&self, msg: &str) {
        eprintln!("[{}] {msg}", self.sender.info());
    }

    /// Send a candle unless it is a duplicate or out of order.
    /// Waits while the channel is full (backpressure from the consumer).
    async fn emit(&mut self, candle: Candle) -> Flow {
        if self.last_open.is_some_and(|last| candle.open_time <= last) {
            return Flow::Continue;
        }
        self.last_open = Some(candle.open_time);
        if self.sender.send(candle).await {
            Flow::Continue
        } else {
            Flow::ConsumerGone
        }
    }

    /// Send every closed candle from `from` to now (or to `end`), over REST.
    async fn backfill(&mut self, mut from: DateTime<Utc>) -> Result<Flow, SourceError> {
        loop {
            let rows = self.fetch_klines(Some(from), PAGE_LIMIT).await?;
            let now = Utc::now();
            let limit = self.end.map_or(now, |end| end.min(now));
            let full_page = rows.len() == PAGE_LIMIT;
            let Some(last) = rows.last().map(|k| k.candle.open_time) else {
                return Ok(Flow::Continue);
            };
            for kline in rows {
                if kline.close_time >= limit {
                    return Ok(Flow::Continue); // still forming, or after `end`
                }
                if self.emit(kline.candle).await == Flow::ConsumerGone {
                    return Ok(Flow::ConsumerGone);
                }
            }
            if !full_page {
                return Ok(Flow::Continue);
            }
            from = last + self.timeframe.duration();
        }
    }

    /// Read the WebSocket until it disconnects.
    async fn live<S>(&mut self, mut ws: S) -> Result<Flow, SourceError>
    where
        S: tokio_stream::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        // Fill the gap since the last candle sent (reconnection).
        if let Some(last) = self.last_open
            && self.backfill(last + self.timeframe.duration()).await? == Flow::ConsumerGone
        {
            return Ok(Flow::ConsumerGone);
        }
        loop {
            let msg = match tokio::time::timeout(WS_IDLE_TIMEOUT, ws.next()).await {
                Err(_) => {
                    self.log("no data received, reconnecting");
                    return Ok(Flow::Disconnected);
                }
                Ok(None) => return Ok(Flow::Disconnected),
                Ok(Some(Err(e))) => {
                    self.log(&format!("websocket error: {e}"));
                    return Ok(Flow::Disconnected);
                }
                Ok(Some(Ok(msg))) => msg,
            };
            let text = match msg {
                Message::Text(text) => text,
                Message::Close(_) => return Ok(Flow::Disconnected),
                _ => continue, // ping/pong handled by tungstenite
            };
            let candle = match parse_ws_kline(text.as_str()) {
                Ok(Some(candle)) => candle,
                Ok(None) => continue, // candle not closed yet
                Err(e) => {
                    self.log(&e.to_string());
                    continue;
                }
            };
            // Missing candles between the last one sent and this one: fetch them.
            if let Some(last) = self.last_open
                && candle.open_time > last + self.timeframe.duration()
                && self.backfill(last + self.timeframe.duration()).await? == Flow::ConsumerGone
            {
                return Ok(Flow::ConsumerGone);
            }
            if self.emit(candle).await == Flow::ConsumerGone {
                return Ok(Flow::ConsumerGone);
            }
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

// ------------------------------------------------------------------ parsing

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

#[derive(Deserialize)]
struct WsEvent {
    k: WsKline,
}

#[derive(Deserialize)]
struct WsKline {
    #[serde(rename = "t")]
    open_time: i64,
    #[serde(rename = "o")]
    open: String,
    #[serde(rename = "h")]
    high: String,
    #[serde(rename = "l")]
    low: String,
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "v")]
    volume: String,
    #[serde(rename = "x")]
    closed: bool,
}

/// Parse a WebSocket kline event. Returns `None` if the candle is not closed.
fn parse_ws_kline(text: &str) -> Result<Option<Candle>, SourceError> {
    let event: WsEvent = serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
    let k = event.k;
    if !k.closed {
        return Ok(None);
    }
    Ok(Some(Candle {
        open_time: millis(k.open_time)?,
        open: decimal(&k.open)?,
        high: decimal(&k.high)?,
        low: decimal(&k.low)?,
        close: decimal(&k.close)?,
        volume: decimal(&k.volume)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    // Hand-written samples following the documented Binance formats.
    const REST: &str = r#"[
        [1700000000000,"150.10","151.00","149.50","150.80","1234.5",1700000059999,"0",10,"0","0","0"],
        [1700000060000,"150.80","152.25","150.70","152.00","987.25",1700000119999,"0",12,"0","0","0"]
    ]"#;

    fn ws(closed: bool) -> String {
        format!(
            r#"{{"e":"kline","E":1700000060001,"s":"SOLUSDT","k":{{"t":1700000000000,"T":1700000059999,"s":"SOLUSDT","i":"1m","o":"150.10","c":"150.80","h":"151.00","l":"149.50","v":"1234.5","x":{closed}}}}}"#
        )
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
    fn parses_closed_ws_kline() {
        let candle = parse_ws_kline(&ws(true)).unwrap().unwrap();
        assert_eq!(candle.open_time.timestamp_millis(), 1_700_000_000_000);
        assert_eq!(candle.close, dec!(150.80));
        assert_eq!(candle.volume, dec!(1234.5));
    }

    #[test]
    fn ignores_forming_ws_kline() {
        assert_eq!(parse_ws_kline(&ws(false)).unwrap(), None);
    }

    #[test]
    fn source_info_is_uppercase_symbol() {
        let mut src = BinanceSource::new("solusdt", Timeframe::M1);
        let stream = src.get_source_stream().unwrap();
        assert_eq!(stream.info().to_string(), "binance:SOLUSDT:1m");
    }
}
