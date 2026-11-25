//! Decision condition (spec F9): adds up the rules' weighted ballots and
//! decides to open or exit a position.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::{Ballot, DecisionModifier, Side, Vote};
use trading::condition::Condition;
use trading::error::{ModifierError, ParameterError};
use trading::source::{Item, SourceData};
use trading::trigger::{Detection, TriggerEventType, TriggerValue};

/// Thresholds of the decision (genes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionParams {
    /// Net open score (long − short, or short − long) needed to open.
    pub open_threshold: Decimal,
    /// Sum of the exit weights against the position needed to exit.
    pub exit_threshold: Decimal,
}

/// What the decision did on this event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    /// A new candle, no decision (sent for every candle).
    Tick,
    /// Open a position on this side.
    Open(Side),
    /// Close the position on this side.
    Exit(Side),
}

/// Value of a decision detection, with the scores it was taken on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionEvent {
    /// The decision.
    pub kind: DecisionKind,
    /// Sum of the open-long weights.
    pub long: Decimal,
    /// Sum of the open-short weights.
    pub short: Decimal,
    /// Sum of the exit weights against the position.
    pub exit: Decimal,
}

impl TriggerValue for DecisionEvent {}

/// The decision condition. Input: the candles (any item) of the trading
/// timeframe; modifiers: [`DecisionModifier`].
pub struct Decision<I> {
    params: DecisionParams,
    position: Option<Side>,
    /// Per rule: its open ballot and weight.
    open: BTreeMap<String, (Side, Decimal)>,
    /// Per rule: its exit ballot and weight.
    exit: BTreeMap<String, (Side, Decimal)>,
    /// Time of the current candle.
    current: Option<DateTime<Utc>>,
    /// Candle of the last decision.
    decided: Option<DateTime<Utc>>,
    input: PhantomData<fn(I)>,
}

impl<I> Decision<I> {
    /// Decision with `params`; both thresholds must be > 0.
    pub fn new(params: DecisionParams) -> Result<Self, ParameterError> {
        if params.open_threshold <= Decimal::ZERO || params.exit_threshold <= Decimal::ZERO {
            return Err(ParameterError::Invalid(
                "decision: thresholds must be > 0".into(),
            ));
        }
        Ok(Self {
            params,
            position: None,
            open: BTreeMap::new(),
            exit: BTreeMap::new(),
            current: None,
            decided: None,
            input: PhantomData,
        })
    }

    /// Position the decision believes is open.
    pub fn position(&self) -> Option<Side> {
        self.position
    }

    fn open_score(&self, side: Side) -> Decimal {
        self.open
            .values()
            .filter(|(s, _)| *s == side)
            .map(|(_, w)| *w)
            .sum()
    }

    fn exit_score(&self) -> Decimal {
        self.position.map_or(Decimal::ZERO, |p| {
            self.exit
                .values()
                .filter(|(s, _)| *s == p)
                .map(|(_, w)| *w)
                .sum()
        })
    }

    fn event(&self, kind: DecisionKind) -> DecisionEvent {
        DecisionEvent {
            kind,
            long: self.open_score(Side::Long),
            short: self.open_score(Side::Short),
            exit: self.exit_score(),
        }
    }

    fn clear(&mut self) {
        self.open.clear();
        self.exit.clear();
    }

    /// Store a ballot if it fits the state (`Revoke` always applies).
    fn apply(&mut self, vote: Vote) {
        match vote.ballot {
            Ballot::Revoke => {
                self.open.remove(&vote.rule);
            }
            Ballot::Open(side) if self.position.is_none() => {
                self.open.insert(vote.rule, (side, vote.weight));
            }
            Ballot::Exit(side) if self.position == Some(side) => {
                self.exit.insert(vote.rule, (side, vote.weight));
            }
            Ballot::Open(_) | Ballot::Exit(_) => {}
        }
    }

    /// Decide on the stored ballots (at most once per candle).
    fn decide(&mut self) -> Option<DecisionEvent> {
        let now = self.current?;
        if self.decided == Some(now) {
            return None;
        }
        let kind = match self.position {
            None => {
                let (long, short) = (self.open_score(Side::Long), self.open_score(Side::Short));
                if long - short >= self.params.open_threshold {
                    DecisionKind::Open(Side::Long)
                } else if short - long >= self.params.open_threshold {
                    DecisionKind::Open(Side::Short)
                } else {
                    return None;
                }
            }
            Some(side) if self.exit_score() >= self.params.exit_threshold => {
                DecisionKind::Exit(side)
            }
            Some(_) => return None,
        };
        let event = self.event(kind);
        self.position = match kind {
            DecisionKind::Open(side) => Some(side),
            _ => None,
        };
        self.clear();
        self.decided = Some(now);
        Some(event)
    }
}

