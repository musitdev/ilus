//! Simulation of the test agent (spec F10): the fitness of a genome.
//!
//! ```text
//! candles 15m (memory) ─┬─> Decision ──> SimAccount (sends Closed on stop / liquidation)
//!                       ├─> Rsi ─> DivergenceVoter 15m ─> SendVotes
//!                       ├─> Aggregate 1h ─> OnValue(Rsi) ─> Projected(DivergenceVoter 1h) ─> SendVotes
//!                       ├─> Combine(Ema fast, Ema slow) ─> EmaCrossVoter ─> SendVotes
//!                       ├─> Trend(EMA n) ─> TrendVoter ─> SendVotes
//!                       └─> PatternVoter (13 patterns) ─> SendVotes
//! ```
//!
//! The candles are given by the caller (loaded once, e.g. from Binance or a
//! file): no network. Those before `from` warm the indicators up
//! (bootstrap); the account trades from `from` to `to` (excluded). Each call
//! runs its own executor, so several simulations can run in parallel
//! threads (not from inside an async task).

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::account::{Account, AccountParams, AccountSummary};
use crate::decision::{Decision, DecisionEvent, DecisionKind};
use crate::genome::{AgentParams, DivergenceGenes, GenomeError};
use crate::voters::{DivergenceVoter, EmaCrossVoter, PatternVoter, TrendVoter};
use crate::{DecisionModifier, SendVotes, Weights};
use trading::action::{Action, ActionContext};
use trading::candle::Candle;
use trading::condition::divergence::DivergenceParams;
use trading::condition::patterns::{PatternCondition, ThresholdSet};
use trading::condition::projected::{Projected, aggregated};
use trading::error::ActionError;
use trading::executor::{Clock, Executor};
use trading::graph::Graph;
use trading::modifier::ModifierHandle;
use trading::source::Timeframe;
use trading::source::mock::MockSource;
use trading::transformer::aggregate::Aggregate;
use trading::transformer::combine::{Combine, OnValue};
use trading::transformer::ema::Ema;
use trading::transformer::rsi::Rsi;
use trading::transformer::trend::Trend;
use trading::trigger::TriggerEvent;

/// RSI period of the divergences (fixed).
const RSI_PERIOD: usize = 14;
/// Pivot candles before / after (fixed).
const PIVOT_SIDE: usize = 3;

/// Simulation settings (not genes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimConfig {
    /// First candle traded (the candles before it warm up the indicators).
    pub from: DateTime<Utc>,
    /// End of the simulation (excluded).
    pub to: DateTime<Utc>,
    /// The account.
    pub account: AccountParams,
}

/// Result of a simulation: the fitness and the details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimResult {
    /// Parameters decoded from the genome.
    pub params: AgentParams,
    /// The account at the end (gain in %, trades, events, ...).
    pub account: AccountSummary,
}

impl SimResult {
    /// The fitness: the gain in % of the capital.
    pub fn gain_pct(&self) -> Decimal {
        self.account.gain_pct
    }
}

/// Why a simulation cannot run.
#[derive(Debug)]
pub enum SimError {
    /// The genome cannot be decoded.
    Genome(GenomeError),
    /// Invalid settings or parameters.
    Invalid(String),
    /// Not enough candles before `from` to warm the indicators up.
    History {
        /// Candles needed before `from`.
        needed: usize,
        /// Candles given before `from`.
        got: usize,
    },
    /// The executor failed.
    Executor(String),
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Genome(e) => write!(f, "{e}"),
            Self::Invalid(e) => write!(f, "simulation: {e}"),
            Self::History { needed, got } => write!(
                f,
                "simulation: {needed} candles needed before `from`, {got} given"
            ),
            Self::Executor(e) => write!(f, "simulation: executor: {e}"),
        }
    }
}

impl std::error::Error for SimError {}

impl From<GenomeError> for SimError {
    fn from(e: GenomeError) -> Self {
        Self::Genome(e)
    }
}

fn invalid(e: impl fmt::Display) -> SimError {
    SimError::Invalid(e.to_string())
}

