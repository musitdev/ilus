//! Trigger events: what a condition emits when it detects something.

use std::fmt;

use chrono::{DateTime, Utc};

use crate::source::{SourceData, SourceInfo};

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

/// Event sent to an action when a condition triggers.
#[derive(Debug)]
pub struct TriggerEvent<V: TriggerValue, D: SourceData> {
    /// Rule that produced it.
    pub rule: String,
    /// What was detected.
    pub event_type: TriggerEventType,
    /// Condition-specific value (e.g. buy signal).
    pub value: V,
    /// Where the data came from.
    pub source: SourceInfo,
    /// Time of the triggering data item.
    pub time: DateTime<Utc>,
    /// Triggering data item (context only).
    pub data: D,
}
