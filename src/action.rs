//! Actions: react to trigger events (spec F3).

use crate::condition::Condition;
use crate::error::ActionError;
use crate::modifier::ModifierSender;
use crate::trigger::TriggerEvent;

/// Event type received by an action written for condition `C`.
pub type EventOf<C> = TriggerEvent<<C as Condition>::Value, <C as Condition>::Input>;

/// An operation executed when the condition `C` triggers.
///
/// Being generic over the condition, event and modifier types are checked by
/// the compiler.
pub trait Action<C: Condition>: Send + 'static {
    /// Execute the action for one event. Events are processed one at a time,
    /// in detection order. Modifiers sent through `modifiers` are applied to
    /// the condition between two data items; they affect the next items.
    fn run(
        &mut self,
        event: EventOf<C>,
        modifiers: &ModifierSender<C::Modifier>,
    ) -> impl Future<Output = Result<(), ActionError>> + Send;
}

/// Prints the trigger event to the console. Works with any condition.
#[derive(Debug, Clone, Copy, Default)]
pub struct Alert;

impl<C: Condition> Action<C> for Alert {
    async fn run(
        &mut self,
        event: EventOf<C>,
        _modifiers: &ModifierSender<C::Modifier>,
    ) -> Result<(), ActionError> {
        println!(
            "[ALERT] rule={} type={:?} value={:?} source={} time={} data={:?}",
            event.rule, event.event_type, event.value, event.source, event.time, event.data
        );
        Ok(())
    }
}
