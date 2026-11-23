//! Futures simulation shared by `backtest_futures` and `chart_futures`:
//! the [`WithTicks`] condition, the [`Futures`] account (action) and the
//! command-line [`Args`]. See `backtest_futures.rs` for the rules.

use std::fmt::Write;
use std::marker::PhantomData;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use ilus::action::{Action, EventOf};
use ilus::condition::Condition;
use ilus::condition::divergence::{Divergence, DivergenceKind, DivergenceParams, RsiDivergence};
use ilus::error::{ActionError, ModifierError};
use ilus::modifier::{ModifierSender, NoModifier};
use ilus::source::{SourceData, Timeframe};
use ilus::transformer::Derived;
use ilus::transformer::rsi::RsiCandle;
use ilus::trigger::{Detection, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

/// Maintenance margin rate (share of notional).
const MAINTENANCE: Decimal = Decimal::from_parts(5, 0, 0, false, 3); // 0.005

// ---------------------------------------------------------------- condition

/// Value emitted by [`WithTicks`]: a plain candle, or a divergence.
#[derive(Debug)]
pub enum Signal {
    Tick,
    Divergence(Divergence),
}
impl TriggerValue for Signal {}

/// Items carrying an RSI candle: the RSI output itself, or the output of a
/// pass-through transformer placed after it (e.g. the chart).
pub trait HasRsiCandle: SourceData {
    fn rsi_candle(&self) -> &RsiCandle;
}

impl HasRsiCandle for RsiCandle {
    fn rsi_candle(&self) -> &RsiCandle {
        self
    }
}

impl HasRsiCandle for Derived<RsiCandle, ()> {
    fn rsi_candle(&self) -> &RsiCandle {
        &self.data
    }
}

/// Wraps [`RsiDivergence`] and also emits an event for every candle
/// (the action needs every candle to check stops and liquidation).
pub struct WithTicks<I = RsiCandle> {
    inner: RsiDivergence,
    input: PhantomData<fn(I)>,
}

impl<I> WithTicks<I> {
    pub fn new(inner: RsiDivergence) -> Self {
        Self {
            inner,
            input: PhantomData,
        }
    }
}

impl<I: HasRsiCandle> Condition for WithTicks<I> {
    type Input = I;
    type Value = Signal;
    type Modifier = NoModifier;

    fn on_data(&mut self, item: &I) -> Option<Detection<Signal>> {
        Some(match self.inner.on_data(item.rsi_candle()) {
            Some(d) => Detection {
                event_type: d.event_type,
                value: Signal::Divergence(d.value),
            },
            None => Detection {
                event_type: TriggerEventType::Other("tick"),
                value: Signal::Tick,
            },
        })
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

// ------------------------------------------------------------------- action

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Side {
    Long,
    Short,
}

/// What happened, for a [`TradeRecorder`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TradeKind {
    Open(Side),
    Close,
    Stop,
    Liquidation,
}

/// Receives every trade of the simulation (e.g. to draw it on a chart).
pub trait TradeRecorder: Send + 'static {
    fn record(&mut self, time: DateTime<Utc>, price: Decimal, kind: TradeKind, label: String);
}

/// No recorder.
impl TradeRecorder for () {
    fn record(&mut self, _: DateTime<Utc>, _: Decimal, _: TradeKind, _: String) {}
}

#[derive(Debug, Clone, Copy)]
struct Position {
    side: Side,
    entry: Decimal,
    /// Position size in SOL.
    qty: Decimal,
    margin: Decimal,
}

impl Position {
    fn pnl(&self, price: Decimal) -> Decimal {
        match self.side {
            Side::Long => self.qty * (price - self.entry),
            Side::Short => self.qty * (self.entry - price),
        }
    }
}

/// Simulated futures account.
pub struct Futures<R: TradeRecorder = ()> {
    recorder: R,
    /// Open time of the candle being processed.
    time: DateTime<Utc>,
    start_sol: Decimal,
    leverage: Decimal,
    /// Stop-loss as a fraction of the entry price (0.03 = 3 %), if any.
    stop: Option<Decimal>,
    stops: u32,
    fee: Decimal,
    /// Equity in USDT when flat (set at the first candle).
    equity: Option<Decimal>,
    start_equity: Decimal,
    position: Option<Position>,
    last_price: Decimal,
    fees_paid: Decimal,
    trades: u32,
    liquidations: u32,
    /// Lowest equity seen (worst case inside candles).
    min_equity: Decimal,
    /// Time, price and equity after closing at the last bearish divergence.
    last_bearish: Option<(DateTime<Utc>, Decimal, Decimal)>,
    log: String,
}

pub fn fmt_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M").to_string()
}

