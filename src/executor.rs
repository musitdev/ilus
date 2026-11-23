//! The executor: runs a [`Graph`] tick by tick (spec §5.4, §5.5).
//!
//! A tick is fully sequential:
//! 1. call the due sources (each returns zero or one item);
//! 2. evaluate the downstream transformers and conditions, in graph order;
//! 3. run the actions of the conditions that triggered (except for bootstrap
//!    items), in graph order, awaiting each one;
//! 4. apply the modifiers produced during the tick, in send order;
//! 5. wait for the next tick.
//!
//! While a source is in bootstrap or replay mode, ticks are global and run
//! every [`DEFAULT_TICK`] (configurable). Otherwise the executor sleeps until
//! the next normal-mode call asked by a source. It stops when every source
//! has ended.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::mpsc;

use crate::action::ActionContext;
use crate::graph::{ActionInput, Graph, NodeKind, NodePoll, Slot};
use crate::modifier::Envelope;
use crate::source::Mode;

/// Default global tick in bootstrap and replay modes.
pub const DEFAULT_TICK: Duration = Duration::from_millis(10);

/// Time used by the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    /// Wall-clock time; the executor really waits between ticks.
    System,
    /// Simulated time starting at the given instant: waiting only advances
    /// it (no real wait). For tests and fast simulations.
    Simulated(DateTime<Utc>),
}

impl Clock {
    /// Current time.
    pub fn now(&self) -> DateTime<Utc> {
        match self {
            Self::System => Utc::now(),
            Self::Simulated(t) => *t,
        }
    }

    async fn sleep(&mut self, d: Duration) {
        match self {
            Self::System if d.is_zero() => tokio::task::yield_now().await,
            Self::System => tokio::time::sleep(d).await,
            Self::Simulated(t) => {
                *t += TimeDelta::from_std(d).unwrap_or_default();
                tokio::task::yield_now().await;
            }
        }
    }

    async fn sleep_until(&mut self, at: DateTime<Utc>) {
        let d = (at - self.now()).to_std().unwrap_or_default();
        self.sleep(d).await;
    }
}

/// Runs a graph.
pub struct Executor {
    graph: Graph,
    tick: Duration,
    clock: Clock,
}

impl Executor {
    /// Prepare `graph` for execution: computes the bootstrap length of every
    /// source from the warm-ups of the nodes downstream of it (spec §5.4).
    pub fn new(mut graph: Graph) -> Self {
        let n = graph.nodes.len();
        let mut cum = vec![0usize; n];
        let mut root = vec![0usize; n];
        let mut need = vec![0usize; n];
        for i in 0..n {
            match &graph.nodes[i].kind {
                NodeKind::Source(_) => root[i] = i,
                NodeKind::Proc { input, node } => {
                    let input = (*input).min(i.saturating_sub(1));
                    cum[i] = cum[input] + node.warm_up();
                    root[i] = root[input];
                    need[root[i]] = need[root[i]].max(cum[i]);
                }
            }
        }
        for (i, node) in graph.nodes.iter_mut().enumerate() {
            if let NodeKind::Source(source) = &mut node.kind {
                source.set_bootstrap(need[i]);
            }
        }
        Self {
            graph,
            tick: DEFAULT_TICK,
            clock: Clock::System,
        }
    }

    /// Global tick in bootstrap and replay modes (zero: as fast as possible).
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Use another clock (e.g. [`Clock::Simulated`] in tests).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Run until every source has ended.
    pub async fn run(self) {
        let Self {
            graph,
            tick,
            mut clock,
        } = self;
        let Graph {
            mut nodes,
            mut actions,
        } = graph;
        let n = nodes.len();
        let (deferred_tx, mut deferred_rx) = mpsc::unbounded_channel::<Envelope>();
        let mut slots: Vec<Option<Slot>> = (0..n).map(|_| None).collect();
        let mut ended = vec![false; n];
        let mut next_due: Vec<Option<DateTime<Utc>>> = vec![None; n];

        loop {
            let now = clock.now();
            slots.iter_mut().for_each(|s| *s = None);

            // 1. Sources.
            let mut active = false;
            let mut fast = false;
            for i in 0..n {
                let NodeKind::Source(source) = &mut nodes[i].kind else {
                    continue;
                };
                if ended[i] {
                    continue;
                }
                let due = source.mode() != Mode::Normal || next_due[i].is_none_or(|t| t <= now);
                if due {
                    match source.poll(now) {
                        NodePoll::Ready(slot) => slots[i] = Some(slot),
                        NodePoll::Pending => {}
                        NodePoll::Ended => ended[i] = true,
                    }
                    next_due[i] = Some(source.next_call(now));
                }
                if !ended[i] {
                    active = true;
                    fast |= source.mode() != Mode::Normal;
                }
            }

            // 2. Transformers and conditions, in graph order.
            for i in 0..n {
                let NodeKind::Proc { input, node } = &mut nodes[i].kind else {
                    continue;
                };
                let Some(Some(input)) = slots.get(*input) else {
                    continue;
                };
                let (mode, time) = (input.mode, input.time);
                let output = node.process(&*input.data, mode);
                slots[i] = output.map(|data| Slot { data, mode, time });
            }

            // 3. Actions of the conditions that triggered (not for bootstrap items).
            let mut ctx = ActionContext::new(deferred_tx.clone(), now);
            for action in actions.iter_mut() {
                let c = action.condition();
                let Some(Some(detection)) = slots.get(c) else {
                    continue;
                };
                if detection.mode == Mode::Bootstrap {
                    continue;
                }
                let Some(node) = nodes.get(c) else { continue };
                let NodeKind::Proc { input, .. } = &node.kind else {
                    continue;
                };
                let Some(Some(data)) = slots.get(*input) else {
                    continue;
                };
                let input = ActionInput {
                    detection: &*detection.data,
                    data: &*data.data,
                    condition: &node.name,
                    source: &node.info,
                    time: detection.time,
                    mode: detection.mode,
                };
                if let Err(e) = action.run(input, &mut ctx).await {
                    eprintln!("[{}] {e}", node.name);
                }
            }

            // 4. Modifiers of this tick, then those from background tasks.
            let mut modifiers = std::mem::take(&mut ctx.modifiers);
            while let Ok(m) = deferred_rx.try_recv() {
                modifiers.push(m);
            }
            for (target, modifier) in modifiers {
                let Some(node) = nodes.get_mut(target) else {
                    continue;
                };
                if let NodeKind::Proc {
                    node: proc_node, ..
                } = &mut node.kind
                    && let Err(e) = proc_node.apply(modifier)
                {
                    eprintln!("[{}] {e}", node.name);
                }
            }

            if !active {
                break;
            }

            // 5. Wait for the next tick.
            if fast {
                clock.sleep(tick).await;
            } else {
                let next = (0..n)
                    .filter(|&i| !ended[i])
                    .filter_map(|i| next_due[i])
                    .min()
                    .unwrap_or(now);
                clock.sleep_until(next).await;
            }
        }
    }
}
