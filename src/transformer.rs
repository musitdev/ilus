//! Transformers: turn one input into a new output (spec F4).
//!
//! A transformer **always forwards its input data**: for every input item, its
//! output is a [`Derived`] item holding the input item and the computed value
//! (`None` while the transformer warms up). The output keeps the running mode
//! of its input.

pub mod aggregate;
#[cfg(feature = "chart")]
pub mod chart;
pub mod combine;
pub mod ema;
pub mod rsi;

use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::error::ModifierError;
use crate::modifier::ModifierValue;
use crate::source::{Item, SourceData};

/// Single-value data item (e.g. an indicator value without its candle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndicatorPoint {
    /// Time of the item.
    pub time: DateTime<Utc>,
    /// Value.
    pub value: Decimal,
}

impl SourceData for IndicatorPoint {
    fn time(&self) -> DateTime<Utc> {
        self.time
    }
}

/// Output item of a transformer: the input item plus the computed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived<D, V> {
    /// The input item, forwarded unchanged.
    pub data: D,
    /// The computed value; `None` while the transformer warms up.
    pub value: Option<V>,
}

impl<D: SourceData, V: fmt::Debug + Clone + Send + Sync + 'static> SourceData for Derived<D, V> {
    fn time(&self) -> DateTime<Utc> {
        self.data.time()
    }
}

/// A sync transformer with exactly one input in the graph.
pub trait Transformer: Send + 'static {
    /// Type of data item consumed.
    type Input: SourceData;
    /// Type of value computed for each input item.
    type Value: fmt::Debug + Clone + Send + Sync + 'static;
    /// Type of modifier accepted. Use [`NoModifier`](crate::modifier::NoModifier) if none.
    type Modifier: ModifierValue;

    /// Short description used in [`SourceInfo`](crate::source::SourceInfo), e.g. `rsi(14)`.
    fn name(&self) -> String;

    /// Items needed before the first value (used to compute the bootstrap
    /// length, spec §5.4).
    fn warm_up(&self) -> usize;

    /// Called for each new input item, in order. Returns the computed value,
    /// or `None` while warming up.
    fn on_data(&mut self, item: Item<'_, Self::Input>) -> Option<Self::Value>;

    /// Called at the end of a tick for each modifier sent to it.
    /// An invalid modifier must be rejected and leave the transformer unchanged.
    fn on_modifier(&mut self, modifier: Self::Modifier) -> Result<(), ModifierError>;
}

/// Output type of transformer `T`.
pub type OutputOf<T> = Derived<<T as Transformer>::Input, <T as Transformer>::Value>;
