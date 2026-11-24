//! Futures simulation shared by `backtest_futures` and `chart_futures`:
//! the [`WithTicks`] condition, the [`Futures`] account (action) and the
//! command-line [`Args`]. See `backtest_futures.rs` for the rules.
#![allow(dead_code)] // each example uses a subset

use std::fmt::Write;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use ilus::action::{Action, ActionContext};
use ilus::candle::Candle;
use ilus::condition::Condition;
use ilus::condition::divergence::{
    ConfirmedPivots, Divergence, DivergenceKind, DivergenceParams, Pivots, RsiDivergence,
};
use ilus::error::{ActionError, ModifierError};
use ilus::modifier::NoModifier;
use ilus::source::{Item, Timeframe};
use ilus::transformer::rsi::RsiCandle;
use ilus::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

/// Maintenance margin rate (share of notional).
const MAINTENANCE: Decimal = Decimal::from_parts(5, 0, 0, false, 3); // 0.005

// ---------------------------------------------------------------- condition

/// Value emitted by [`WithTicks`] for every candle: the divergence it
/// confirms, if any, and the price pivots it confirms (for the RSI exits).
#[derive(Debug)]
pub struct Signal {
    pub divergence: Option<Divergence>,
    pub pivots: ConfirmedPivots,
}
impl TriggerValue for Signal {}

/// Wraps [`RsiDivergence`] and emits an event for every candle (the action
/// needs every candle to check stops, liquidation and RSI exits).
pub struct WithTicks {
    divergence: RsiDivergence,
    pivots: Pivots,
}

impl WithTicks {
    /// Divergences with `params`; pivots with the same `left` / `right`.
    pub fn new(params: DivergenceParams) -> Result<Self, ilus::error::ParameterError> {
        Ok(Self {
            divergence: RsiDivergence::new(params)?,
            pivots: Pivots::new(params.left, params.right),
        })
    }
}

impl Condition for WithTicks {
    type Input = RsiCandle;
    type Value = Signal;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        self.divergence.warm_up()
    }

    fn on_data(&mut self, item: Item<'_, RsiCandle>) -> Option<Detection<Signal>> {
        let pivots = self.pivots.push(item.data);
        let detection = self.divergence.on_data(item);
        let event_type = detection
            .as_ref()
            .map_or(TriggerEventType::Other("tick"), |d| d.event_type);
        Some(Detection {
            event_type,
            value: Signal {
                divergence: detection.map(|d| d.value),
                pivots,
            },
        })
    }

    fn on_modifier(
        &mut self,
        m: NoModifier,
    ) -> Result<Option<Detection<Self::Value>>, ModifierError> {
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
    /// Closed by the RSI exit rule.
    RsiExit,
    Liquidation,
}

/// One trade of the simulation.
#[derive(Debug, Clone)]
pub struct Trade {
    pub time: DateTime<Utc>,
    pub price: Decimal,
    pub kind: TradeKind,
    pub label: String,
}

/// Receives every trade of the simulation (e.g. to draw it on a chart, with
/// a modifier sent through `ctx`).
pub trait TradeRecorder: Send + 'static {
    fn record(&mut self, ctx: &mut ActionContext, trade: Trade);
}

/// No recorder.
impl TradeRecorder for () {
    fn record(&mut self, _: &mut ActionContext, _: Trade) {}
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
    /// Trades of the current event, given to the recorder after it.
    pending: Vec<Trade>,
    /// Open time of the candle being processed.
    time: DateTime<Utc>,
    start_sol: Decimal,
    leverage: Decimal,
    /// Stop-loss as a fraction of the entry price (0.03 = 3 %), if any.
    stop: Option<Decimal>,
    stops: u32,
    /// RSI exit levels: a long closes at the first high pivot with RSI
    /// above `high`, a short at the first low pivot with RSI below `low`.
    exit_rsi: Option<(Decimal, Decimal)>,
    rsi_exits: u32,
    /// Close a position and open the opposite one on an opposite divergence
    /// (off by default: a position closes only on its exit rules).
    flip_on_divergence: bool,
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
            pending: Vec::new(),
            time: DateTime::<Utc>::UNIX_EPOCH,
            start_sol,
            leverage,
            stop: stop_pct.map(|p| p / Decimal::ONE_HUNDRED),
            stops: 0,
            exit_rsi: None,
            rsi_exits: 0,
            flip_on_divergence: false,
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

