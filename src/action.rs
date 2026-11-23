//! Actions: react to the detections of their condition (spec F3).

use std::any::Any;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::error::ActionError;
use crate::modifier::{DeferredModifier, Envelope, ModifierHandle, ModifierValue};
use crate::source::SourceData;
use crate::trigger::{TriggerEvent, TriggerValue};

/// An operation run when its condition triggers.
///
/// An action is connected to **exactly one** condition; if two conditions need
/// the same behaviour, each gets its own instance. An action can keep state.
/// It is awaited within the tick: for network work, start a task (copying what
/// it needs) and return at once.
pub trait Action: Send + 'static {
    /// Value type of the condition it is connected to.
    type Value: TriggerValue;
    /// Input data type of that condition (the triggering item).
    type Input: SourceData;

    /// Handle one event. Modifiers sent through `ctx` are applied at the end
    /// of the tick.
    fn run(
        &mut self,
        event: TriggerEvent<'_, Self::Value, Self::Input>,
        ctx: &mut ActionContext,
    ) -> impl Future<Output = Result<(), ActionError>> + Send;
}

/// Lets an action send modifiers to conditions and transformers.
pub struct ActionContext {
    pub(crate) modifiers: Vec<Envelope>,
    pub(crate) deferred: mpsc::UnboundedSender<Envelope>,
    now: DateTime<Utc>,
}

impl ActionContext {
    pub(crate) fn new(deferred: mpsc::UnboundedSender<Envelope>, now: DateTime<Utc>) -> Self {
        Self {
            modifiers: Vec::new(),
            deferred,
            now,
        }
    }

    /// Executor time of the current tick.
    pub fn now(&self) -> DateTime<Utc> {
        self.now
    }

    /// Send a modifier; it is applied at the end of this tick, in send order.
    pub fn send<M: ModifierValue>(&mut self, to: &ModifierHandle<M>, modifier: M) {
        self.modifiers
            .push((to.node, Box::new(modifier) as Box<dyn Any + Send>));
    }

    /// A sender usable from a background task (e.g. network work started by
    /// the action). Its modifiers are applied at the end of the first tick
    /// after they arrive.
    pub fn deferred<M: ModifierValue>(&self, to: &ModifierHandle<M>) -> DeferredModifier<M> {
        DeferredModifier::new(to, self.deferred.clone())
    }
}

/// Prints the trigger event. Works with any condition (one instance per condition).
#[derive(Debug)]
pub struct Alert<V, D> {
    value: std::marker::PhantomData<fn(V, D)>,
}

impl<V, D> Alert<V, D> {
    /// A new alert action.
    pub fn new() -> Self {
        Self {
            value: std::marker::PhantomData,
        }
    }
}

impl<V, D> Default for Alert<V, D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: TriggerValue, D: SourceData> Action for Alert<V, D> {
    type Value = V;
    type Input = D;

    async fn run(
        &mut self,
        event: TriggerEvent<'_, V, D>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        println!(
            "[ALERT] condition={} type={:?} value={:?} source={} time={} mode={:?} data={:?}",
            event.condition,
            event.event_type,
            event.value,
            event.source,
            event.time,
            event.mode,
            event.data
        );
        Ok(())
    }
}