/// Candles needed before `from` so that every rule is warm when trading
/// starts (the 1h chain counts in 1h candles).
pub fn history_needed(p: &AgentParams) -> usize {
    let pivots = PIVOT_SIDE + 1 + PIVOT_SIDE;
    let patterns = ThresholdSet::default();
    [
        RSI_PERIOD + 1 + pivots,
        4 * (RSI_PERIOD + 1 + pivots) + 4,
        p.ema_slow + 1,
        p.trend_period + 1,
        patterns.avg_len.max(patterns.trend_len) + 3,
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
}

/// Simulate the agent of `genome` on 15m `candles` (sorted, no gap).
pub fn simulate(
    genome: &[f64],
    candles: &[Candle],
    cfg: &SimConfig,
) -> Result<SimResult, SimError> {
    let params = AgentParams::decode(genome)?;
    simulate_params(&params, candles, cfg)
}

/// Simulate the agent of `params` on 15m `candles` (sorted, no gap).
pub fn simulate_params(
    params: &AgentParams,
    candles: &[Candle],
    cfg: &SimConfig,
) -> Result<SimResult, SimError> {
    if cfg.from >= cfg.to {
        return Err(invalid("from must be before to"));
    }
    let account = Account::new(cfg.account).map_err(invalid)?;
    let before = candles
        .iter()
        .take_while(|c| c.open_time < cfg.from)
        .count();
    let needed = history_needed(params);
    if before < needed {
        return Err(SimError::History {
            needed,
            got: before,
        });
    }
    let items: Vec<Candle> = candles
        .iter()
        .filter(|c| c.open_time < cfg.to)
        .cloned()
        .collect();

    let shared = Arc::new(Mutex::new(account));
    let mut g = Graph::new();
    let src = g.source(MockSource::replay("agent", Timeframe::M15, items).with_bootstrap(before));
    build(&mut g, &src, params, shared.clone())?;
    Executor::new(g)
        .with_clock(Clock::Simulated(DateTime::<Utc>::UNIX_EPOCH))
        .with_tick(Duration::ZERO)
        .run()
        .map_err(|e| SimError::Executor(e.to_string()))?;
    let summary = shared
        .lock()
        .map_err(|_| SimError::Executor("account lock poisoned".into()))?
        .summary();
    Ok(SimResult {
        params: *params,
        account: summary,
    })
}

/// The account action: trades the decisions; tells the decision when the
/// stop-loss or a liquidation closed the position.
struct SimAccount {
    account: Arc<Mutex<Account>>,
    decision: ModifierHandle<DecisionModifier>,
}

impl Action for SimAccount {
    type Value = DecisionEvent;
    type Input = Candle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, DecisionEvent, Candle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let mut account = self
            .account
            .lock()
            .map_err(|_| ActionError::Failed("account lock poisoned".into()))?;
        let c = event.data;
        match event.value.kind {
            DecisionKind::Tick => {
                if account.candle(c) {
                    ctx.send(&self.decision, DecisionModifier::Closed);
                }
            }
            DecisionKind::Open(side) => {
                if !account.open(side, c) {
                    // Not opened (no equity left): the decision is flat again.
                    ctx.send(&self.decision, DecisionModifier::Closed);
                }
            }
            DecisionKind::Exit(_) => account.exit(c),
        }
        Ok(())
    }
}

fn divergence(r: &DivergenceGenes, timeframe: Timeframe) -> Result<DivergenceVoter, SimError> {
    let params = DivergenceParams {
        left: PIVOT_SIDE,
        right: PIVOT_SIDE,
        min_gap: timeframe.duration() * 2,
        rsi_low: r.rsi_low,
        rsi_high: r.rsi_high,
        lookahead: r.lookahead,
    };
    DivergenceVoter::new(params, r.exit_rsi_high, r.exit_rsi_low).map_err(invalid)
}

fn send<I>(rule: &str, w: Weights, to: ModifierHandle<DecisionModifier>) -> SendVotes<I> {
    SendVotes::new(rule, w, to)
}

/// Wire the agent of `p` on `src` (the decision first: it sees each candle
/// before the votes of that candle).
fn build(
    g: &mut Graph,
    src: &trading::graph::Output<Candle>,
    p: &AgentParams,
    account: Arc<Mutex<Account>>,
) -> Result<(), SimError> {
    let decision = g.condition(
        "decision",
        Decision::<Candle>::new(p.decision).map_err(invalid)?,
        src,
    );
    let to = g.modifier_handle(&decision);
    g.action(
        SimAccount {
            account,
            decision: to,
        },
        &decision,
    );

    let rsi = g.transform(Rsi::new(RSI_PERIOD).map_err(invalid)?, src);
    let v = g.condition("div-15m", divergence(&p.div15_rule, Timeframe::M15)?, &rsi);
    g.action(send("div-15m", p.div15, to), &v);

    let h1 = g.transform(
        Aggregate::new(Timeframe::M15, Timeframe::H1).map_err(invalid)?,
        src,
    );
    let h1_rsi = g.transform(OnValue::new(Rsi::new(RSI_PERIOD).map_err(invalid)?), &h1);
    let voter = Projected::new(
        divergence(&p.div1h_rule, Timeframe::H1)?,
        aggregated::<Decimal>,
    );
    let v = g.condition("div-1h", voter, &h1_rsi);
    g.action(send("div-1h", p.div1h, to), &v);

    let emas = g.transform(
        Combine::new((
            Ema::new(p.ema_fast).map_err(invalid)?,
            Ema::new(p.ema_slow).map_err(invalid)?,
        )),
        src,
    );
    let v = g.condition("ema", EmaCrossVoter::default(), &emas);
    g.action(send("ema", p.ema, to), &v);

    let trend = g.transform(Trend::<Candle>::new(p.trend_period).map_err(invalid)?, src);
    let v = g.condition("trend", TrendVoter::default(), &trend);
    g.action(send("trend", p.trend, to), &v);

    let patterns =
        PatternVoter::new(PatternCondition::new(ThresholdSet::default()).map_err(invalid)?);
    let v = g.condition("patterns", patterns, src);
    g.action(send("patterns", p.patterns, to), &v);
    Ok(())
}