impl<R: TradeRecorder> Futures<R> {
    pub fn new(
        start_sol: Decimal,
        leverage: Decimal,
        stop_pct: Option<Decimal>,
        fee_pct: Decimal,
        recorder: R,
    ) -> Self {
        Self {
            recorder,
            time: DateTime::<Utc>::UNIX_EPOCH,
            start_sol,
            leverage,
            stop: stop_pct.map(|p| p / Decimal::ONE_HUNDRED),
            stops: 0,
            fee: fee_pct / Decimal::ONE_HUNDRED,
            equity: None,
            start_equity: Decimal::ZERO,
            position: None,
            last_price: Decimal::ZERO,
            fees_paid: Decimal::ZERO,
            trades: 0,
            liquidations: 0,
            min_equity: Decimal::MAX,
            last_bearish: None,
            log: format!(
                "##### Leverage {}x, stop-loss {}, fee {}% of notional per transaction\n",
                leverage.normalize(),
                stop_pct.map_or("none".to_string(), |p| format!("{}%", p.normalize())),
                fee_pct.normalize()
            ),
        }
    }

    fn pct(&self, equity: Decimal) -> Decimal {
        ((equity - self.start_equity) / self.start_equity * Decimal::ONE_HUNDRED).round_dp(2)
    }

    /// Equity if everything were closed at `price` (no fee).
    fn mark(&self, price: Decimal) -> Decimal {
        match self.position {
            Some(p) => p.margin + p.pnl(price),
            None => self.equity.unwrap_or_default(),
        }
    }

    /// Close the position if the candle reached the stop-loss price.
    fn check_stop(&mut self, when: &str, open: Decimal, low: Decimal, high: Decimal) {
        let (Some(p), Some(stop)) = (self.position, self.stop) else {
            return;
        };
        let fill = match p.side {
            Side::Long => {
                let level = p.entry * (Decimal::ONE - stop);
                (low <= level).then(|| level.min(open))
            }
            Side::Short => {
                let level = p.entry * (Decimal::ONE + stop);
                (high >= level).then(|| level.max(open))
            }
        };
        if let Some(price) = fill {
            self.stops += 1;
            let _ = writeln!(
                self.log,
                "{when}  STOP  {:?} hit at {}",
                p.side,
                price.round_dp(4)
            );
            self.close(when, price, TradeKind::Stop);
        }
    }

    /// Check liquidation against the worst price of the candle.
    fn check_liquidation(&mut self, when: &str, low: Decimal, high: Decimal) {
        let Some(p) = self.position else { return };
        let worst = match p.side {
            Side::Long => low,
            Side::Short => high,
        };
        let equity = p.margin + p.pnl(worst);
        if equity <= MAINTENANCE * p.qty * worst {
            self.min_equity = Decimal::ZERO;
            self.liquidations += 1;
            self.position = None;
            self.equity = Some(Decimal::ZERO);
            let _ = writeln!(
                self.log,
                "{when}  LIQUIDATED {:?} (entry {}, price reached {}) -> margin lost, trading stops",
                p.side,
                p.entry.normalize(),
                worst.normalize()
            );
            self.recorder.record(
                self.time,
                worst,
                TradeKind::Liquidation,
                "liquidated".into(),
            );
        } else {
            self.min_equity = self.min_equity.min(equity);
        }
    }

    fn close(&mut self, when: &str, price: Decimal, kind: TradeKind) {
        let Some(p) = self.position.take() else {
            return;
        };
        let fee = p.qty * price * self.fee;
        self.fees_paid += fee;
        let equity = (p.margin + p.pnl(price) - fee).max(Decimal::ZERO);
        self.equity = Some(equity);
        self.trades += 1;
        let _ = writeln!(
            self.log,
            "{when}  CLOSE {:<5} @ {:<8} pnl {:>9} -> equity {:>9} USDT ({:+}%)",
            format!("{:?}", p.side),
            price.round_dp(4).normalize(),
            (p.pnl(price) - fee).round_dp(2),
            equity.round_dp(2),
            self.pct(equity)
        );
        let label = format!(
            "{} {:+}%",
            if kind == TradeKind::Stop {
                "stop"
            } else {
                "close"
            },
            self.pct(equity)
        );
        self.recorder.record(self.time, price, kind, label);
    }