    /// RSI exit: a long closes at the first high pivot after entry with RSI
    /// above `high` (e.g. 75), a short at the first low pivot with RSI below
    /// `low` (e.g. 30), at the close of the candle confirming the pivot.
    pub fn with_rsi_exit(mut self, high: Decimal, low: Decimal) -> Self {
        self.exit_rsi = Some((high, low));
        let _ = writeln!(
            self.log,
            "RSI exit: long at a high with RSI > {high}, short at a low with RSI < {low}"
        );
        self
    }

    /// On an opposite divergence, close the position and open the opposite
    /// one (the previous behaviour). Off by default.
    pub fn with_flip_on_divergence(mut self, flip: bool) -> Self {
        self.flip_on_divergence = flip;
        if flip {
            let _ = writeln!(self.log, "Flip on opposite divergence");
        }
        self
    }

    /// Close the position if the candle confirms an RSI exit pivot.
    fn check_rsi_exit(&mut self, when: &str, pivots: &ConfirmedPivots, close: Decimal) {
        let (Some(p), Some((high, low))) = (self.position, self.exit_rsi) else {
            return;
        };
        let exit = match p.side {
            Side::Long => pivots.high.filter(|h| h.rsi > high),
            Side::Short => pivots.low.filter(|l| l.rsi < low),
        };
        if let Some(pivot) = exit {
            self.rsi_exits += 1;
            let _ = writeln!(
                self.log,
                "{when}  RSI EXIT {:?} (pivot {} rsi {})",
                p.side,
                pivot.time.format("%m-%d %H:%M"),
                pivot.rsi.round_dp(2)
            );
            self.close(when, close, TradeKind::RsiExit);
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
            self.pending.push(Trade {
                time: self.time,
                price: worst,
                kind: TradeKind::Liquidation,
                label: "liquidated".into(),
            });
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
            match kind {
                TradeKind::Stop => "stop",
                TradeKind::RsiExit => "RSI exit",
                _ => "close",
            },
            self.pct(equity)
        );
        self.pending.push(Trade {
            time: self.time,
            price,
            kind,
            label,
        });
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
        self.pending.push(Trade {
            time: self.time,
            price,
            kind: TradeKind::Open(side),
            label,
        });
    }
}

impl<R: TradeRecorder> Futures<R> {
    /// Process one candle (and its divergence, if any).
    fn step(&mut self, value: &Signal, c: &Candle) {
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
            return; // no equity left
        }
        self.check_rsi_exit(&when, &value.pivots, c.close);

        let Some(d) = &value.divergence else {
            return;
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
            return;
        }
        if self.position.is_some_and(|p| p.side == opposite) {
            if !self.flip_on_divergence {
                // An open position only closes on its exit rules (RSI exit,
                // stop-loss, liquidation), not on an opposite divergence.
                let _ = writeln!(
                    self.log,
                    "{when}  skip {:?} divergence (position open)",
                    d.kind
                );
                return;
            }
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
    }
}

impl<R: TradeRecorder> Action for Futures<R> {
    type Value = Signal;
    type Input = RsiCandle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, Signal, RsiCandle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        self.step(event.value, &event.data.data);
        for trade in std::mem::take(&mut self.pending) {
            self.recorder.record(ctx, trade);
        }
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
            "=== Summary: {} transactions, {} stops, {} RSI exits, {} liquidations, fees paid {} USDT, lowest equity {} USDT",
            self.trades,
            self.stops,
            self.rsi_exits,
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
    /// Global tick of the replay (`--tick-ms`, default 0: as fast as possible).
    pub tick: Duration,
    /// RSI exit levels (`--exit-rsi-high`, `--exit-rsi-low`; default 75 / 30).
    pub exit_rsi: (Decimal, Decimal),
    /// `--flip-on-divergence`: an opposite divergence closes the position and
    /// opens the opposite one (default: ignored while a position is open).
    pub flip_on_divergence: bool,
    /// Description of the run.
    pub header: String,
}

