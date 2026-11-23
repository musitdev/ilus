//! Trigger events: what a condition emits when it detects something.

use std::fmt;

use chrono::{DateTime, Utc};

use crate::source::{Mode, SourceInfo};

/// Value produced by a condition when it triggers. Its meaning is specific
/// to the condition algorithm (e.g. a buy/sell signal), not to the source data.
pub trait TriggerValue: fmt::Debug + Send + Sync + 'static {}

/// What was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerEventType {
    /// Bullish engulfing pattern.
    BullishEngulfing,
    /// Bearish engulfing pattern.
    BearishEngulfing,
    /// Hammer pattern.
    Hammer,
    /// Hanging man pattern.
    HangingMan,
    /// Inverted hammer pattern.
    InvertedHammer,
    /// Shooting star pattern.
    ShootingStar,
    /// Doji pattern.
    Doji,
    /// Morning star pattern.
    MorningStar,
    /// Evening star pattern.
    EveningStar,
    /// Bullish harami pattern.
    BullishHarami,
    /// Bearish harami pattern.
    BearishHarami,
    /// Piercing line pattern.
    PiercingLine,
    /// Dark cloud cover pattern.
    DarkCloudCover,
    /// Price lower low with RSI higher low (spec F6).
    BullishDivergence,
    /// Price higher high with RSI lower high (spec F6).
    BearishDivergence,
    /// Any other kind of detection, identified by name.
    Other(&'static str),
}

/// What a condition returns when it detects something. The rule turns it into
/// a full [`TriggerEvent`] by adding the context (rule, source, time, data).
#[derive(Debug, Clone, PartialEq)]
pub struct Detection<V> {
    /// What was detected.
    pub event_type: TriggerEventType,
    /// Condition-specific value.
    pub value: V,
}

/// Event given to an action when its condition triggers (never for bootstrap
/// items). The action is awaited within the tick, so the event borrows the
/// detection value and the triggering item.
#[derive(Debug)]
pub struct TriggerEvent<'a, V, D> {
    /// Name of the condition node.
    pub condition: &'a str,
    /// What was detected.
    pub event_type: TriggerEventType,
    /// Condition-specific value (e.g. buy signal).
    pub value: &'a V,
    /// Where the data came from (the condition input).
    pub source: &'a SourceInfo,
    /// Time of the triggering item.
    pub time: DateTime<Utc>,
    /// Running mode of the triggering item (replay or normal).
    pub mode: Mode,
    /// The triggering item (the condition input).
    pub data: &'a D,
}