    fn open(&mut self, when: &str, side: Side, price: Decimal) {
        let equity = self.equity.unwrap_or_default();
        if equity <= Decimal::ZERO {
            return;
        }
        // equity = margin + fee, fee = margin * leverage * fee rate
        let margin = equity / (Decimal::ONE + self.leverage * self.fee);
        let fee = equity - margin;
        self.fees_paid += fee;
        let qty = margin * self.leverage / price;
        self.position = Some(Position {
            side,
            entry: price,
            qty,
            margin,
        });
        self.trades += 1;
        let _ = writeln!(
            self.log,
            "{when}  OPEN  {:<5} @ {:<8} size {:>8} SOL  margin {:>9} USDT",
            format!("{side:?}"),
            price.normalize(),
            qty.round_dp(3),
            margin.round_dp(2)
        );
        let label = format!("{side:?} {}", price.normalize());
        self.recorder
            .record(self.time, price, TradeKind::Open(side), label);
    }
}

impl<I: HasRsiCandle, R: TradeRecorder> Action<WithTicks<I>> for Futures<R> {
    async fn run(
        &mut self,
        event: EventOf<WithTicks<I>>,
        _modifiers: &ModifierSender<NoModifier>,
    ) -> Result<(), ActionError> {
        let c = &event.data.rsi_candle().data;
        self.time = c.open_time;
        let when = fmt_time(c.open_time);
        self.last_price = c.close;
        if self.equity.is_none() {
            self.start_equity = self.start_sol * c.close;
            self.equity = Some(self.start_equity);
            self.min_equity = self.start_equity;
            let _ = writeln!(
                self.log,
                "{when}  start: {} SOL @ {} = {} USDT",
                self.start_sol,
                c.close.normalize(),
                self.start_equity.round_dp(2)
            );
        }

        self.check_stop(&when, c.open, c.low, c.high);
        self.check_liquidation(&when, c.low, c.high);
        if self.position.is_none() && self.equity == Some(Decimal::ZERO) {
            return Ok(()); // no equity left
        }

        let Signal::Divergence(d) = event.value else {
            return Ok(());
        };
        let (target, opposite) = match d.kind {
            DivergenceKind::Bullish => (Side::Long, Side::Short),
            DivergenceKind::Bearish => (Side::Short, Side::Long),
        };
        if self.position.is_some_and(|p| p.side == target) {
            let _ = writeln!(
                self.log,
                "{when}  skip {:?} divergence (already {target:?})",
                d.kind
            );
            return Ok(());
        }
        if self.position.is_some_and(|p| p.side == opposite) {
            self.close(&when, c.close, TradeKind::Close);
        }
        if d.kind == DivergenceKind::Bearish {
            let equity = self.equity.unwrap_or_default();
            self.last_bearish = Some((c.open_time, c.close, equity));
            let _ = writeln!(
                self.log,
                "{when}  (bearish divergence: equity = {} SOL at {})",
                (equity / c.close).round_dp(4),
                c.close.normalize()
            );
        }
        self.open(&when, target, c.close);
        Ok(())
    }
}

/// Print the log and the summary when the stream ends.
impl<R: TradeRecorder> Drop for Futures<R> {
    fn drop(&mut self) {
        let mut out = std::mem::take(&mut self.log);
        let now = self.mark(self.last_price);
        let _ = writeln!(
            out,
            "=== Summary: {} transactions, {} stops, {} liquidations, fees paid {} USDT, lowest equity {} USDT",
            self.trades,
            self.stops,
            self.liquidations,
            self.fees_paid.round_dp(2),
            self.min_equity.round_dp(2)
        );
        if let Some((time, price, equity)) = self.last_bearish {
            let _ = writeln!(
                out,
                "At the last bearish divergence ({}, price {}): {} USDT = {} SOL ({:+}%)",
                fmt_time(time),
                price.normalize(),
                equity.round_dp(2),
                (equity / price).round_dp(4),
                self.pct(equity)
            );
        }
        if self.last_price > Decimal::ZERO {
            let _ = writeln!(
                out,
                "Now (price {}, open {}): {} USDT = {} SOL ({:+}%)",
                self.last_price.normalize(),
                self.position
                    .map_or("none".to_string(), |p| format!("{:?}", p.side)),
                now.round_dp(2),
                (now / self.last_price).round_dp(4),
                self.pct(now)
            );
        }
        println!("{out}");
    }
}

// ---------------------------------------------------------------- arguments

/// Command-line options shared by the futures examples.
pub struct Args {
    pub timeframe: Timeframe,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub leverage: Decimal,
    pub stop: Option<Decimal>,
    pub params: DivergenceParams,
    pub fees: Vec<Decimal>,
    /// Description of the run.
    pub header: String,
}