/// Parse `[--days N | --from D [--to D]] [--leverage L] [--stop P] [--left N]
/// [--right N] [--min-gap N] [--rsi-low R] [--rsi-high R] [--lookahead N] [--tick-ms N]
/// [timeframe] [fees %...]`.
pub fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut days: u64 = 1;
    let mut from_date: Option<chrono::NaiveDate> = None;
    let mut to_date: Option<chrono::NaiveDate> = None;
    let mut leverage = Decimal::from(10);
    let mut tick = Duration::ZERO;
    let mut exit_rsi = (Decimal::from(75), Decimal::from(30));
    let mut flip_on_divergence = false;
    let mut stop: Option<Decimal> = None;
    // Pivot options; the gaps are given in candles and converted below.
    let (mut left, mut right) = (None::<usize>, None::<usize>);
    let mut min_gap = None::<i32>;
    let (mut rsi_low, mut rsi_high, mut lookahead) =
        (None::<Decimal>, None::<Decimal>, None::<usize>);
    while let Some(flag) = args.first().filter(|a| a.starts_with("--")).cloned() {
        if flag == "--flip-on-divergence" {
            flip_on_divergence = true;
            args.remove(0);
            continue;
        }
        let value = args.get(1).ok_or(format!("{flag} needs a value"))?.clone();
        match flag.as_str() {
            "--days" => days = value.parse().map_err(|e| format!("invalid --days: {e}"))?,
            "--tick-ms" => {
                tick = Duration::from_millis(
                    value
                        .parse()
                        .map_err(|e| format!("invalid --tick-ms: {e}"))?,
                )
            }
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
            "--exit-rsi-high" => {
                exit_rsi.0 = Decimal::from_str(&value)
                    .map_err(|e| format!("invalid --exit-rsi-high: {e}"))?
            }
            "--exit-rsi-low" => {
                exit_rsi.1 =
                    Decimal::from_str(&value).map_err(|e| format!("invalid --exit-rsi-low: {e}"))?
            }
            "--rsi-low" => {
                rsi_low =
                    Some(Decimal::from_str(&value).map_err(|e| format!("invalid --rsi-low: {e}"))?)
            }
            "--rsi-high" => {
                rsi_high = Some(
                    Decimal::from_str(&value).map_err(|e| format!("invalid --rsi-high: {e}"))?,
                )
            }
            "--lookahead" => {
                lookahead = Some(
                    value
                        .parse()
                        .map_err(|e| format!("invalid --lookahead: {e}"))?,
                )
            }
            _ => return Err(format!("unknown option {flag}").into()),
        }
        args.drain(..2);
    }
    if days < 1 || leverage < Decimal::ONE {
        return Err("--days and --leverage must be >= 1".into());
    }
    let timeframe = match args.first().and_then(|a| Timeframe::parse(a)) {
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
    params.rsi_low = rsi_low.unwrap_or(params.rsi_low);
    params.rsi_high = rsi_high.unwrap_or(params.rsi_high);
    params.lookahead = lookahead.unwrap_or(params.lookahead);
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
        "Futures simulation SOL/USDT {timeframe}, {}x, stop-loss {}, pivots left {} / right {}, min gap {} candles, RSI {} / {}, lookahead {}, RSI exit {} / {}, {}, from {} to {} (UTC)\n",
        leverage.normalize(),
        stop.map_or("none".to_string(), |p| format!("{}%", p.normalize())),
        params.left,
        params.right,
        params.min_gap.num_seconds() / candle.num_seconds(),
        params.rsi_low,
        params.rsi_high,
        params.lookahead,
        exit_rsi.0,
        exit_rsi.1,
        if flip_on_divergence {
            "flip on divergence"
        } else {
            "no flip"
        },
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
        tick,
        exit_rsi,
        flip_on_divergence,
        header,
    })
}
