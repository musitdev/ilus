//! ilus — a market-monitoring bot.
//!
//! The bot is a pipeline of building blocks (see `SPECS.md`):
//!
//! ```text
//! Source --> [Transformer]* --> Condition --trigger event--> Action
//!                                   ^                          |
//!                                   +-------- modifier --------+
//! ```
//!
//! - [`source`]: where data comes from ([`source::Source`], [`source::SourceStream`]).
//!   Each source has one channel and exactly one consumer.
//! - [`candle`]: the OHLCV data item delivered by provider sources.
//! - [`transformer`]: turns one source into another ([`transformer::Transformer`],
//!   [`transformer::Transformed`]).
//! - [`condition`]: detects situations on a source stream ([`condition::Condition`]).
//! - [`trigger`]: events emitted by conditions ([`trigger::TriggerEvent`]).
//! - [`action`]: reacts to trigger events ([`action::Action`]).
//! - [`modifier`]: information sent back to a condition ([`modifier::ModifierSender`]).
//! - [`rule`]: binds a source, a condition and an action ([`rule::Rule`]).
//! - [`bot`]: wires everything together and runs it ([`bot::Bot`]).
//!
//! Everything is statically typed and wired in code: no configuration file.

pub mod action;
pub mod bot;
pub mod candle;
pub mod condition;
pub mod error;
pub mod modifier;
pub mod rule;
pub mod source;
pub mod transformer;
pub mod trigger;