impl<I: SourceData> Condition for Decision<I> {
    type Input = I;
    type Value = DecisionEvent;
    type Modifier = DecisionModifier;

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, item: Item<'_, I>) -> Option<Detection<DecisionEvent>> {
        self.current = Some(item.data.time());
        Some(Detection {
            event_type: TriggerEventType::Other("tick"),
            value: self.event(DecisionKind::Tick),
        })
    }

    fn on_modifier(
        &mut self,
        modifier: DecisionModifier,
    ) -> Result<Option<Detection<DecisionEvent>>, ModifierError> {
        match modifier {
            DecisionModifier::Vote(vote) => self.apply(vote),
            DecisionModifier::Closed => {
                self.position = None;
                self.clear();
                return Ok(None);
            }
        }
        Ok(self.decide().map(|value| Detection {
            event_type: TriggerEventType::Other(match value.kind {
                DecisionKind::Open(_) => "open",
                _ => "exit",
            }),
            value,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use rust_decimal_macros::dec;
    use trading::candle::Candle;
    use trading::source::Mode;

    fn decision() -> Decision<Candle> {
        Decision::new(DecisionParams {
            open_threshold: dec!(2),
            exit_threshold: dec!(1),
        })
        .unwrap()
    }

    fn candle(d: &mut Decision<Candle>, i: i64) -> DecisionEvent {
        let c = Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(15 * i),
            open: dec!(1),
            high: dec!(1),
            low: dec!(1),
            close: dec!(1),
            volume: dec!(1),
        };
        d.on_data(Item {
            data: &c,
            mode: Mode::Replay,
        })
        .unwrap()
        .value
    }

    fn vote(d: &mut Decision<Candle>, rule: &str, ballot: Ballot, w: i64) -> Option<DecisionKind> {
        d.on_modifier(DecisionModifier::Vote(Vote {
            rule: rule.into(),
            ballot,
            weight: Decimal::from(w),
        }))
        .unwrap()
        .map(|det| det.value.kind)
    }

    use Ballot::{Exit, Open, Revoke};
    use Side::{Long, Short};

    #[test]
    fn opens_on_the_net_score() {
        let mut d = decision();
        assert_eq!(candle(&mut d, 0).kind, DecisionKind::Tick);
        assert_eq!(
            vote(&mut d, "a", Open(Long), 2),
            Some(DecisionKind::Open(Long))
        );
        assert_eq!(d.position(), Some(Long));

        let mut d = decision();
        candle(&mut d, 0);
        assert_eq!(vote(&mut d, "a", Open(Short), 1), None);
        assert_eq!(vote(&mut d, "b", Open(Long), 2), None, "net 1 < 2");
        assert_eq!(
            vote(&mut d, "c", Open(Long), 1),
            Some(DecisionKind::Open(Long)),
            "3 - 1"
        );
    }

    #[test]
    fn a_rule_has_one_open_ballot_and_revoke_removes_it() {
        let mut d = decision();
        candle(&mut d, 0);
        assert_eq!(vote(&mut d, "a", Open(Long), 1), None);
        // The opposite ballot of the same rule replaces it.
        assert_eq!(vote(&mut d, "a", Open(Short), 1), None);
        assert_eq!(vote(&mut d, "b", Open(Long), 1), None, "long 1, short 1");
        assert_eq!(vote(&mut d, "b", Revoke, 0), None);
        let tick = candle(&mut d, 1);
        assert_eq!((tick.long, tick.short), (dec!(0), dec!(1)));
        assert_eq!(
            vote(&mut d, "c", Open(Short), 1),
            Some(DecisionKind::Open(Short))
        );
    }

    #[test]
    fn exits_on_the_exit_score_and_ignores_ballots_out_of_state() {
        let mut d = decision();
        candle(&mut d, 0);
        assert_eq!(vote(&mut d, "x", Exit(Long), 5), None, "flat: exit ignored");
        assert_eq!(
            vote(&mut d, "a", Open(Long), 2),
            Some(DecisionKind::Open(Long))
        );
        candle(&mut d, 1);
        assert_eq!(
            vote(&mut d, "b", Open(Short), 9),
            None,
            "in position: open ignored"
        );
        assert_eq!(
            vote(&mut d, "x", Exit(Short), 5),
            None,
            "not the position side"
        );
        assert_eq!(
            vote(&mut d, "x", Exit(Long), 1),
            Some(DecisionKind::Exit(Long))
        );
        assert_eq!(d.position(), None);
        // Ballots cleared: the short ballot sent in position is not there.
        let tick = candle(&mut d, 2);
        assert_eq!(
            (tick.long, tick.short, tick.exit),
            (dec!(0), dec!(0), dec!(0))
        );
    }

    #[test]
    fn at_most_one_decision_per_candle_and_ballots_cleared() {
        let mut d = decision();
        candle(&mut d, 0);
        assert_eq!(
            vote(&mut d, "a", Open(Long), 2),
            Some(DecisionKind::Open(Long))
        );
        // Same candle: enough to exit, but no second decision.
        assert_eq!(vote(&mut d, "x", Exit(Long), 3), None);
        // Next candle: the exit ballot (stored) counts with the next vote.
        candle(&mut d, 1);
        assert_eq!(
            vote(&mut d, "y", Exit(Long), 0),
            Some(DecisionKind::Exit(Long))
        );
        // Cleared after the open: "a" no longer votes long.
        candle(&mut d, 2);
        assert_eq!(vote(&mut d, "b", Open(Long), 1), None);
    }

    #[test]
    fn closed_makes_it_flat_and_clears() {
        let mut d = decision();
        candle(&mut d, 0);
        vote(&mut d, "a", Open(Long), 2);
        candle(&mut d, 1);
        vote(&mut d, "x", Exit(Long), 0);
        assert_eq!(d.on_modifier(DecisionModifier::Closed).unwrap(), None);
        assert_eq!(d.position(), None);
        assert_eq!(
            vote(&mut d, "b", Open(Short), 2),
            Some(DecisionKind::Open(Short))
        );
        assert!(
            Decision::<Candle>::new(DecisionParams {
                open_threshold: dec!(0),
                exit_threshold: dec!(1),
            })
            .is_err()
        );
    }
}
