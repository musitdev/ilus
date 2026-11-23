//! ilus — a market-monitoring bot.
//!
//! Entities are the nodes of a [`graph::Graph`] run tick by tick by an
//! [`executor::Executor`] (see `SPECS.md` §5):
//!
//! ```text
//! Source ──> [Transformer]* ──> Condition ──> Action
//!                 ^                 ^            │
//!                 └─────────────────┴─ modifier ─┘
//! ```
//!
//! - [`source`]: produce closed data items, each tagged with a running
//!   [`source::Mode`] (bootstrap, replay, normal).
//! - [`candle`]: the OHLCV data item of provider sources.
//! - [`transformer`]: sync, turn one input into a new output (always
//!   forwarding the input), e.g. RSI.
//! - [`condition`]: sync, detect situations on one input.
//! - [`trigger`]: detections and trigger events.
//! - [`action`]: react to the detections of one condition (can be async).
//! - [`modifier`]: information sent back to a condition or a transformer.
//! - [`graph`]: typed builder of the graph.
//! - [`executor`]: runs the graph.
//!
//! Everything is wired in code: no configuration file.

pub mod action;
pub mod candle;
pub mod condition;
pub mod error;
pub mod executor;
pub mod graph;
pub mod modifier;
pub mod source;
pub mod transformer;
pub mod trigger;
