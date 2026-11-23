//! Conditions: detect situations on their input (spec F2).

pub mod divergence;

use crate::error::ModifierError;
use crate::modifier::ModifierValue;
use crate::source::{Item, SourceData};
use crate::trigger::{Detection, TriggerValue};

/// A sync detector with exactly one input in the graph.
///
/// It is called for every new input item, including bootstrap items (to warm
/// up its state); detections of bootstrap items are not sent to actions.
pub trait Condition: Send + 'static {
    /// Type of data item consumed.
    type Input: SourceData;
    /// Type of value emitted when it triggers.
    type Value: TriggerValue;
    /// Type of modifier accepted. Use [`NoModifier`](crate::modifier::NoModifier) if none.
    type Modifier: ModifierValue;

    /// Items needed before the first possible detection (used to compute the
    /// bootstrap length, spec §5.4).
    fn warm_up(&self) -> usize;

    /// Called for each new input item.
    fn on_data(&mut self, item: Item<'_, Self::Input>) -> Option<Detection<Self::Value>>;

    /// Called at the end of a tick for each modifier sent to it.
    /// An invalid modifier must be rejected and leave the condition unchanged.
    fn on_modifier(&mut self, modifier: Self::Modifier) -> Result<(), ModifierError>;
}
