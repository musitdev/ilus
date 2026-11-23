//! Transformers: turn one source into another (spec F4).
//!
//! A transformer **always forwards its input data**: for every input item, the
//! transformed source emits a [`Derived`] item holding the input item and the
//! computed value (`None` while the transformer warms up).

#[cfg(feature = "chart")]
pub mod chart;
pub mod rsi;

use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio_stream::StreamExt;

use tokio::sync::mpsc;

use crate::error::{ModifierError, SourceError};
use crate::modifier::{Control, ModifierSender, ModifierValue};
use crate::source::{Source, SourceChannel, SourceData, SourceInfo, SourceStream};

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

/// Item emitted by a transformed source: the input item plus the computed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived<D, V> {
    /// The input item, forwarded unchanged.
    pub data: D,
    /// The computed value; `None` while the transformer warms up.
    pub value: Option<V>,
}

impl<D: SourceData, V: fmt::Debug + Send + Sync + 'static> SourceData for Derived<D, V> {
    fn time(&self) -> DateTime<Utc> {
        self.data.time()
    }
}

/// Computes a value from a stream of `Input` items.
pub trait Transformer: Send + 'static {
    /// Type of data item consumed.
    type Input: SourceData;
    /// Type of value computed for each input item.
    type Value: fmt::Debug + Send + Sync + 'static;
    /// Type of modifier accepted. Use [`NoModifier`](crate::modifier::NoModifier) if none.
    type Modifier: ModifierValue;

    /// Short description used in [`SourceInfo`], e.g. `rsi(14)`.
    fn name(&self) -> String;

    /// Number of input items needed before the first value.
    fn warm_up(&self) -> usize;

    /// Called for each closed input item, in order.
    /// Returns the computed value, or `None` while warming up.
    fn on_data(&mut self, item: &Self::Input) -> Option<Self::Value>;

    /// Called when a modifier is received, between two input items.
    /// An invalid modifier must be rejected and leave the transformer unchanged.
    fn on_modifier(&mut self, modifier: Self::Modifier) -> Result<(), ModifierError>;
}

/// Data type of the source produced by transformer `T`.
pub type OutputOf<T> = Derived<<T as Transformer>::Input, <T as Transformer>::Value>;

/// A source made of a transformer applied to the stream of another source.
pub struct Transformed<T: Transformer> {
    transformer: T,
    input: SourceStream<T::Input>,
    channel: SourceChannel<OutputOf<T>>,
    modifiers: ModifierSender<T::Modifier>,
    modifier_rx: mpsc::UnboundedReceiver<Control<T::Modifier>>,
}

impl<T: Transformer> Transformed<T> {
    /// Apply `transformer` to `input` (taken with another source's `get_source_stream`).
    pub fn new(transformer: T, input: SourceStream<T::Input>) -> Self {
        let info = SourceInfo::Transformed {
            input: Box::new(input.info().clone()),
            transformer: transformer.name(),
        };
        let (tx, modifier_rx) = mpsc::unbounded_channel();
        Self {
            transformer,
            input,
            channel: SourceChannel::new(info),
            modifiers: ModifierSender::new(tx),
            modifier_rx,
        }
    }

    /// Handle to send modifiers to this transformer (e.g. from an action).
    ///
    /// After its input has ended, the transformer keeps applying modifiers
    /// until every handle is dropped: do not keep handles alive needlessly.
    pub fn modifier_sender(&self) -> ModifierSender<T::Modifier> {
        self.modifiers.clone()
    }
}

impl<T: Transformer> Source for Transformed<T> {
    type Data = OutputOf<T>;

    fn channel(&mut self) -> &mut SourceChannel<OutputOf<T>> {
        &mut self.channel
    }

    async fn run(self) -> Result<(), SourceError> {
        let Self {
            mut transformer,
            mut input,
            channel,
            modifiers,
            mut modifier_rx,
        } = self;
        // Our own handle must not keep the modifier channel open.
        drop(modifiers);
        let sender = channel.into_sender();
        let info = sender.info().clone();
        let apply = |transformer: &mut T, ctl: Control<T::Modifier>| {
            if let Control::Modifier(m) = ctl
                && let Err(e) = transformer.on_modifier(m)
            {
                eprintln!("[{info}] {e}");
            }
        };
        loop {
            tokio::select! {
                biased;
                // Modifiers are applied between two input items, in send order.
                Some(ctl) = modifier_rx.recv() => apply(&mut transformer, ctl),
                item = input.next() => {
                    let Some(item) = item else { break };
                    let value = transformer.on_data(&item);
                    if !sender.send(Derived { data: item, value }).await {
                        break; // no consumer
                    }
                }
            }
        }
        // Input ended: close the output so that consumers can finish, but keep
        // applying modifiers sent later (e.g. by an action still processing
        // queued events) until every modifier sender is dropped.
        drop(sender);
        while let Some(ctl) = modifier_rx.recv().await {
            apply(&mut transformer, ctl);
        }
        Ok(())
    }
}
