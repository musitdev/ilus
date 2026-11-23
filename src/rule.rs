//! Rules: bind a source, a condition and an action.

use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::action::{Action, EventOf};
use crate::condition::Condition;
use crate::error::Error;
use crate::modifier::{Control, ModifierSender};
use crate::source::{SourceData, SourceStream};
use crate::trigger::TriggerEvent;

/// Default maximum number of events waiting for the action.
pub const DEFAULT_MAX_PENDING: usize = 64;

/// A source → condition → action binding.
pub struct Rule<C: Condition, A: Action<C>> {
    name: String,
    input: SourceStream<C::Input>,
    condition: C,
    action: A,
    max_pending: usize,
}

impl<C: Condition, A: Action<C>> Rule<C, A> {
    /// Create a rule on `input` (taken with a source's `get_source_stream`).
    pub fn new(
        name: impl Into<String>,
        input: SourceStream<C::Input>,
        condition: C,
        action: A,
    ) -> Self {
        Self {
            name: name.into(),
            input,
            condition,
            action,
            max_pending: DEFAULT_MAX_PENDING,
        }
    }

    /// Maximum number of events waiting for the action (at least 1).
    /// When reached, the rule stops reading its input until the queue is empty.
    pub fn with_max_pending(mut self, max_pending: usize) -> Self {
        self.max_pending = max_pending.max(1);
        self
    }

    /// Name of the rule.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Run the rule until its source ends and every queued event is processed.
    ///
    /// - The condition sees every data item, in order.
    /// - **No event is lost**: every detection is queued and processed by the
    ///   action, in order, one at a time, in its own task.
    /// - **Backpressure**: when `max_pending` events are waiting, the rule stops
    ///   reading its input until the queue is empty. The bounded source channel
    ///   then fills up and the source is slowed down.
    /// - Modifiers are applied between data items, in send order. They affect
    ///   the next data items, not events already queued.
    pub(crate) async fn run(self) -> Result<(), Error> {
        let Self {
            name,
            mut input,
            mut condition,
            mut action,
            max_pending,
        } = self;

        let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<Control<C::Modifier>>();
        // Unbounded, but the rule never queues more than `max_pending` events.
        let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<EventOf<C>>();
        let modifiers = ModifierSender::new(ctl_tx);

        let action_rule = name.clone();
        let action_task = tokio::spawn(async move {
            while let Some(event) = evt_rx.recv().await {
                if let Err(e) = action.run(event, &modifiers).await {
                    eprintln!("[{action_rule}] {e}");
                }
                if modifiers.action_done().is_err() {
                    break;
                }
            }
        });

        let source = input.info().clone();
        let mut evt_tx = Some(evt_tx);
        // Events sent to the action and not finished yet.
        let mut pending = 0usize;
        // Input reading suspended until the queue is empty.
        let mut paused = false;
        loop {
            tokio::select! {
                biased;
                Some(ctl) = ctl_rx.recv() => match ctl {
                    Control::Modifier(m) => {
                        if let Err(e) = condition.on_modifier(m) {
                            eprintln!("[{name}] {e}");
                        }
                    }
                    Control::ActionDone => {
                        pending = pending.saturating_sub(1);
                        if pending == 0 {
                            paused = false;
                        }
                    }
                },
                item = input.next(), if evt_tx.is_some() && !paused => match item {
                    Some(item) => {
                        if let (Some(detection), Some(tx)) = (condition.on_data(&item), &evt_tx) {
                            let event = TriggerEvent {
                                rule: name.clone(),
                                event_type: detection.event_type,
                                value: detection.value,
                                source: source.clone(),
                                time: item.time(),
                                data: item,
                            };
                            if tx.send(event).is_err() {
                                evt_tx = None; // action task stopped
                            } else {
                                pending += 1;
                                paused = pending >= max_pending;
                            }
                        }
                    }
                    // Source ended: let the action task finish the queued events.
                    None => evt_tx = None,
                },
                else => break,
            }
        }

        action_task.await.map_err(|e| Error::Rule {
            rule: name,
            reason: e.to_string(),
        })
    }
}
