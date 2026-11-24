//! Modifiers: information sent (mostly by actions) to a condition or a
//! transformer to change how it operates (spec F5).
//!
//! Modifiers are addressed with a typed [`ModifierHandle`], obtained from the
//! [`Graph`](crate::graph::Graph) builder. They are applied by the executor at
//! the end of the tick in which they are produced (spec §5.5).

use std::any::Any;
use std::fmt;
use std::marker::PhantomData;

use tokio::sync::mpsc;

use crate::error::ModifierError;

/// Information sent to a condition or a transformer. Its meaning is specific
/// to the receiver.
pub trait ModifierValue: fmt::Debug + Send + Sync + 'static {}

/// Modifier type for nodes that accept no modifier (it cannot be built).
#[derive(Debug)]
pub enum NoModifier {}

impl ModifierValue for NoModifier {}

/// Typed address of a node that accepts modifiers of type `M`.
pub struct ModifierHandle<M> {
    pub(crate) node: usize,
    pub(crate) modifier: PhantomData<fn(M)>,
}

impl<M> Clone for ModifierHandle<M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> Copy for ModifierHandle<M> {}

impl<M> fmt::Debug for ModifierHandle<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ModifierHandle(node {})", self.node)
    }
}

/// A type-erased modifier addressed to a node.
pub(crate) type Envelope = (usize, Box<dyn Any + Send>);

/// Sends a modifier from a background task (e.g. started by an action for
/// network work). It is applied at the end of the first tick after it arrives.
pub struct DeferredModifier<M> {
    node: usize,
    tx: mpsc::UnboundedSender<Envelope>,
    modifier: PhantomData<fn(M)>,
}

impl<M> Clone for DeferredModifier<M> {
    fn clone(&self) -> Self {
        Self {
            node: self.node,
            tx: self.tx.clone(),
            modifier: PhantomData,
        }
    }
}

impl<M: ModifierValue> DeferredModifier<M> {
    pub(crate) fn new(handle: &ModifierHandle<M>, tx: mpsc::UnboundedSender<Envelope>) -> Self {
        Self {
            node: handle.node,
            tx,
            modifier: PhantomData,
        }
    }

    /// Send the modifier to the executor.
    pub fn send(&self, modifier: M) -> Result<(), ModifierError> {
        self.tx
            .send((self.node, Box::new(modifier)))
            .map_err(|_| ModifierError::TargetStopped)
    }
}