pub fn timeframe(arg: &str) -> Option<Timeframe> {
    [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::H1,
        Timeframe::H4,
        Timeframe::D1,
    ]
    .into_iter()
    .find(|tf| tf.to_string() == arg)
}

/// Parse `[--days N | --from D [--to D]] [--leverage L] [--stop P] [--left N]
/// [--right N] [--min-gap N] [--max-gap N] [timeframe] [fees %...]`.
pub fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut days: u64 = 1;
    let mut from_date: Option<chrono::NaiveDate> = None;
    let mut to_date: Option<chrono::NaiveDate> = None;
    let mut leverage = Decimal::from(10);
    let mut stop: Option<Decimal> = None;
    // Pivot options; the gaps are given in candles and converted below.
    let (mut left, mut right) = (None::<usize>, None::<usize>);
    let (mut min_gap, mut max_gap) = (None::<i32>, None::<i32>);
    while let Some(flag) = args.first().filter(|a| a.starts_with("--")).cloned() {
        let value = args.get(1).ok_or(format!("{flag} needs a value"))?.clone();
        match flag.as_str() {
            "--days" => days = value.parse().map_err(|e| format!("invalid --days: {e}"))?,
            "--leverage" => {
                leverage =
                    Decimal::from_str(&value).map_err(|e| format!("invalid --leverage: {e}"))?
            }
            "--stop" => {
                let p = Decimal::from_str(&value).map_err(|e| format!("invalid --stop: {e}"))?;
                if p <= Decimal::ZERO {
                    return Err("--stop must be > 0".into());
                }
                stop = Some(p);
            }
            "--from" => {
                from_date = Some(
                    value
                        .parse()
                        .map_err(|e| format!("invalid --from (YYYY-MM-DD): {e}"))?,
                )
            }
            "--to" => {
                to_date = Some(
                    value
                        .parse()
                        .map_err(|e| format!("invalid --to (YYYY-MM-DD): {e}"))?,
                )
            }
            "--left" => left = Some(value.parse().map_err(|e| format!("invalid --left: {e}"))?),
            "--right" => right = Some(value.parse().map_err(|e| format!("invalid --right: {e}"))?),
            "--min-gap" => {
                min_gap = Some(
                    value
                        .parse()
                        .map_err(|e| format!("invalid --min-gap: {e}"))?,
                )
            }
            "--max-gap" => {
                max_gap = Some(
                    value
                        .parse()
                        .map_err(|e| format!("invalid --max-gap: {e}"))?,
                )
            }
            _ => return Err(format!("unknown option {flag}").into()),
        }
        args.drain(..2);
    }
    if days < 1 || leverage < Decimal::ONE {
        return Err("--days and --leverage must be >= 1".into());
    }
    let timeframe = match args.first().and_then(|a| timeframe(a)) {
        Some(tf) => {
            args.remove(0);
            tf
        }
        None => Timeframe::M1,
    };
    // Defaults depend on the timeframe; options override them.
    let mut params = DivergenceParams::for_timeframe(timeframe);
    let candle = timeframe.duration();
    params.left = left.unwrap_or(params.left);
    params.right = right.unwrap_or(params.right);
    if let Some(n) = min_gap {
        params.min_gap = candle * n;
    }
    if let Some(n) = max_gap {
        params.max_gap = candle * n;
    }
    let mut fees = args
        .iter()
        .map(|a| Decimal::from_str(a).map_err(|e| format!("invalid fee '{a}': {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    if fees.is_empty() {
        fees.push(Decimal::ZERO);
    }

    let midnight = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).map(|t| t.and_utc());
    let now = match to_date {
        Some(d) => midnight(d).ok_or("invalid --to")?,
        None => Utc::now(),
    };
    let from = midnight(from_date.unwrap_or(Utc::now().date_naive() - chrono::Days::new(days)))
        .ok_or("cannot compute start date")?;
    if from >= now {
        return Err("the start must be before the end".into());
    }
    let header = format!(
        "Futures simulation SOL/USDT {timeframe}, {}x, stop-loss {}, pivots left {} / right {}, gap {}..{} candles, from {} to {} (UTC)\n",
        leverage.normalize(),
        stop.map_or("none".to_string(), |p| format!("{}%", p.normalize())),
        params.left,
        params.right,
        params.min_gap.num_seconds() / candle.num_seconds(),
        params.max_gap.num_seconds() / candle.num_seconds(),
        fmt_time(from),
        fmt_time(now)
    );

    Ok(Args {
        timeframe,
        from,
        to: now,
        leverage,
        stop,
        params,
        fees,
        header,
    })
}
