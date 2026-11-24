//! Multi-rule agent (spec F9): rules vote, a decision condition adds up the
//! weights and decides.
//!
//! ```text
//! source ─┬─> ... ──> voter ──> SendVotes ──┐  Vote modifiers
//!         ├─> ... ──> voter ──> SendVotes ──┤
//!         └─> Decision <────────────────────┘ ──> account action
//! ```
//!
//! - [`voters`]: one condition per rule, emitting [`Ballots`].
//! - [`SendVotes`]: the action of a rule; sends each ballot to the decision
//!   with the rule's weight.
//! - [`decision::Decision`]: adds up the weights, opens and exits.
//! - [`genome`]: the genome (genes in [0, 1]) and the agent's parameters.
//! - [`account`]: the simulated futures account.
//! - [`simulate`](simulate::simulate): runs the agent of a genome on candles;
//!   the gain in % is the fitness used by the selection.

pub mod account;
pub mod decision;
pub mod genome;
pub mod simulate;
pub mod voters;

use std::marker::PhantomData;

use rust_decimal::Decimal;

use trading::action::{Action, ActionContext};
use trading::error::ActionError;
use trading::modifier::{ModifierHandle, ModifierValue};
use trading::source::SourceData;
use trading::trigger::{TriggerEvent, TriggerValue};

/// Side of a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Bought: gains when the price rises.
    Long,
    /// Sold: gains when the price falls.
    Short,
}

impl Side {
    /// The other side.
    pub fn opposite(self) -> Self {
        match self {
            Self::Long => Self::Short,
            Self::Short => Self::Long,
        }
    }
}

/// What a rule says about the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ballot {
    /// Open a position on this side (replaces the rule's open ballot).
    Open(Side),
    /// The rule's open ballot no longer holds.
    Revoke,
    /// Close the position on this side (replaces the rule's exit ballot).
    Exit(Side),
}

/// Value of a voter detection: the ballots of one candle, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ballots(pub Vec<Ballot>);

impl TriggerValue for Ballots {}

/// A ballot with the rule and its weight, sent to the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vote {
    /// Name of the rule (one open and one exit ballot are kept per rule).
    pub rule: String,
    /// The ballot.
    pub ballot: Ballot,
    /// Its weight (0 for `Revoke`).
    pub weight: Decimal,
}

/// Modifiers accepted by the [`decision::Decision`] condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionModifier {
    /// A rule's ballot.
    Vote(Vote),
    /// The account closed the position by itself (stop-loss, liquidation).
    Closed,
}

impl ModifierValue for DecisionModifier {}

/// Weights of a rule (genes). 0 disables that part of the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    /// Weight of the open ballots.
    pub open: Decimal,
    /// Weight of the exit ballots.
    pub exit: Decimal,
}

/// Action of a rule: sends each ballot of its voter to the decision.
pub struct SendVotes<I> {
    rule: String,
    weights: Weights,
    to: ModifierHandle<DecisionModifier>,
    input: PhantomData<fn(I)>,
}

impl<I> SendVotes<I> {
    /// Votes of `rule`, with `weights`, sent to the decision `to`.
    pub fn new(
        rule: impl Into<String>,
        weights: Weights,
        to: ModifierHandle<DecisionModifier>,
    ) -> Self {
        Self {
            rule: rule.into(),
            weights,
            to,
            input: PhantomData,
        }
    }

    /// The votes for `ballots`, in order.
    pub fn votes(&self, ballots: &Ballots) -> Vec<Vote> {
        ballots
            .0
            .iter()
            .map(|&ballot| Vote {
                rule: self.rule.clone(),
                ballot,
                weight: match ballot {
                    Ballot::Open(_) => self.weights.open,
                    Ballot::Exit(_) => self.weights.exit,
                    Ballot::Revoke => Decimal::ZERO,
                },
            })
            .collect()
    }
}

impl<I: SourceData> Action for SendVotes<I> {
    type Value = Ballots;
    type Input = I;

    fn run(
        &mut self,
        event: TriggerEvent<'_, Ballots, I>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        for vote in self.votes(event.value) {
            ctx.send(&self.to, DecisionModifier::Vote(vote));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// A handle on a decision node (from a graph: handles are not built by hand).
    fn handle() -> ModifierHandle<DecisionModifier> {
        use trading::source::Timeframe;
        use trading::source::mock::MockSource;
        let mut g = trading::graph::Graph::new();
        let src = g.source(MockSource::<trading::candle::Candle>::replay(
            "T",
            Timeframe::M1,
            Vec::new(),
        ));
        let params = crate::decision::DecisionParams {
            open_threshold: dec!(1),
            exit_threshold: dec!(1),
        };
        let decision = g.condition(
            "decision",
            crate::decision::Decision::new(params).unwrap(),
            &src,
        );
        g.modifier_handle(&decision)
    }

    #[test]
    fn one_vote_per_ballot_with_the_rule_weight() {
        let send = SendVotes::<trading::candle::Candle>::new(
            "ema",
            Weights {
                open: dec!(2),
                exit: dec!(1.5),
            },
            handle(),
        );
        let ballots = Ballots(vec![
            Ballot::Open(Side::Long),
            Ballot::Exit(Side::Short),
            Ballot::Revoke,
        ]);
        let weights: Vec<(Ballot, Decimal)> = send
            .votes(&ballots)
            .into_iter()
            .map(|v| {
                assert_eq!(v.rule, "ema");
                (v.ballot, v.weight)
            })
            .collect();
        assert_eq!(
            weights,
            vec![
                (Ballot::Open(Side::Long), dec!(2)),
                (Ballot::Exit(Side::Short), dec!(1.5)),
                (Ballot::Revoke, dec!(0)),
            ]
        );
        assert_eq!(Side::Long.opposite(), Side::Short);
    }
}
