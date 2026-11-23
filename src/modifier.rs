//! Modifiers: information sent to a condition to change how it operates (spec F5).

use std::fmt;

use tokio::sync::mpsc;

use crate::error::ModifierError;

/// Information sent to a condition. Its meaning is specific to the condition.
pub trait ModifierValue: fmt::Debug + Send + Sync + 'static {}

/// Modifier type for conditions that accept no modifier (it cannot be built).
#[derive(Debug)]
pub enum NoModifier {}

impl ModifierValue for NoModifier {}

/// Messages from the action side to the rule. Modifiers and the "action done"
/// notification (used to count queued events) share one channel, so they are
/// handled in the order they were sent.
pub(crate) enum Control<M> {
    Modifier(M),
    ActionDone,
}

/// Handle to send modifiers to a condition (given to the rule's action) or to
/// a transformer ([`Transformed::modifier_sender`](crate::transformer::Transformed::modifier_sender)).
/// Cloning it gives another handle to the same target.
pub struct ModifierSender<M> {
    tx: mpsc::UnboundedSender<Control<M>>,
}

impl<M> Clone for ModifierSender<M> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<M: ModifierValue> ModifierSender<M> {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Control<M>>) -> Self {
        Self { tx }
    }

    /// Send a modifier. It is applied by the target between two data items,
    /// in send order.
    pub fn send(&self, modifier: M) -> Result<(), ModifierError> {
        self.tx
            .send(Control::Modifier(modifier))
            .map_err(|_| ModifierError::TargetStopped)
    }

    pub(crate) fn action_done(&self) -> Result<(), ModifierError> {
        self.tx
            .send(Control::ActionDone)
            .map_err(|_| ModifierError::TargetStopped)
    }
}
