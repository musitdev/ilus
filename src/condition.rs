//! Conditions: detect situations on a source stream (spec F2).

pub mod divergence;

use crate::error::ModifierError;
use crate::modifier::ModifierValue;
use crate::source::SourceData;
use crate::trigger::{Detection, TriggerValue};

/// A detector consuming the data stream of exactly one source.
///
/// A condition receives every data item, in order, and reports what it
/// detects. Every detection is queued by the [`Rule`](crate::rule::Rule) and
/// processed by the action: none is lost.
pub trait Condition: Send + 'static {
    /// Type of data item consumed (must match the rule's source).
    type Input: SourceData;
    /// Type of value emitted when it triggers.
    type Value: TriggerValue;
    /// Type of modifier accepted. Use [`NoModifier`](crate::modifier::NoModifier) if none.
    type Modifier: ModifierValue;

    /// Called for each closed data item, in order.
    fn on_data(&mut self, item: &Self::Input) -> Option<Detection<Self::Value>>;

    /// Called when a modifier is received, between two data items.
    /// An invalid modifier must be rejected and leave the condition unchanged.
    fn on_modifier(&mut self, modifier: Self::Modifier) -> Result<(), ModifierError>;
}
