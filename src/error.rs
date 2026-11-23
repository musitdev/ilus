//! Error types shared by the building blocks.

use thiserror::Error;

/// Error raised by a source while fetching data.
#[derive(Debug, Error)]
pub enum SourceError {
    /// The provider could not be reached or returned an error.
    #[error("provider error: {0}")]
    Provider(String),
    /// The requested asset or timeframe is not available.
    #[error("unknown asset or timeframe: {0}")]
    Unknown(String),
    /// The stream of this source was already taken (one consumer per source).
    #[error("stream of source {0} already taken")]
    StreamTaken(String),
}

/// Invalid parameter given to a building block (e.g. RSI period 0).
#[derive(Debug, Error)]
pub enum ParameterError {
    /// The parameter is invalid.
    #[error("invalid parameter: {0}")]
    Invalid(String),
}

/// Error raised by an action while executing.
#[derive(Debug, Error)]
pub enum ActionError {
    /// The action failed.
    #[error("action failed: {0}")]
    Failed(String),
}

/// Error related to modifiers.
#[derive(Debug, Error)]
pub enum ModifierError {
    /// The condition rejected the modifier; it is left unchanged.
    #[error("modifier rejected: {0}")]
    Rejected(String),
    /// The target condition is no longer running.
    #[error("target condition stopped")]
    TargetStopped,
}

/// Top-level error returned by the bot's tasks.
#[derive(Debug, Error)]
pub enum Error {
    /// A source failed.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// A rule failed (e.g. its action task panicked).
    #[error("rule '{rule}' failed: {reason}")]
    Rule {
        /// Name of the failing rule.
        rule: String,
        /// What went wrong.
        reason: String,
    },
}
