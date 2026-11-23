//! The graph of entities (spec §5.1).
//!
//! The builder is statically typed: every call returns a typed handle, so
//! connecting an entity to an output of the wrong type is a compile error.
//! Internally the nodes are stored as trait objects, so that a graph can hold
//! hundreds of entities of different types.
//!
//! ```text
//! let sol     = g.source(BinanceSource::new("SOLUSDT", M15));     // Output<Candle>
//! let sol_rsi = g.transform(Rsi::new(14)?, &sol);                 // Output<RsiCandle>
//! let div     = g.condition("div", RsiDivergence::default(), &sol_rsi);
//! g.action(Alert::new(), &div);
//! ```

use std::any::Any;
use std::fmt;
use std::marker::PhantomData;

use chrono::{DateTime, Utc};

use crate::action::{Action, ActionContext};
use crate::condition::Condition;
use crate::error::{ActionError, ModifierError};
use crate::modifier::{ModifierHandle, NoModifier};
use crate::source::{Item, Mode, Source, SourceInfo, SourcePoll};
use crate::transformer::{Derived, OutputOf, Transformer};
use crate::trigger::{Detection, TriggerEvent};

// ------------------------------------------------------------------ handles

/// Typed handle on the output of a source or a transformer. `M` is the
/// modifier type accepted by the node.
pub struct Output<T, M = NoModifier> {
    node: usize,
    types: PhantomData<fn() -> (T, M)>,
}

impl<T, M> Clone for Output<T, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, M> Copy for Output<T, M> {}

impl<T, M> fmt::Debug for Output<T, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Output(node {})", self.node)
    }
}

/// Marker carrying the types of a condition handle (no data).
type ConditionTypes<V, D, M> = PhantomData<fn() -> (V, D, M)>;

/// Typed handle on a condition: its value type `V`, its input type `D` and
/// the modifier type `M` it accepts.
pub struct ConditionOutput<V, D, M = NoModifier> {
    node: usize,
    types: ConditionTypes<V, D, M>,
}

impl<V, D, M> Clone for ConditionOutput<V, D, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V, D, M> Copy for ConditionOutput<V, D, M> {}

impl<V, D, M> fmt::Debug for ConditionOutput<V, D, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConditionOutput(node {})", self.node)
    }
}

/// A handle on a node that can receive modifiers.
pub trait NodeHandle {
    /// Modifier type accepted by the node.
    type Modifier;
    #[doc(hidden)]
    fn node(&self) -> usize;
}

impl<T, M> NodeHandle for Output<T, M> {
    type Modifier = M;
    fn node(&self) -> usize {
        self.node
    }
}

impl<V, D, M> NodeHandle for ConditionOutput<V, D, M> {
    type Modifier = M;
    fn node(&self) -> usize {
        self.node
    }
}

// ----------------------------------------------------------- erased nodes

/// Data of a node output during a tick.
pub(crate) struct Slot {
    pub(crate) data: Box<dyn Any + Send>,
    pub(crate) mode: Mode,
    pub(crate) time: DateTime<Utc>,
}

pub(crate) enum NodePoll {
    Ready(Slot),
    Pending,
    Ended,
}

pub(crate) trait DynSource: Send {
    fn set_bootstrap(&mut self, items: usize);
    fn mode(&self) -> Mode;
    fn poll(&mut self, now: DateTime<Utc>) -> NodePoll;
    fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc>;
}

struct SourceNode<S>(S);

impl<S: Source> DynSource for SourceNode<S> {
    fn set_bootstrap(&mut self, items: usize) {
        self.0.set_bootstrap(items);
    }
    fn mode(&self) -> Mode {
        self.0.mode()
    }
    fn poll(&mut self, now: DateTime<Utc>) -> NodePoll {
        match self.0.poll(now) {
            SourcePoll::Ready(data, mode) => NodePoll::Ready(Slot {
                time: crate::source::SourceData::time(&data),
                data: Box::new(data),
                mode,
            }),
            SourcePoll::Pending => NodePoll::Pending,
            SourcePoll::Ended => NodePoll::Ended,
        }
    }
    fn next_call(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        self.0.next_call(now)
    }
}

/// A transformer or a condition.
pub(crate) trait DynProc: Send {
    fn warm_up(&self) -> usize;
    /// Process one input item; returns the output data, if any. An item not
    /// newer than the last processed one is skipped (never the same value twice).
    fn process(
        &mut self,
        input: &dyn Any,
        mode: Mode,
        time: DateTime<Utc>,
    ) -> Option<Box<dyn Any + Send>>;
    fn apply(&mut self, modifier: Box<dyn Any + Send>) -> Result<(), ModifierError>;
}

fn wrong_modifier() -> ModifierError {
    ModifierError::Rejected("modifier of the wrong type (internal error)".into())
}

/// Time of the last item processed by a node; `true` if `time` is newer.
fn is_new(last: &mut Option<DateTime<Utc>>, time: DateTime<Utc>) -> bool {
    if last.is_some_and(|l| time <= l) {
        return false;
    }
    *last = Some(time);
    true
}

struct TransformerNode<T> {
    transformer: T,
    last: Option<DateTime<Utc>>,
}

impl<T: Transformer> DynProc for TransformerNode<T> {
    fn warm_up(&self) -> usize {
        self.transformer.warm_up()
    }
    fn process(
        &mut self,
        input: &dyn Any,
        mode: Mode,
        time: DateTime<Utc>,
    ) -> Option<Box<dyn Any + Send>> {
        if !is_new(&mut self.last, time) {
            return None;
        }
        let data = input.downcast_ref::<T::Input>()?;
        let value = self.transformer.on_data(Item { data, mode });
        Some(Box::new(Derived {
            data: data.clone(),
            value,
        }))
    }
    fn apply(&mut self, modifier: Box<dyn Any + Send>) -> Result<(), ModifierError> {
        let m = modifier
            .downcast::<T::Modifier>()
            .map_err(|_| wrong_modifier())?;
        self.transformer.on_modifier(*m)
    }
}

struct ConditionNode<C> {
    condition: C,
    last: Option<DateTime<Utc>>,
}

impl<C: Condition> DynProc for ConditionNode<C> {
    fn warm_up(&self) -> usize {
        self.condition.warm_up()
    }
    fn process(
        &mut self,
        input: &dyn Any,
        mode: Mode,
        time: DateTime<Utc>,
    ) -> Option<Box<dyn Any + Send>> {
        if !is_new(&mut self.last, time) {
            return None;
        }
        let data = input.downcast_ref::<C::Input>()?;
        let detection = self.condition.on_data(Item { data, mode })?;
        Some(Box::new(detection))
    }
    fn apply(&mut self, modifier: Box<dyn Any + Send>) -> Result<(), ModifierError> {
        let m = modifier
            .downcast::<C::Modifier>()
            .map_err(|_| wrong_modifier())?;
        self.condition.on_modifier(*m)
    }
}

/// What an action receives from the executor.
pub(crate) struct ActionInput<'a> {
    pub(crate) detection: &'a (dyn Any + Send),
    pub(crate) data: &'a (dyn Any + Send),
    pub(crate) condition: &'a str,
    pub(crate) source: &'a SourceInfo,
    pub(crate) time: DateTime<Utc>,
    pub(crate) mode: Mode,
}

pub(crate) trait DynAction: Send {
    fn condition(&self) -> usize;
    fn run(&mut self, input: ActionInput<'_>, ctx: &mut ActionContext) -> Result<(), ActionError>;
}

struct ActionNode<A> {
    action: A,
    condition: usize,
}

impl<A: Action> DynAction for ActionNode<A> {
    fn condition(&self) -> usize {
        self.condition
    }
    fn run(&mut self, input: ActionInput<'_>, ctx: &mut ActionContext) -> Result<(), ActionError> {
        let detection = input.detection.downcast_ref::<Detection<A::Value>>();
        let data = input.data.downcast_ref::<A::Input>();
        let (Some(detection), Some(data)) = (detection, data) else {
            return Err(ActionError::Failed(
                "event of the wrong type (internal error)".into(),
            ));
        };
        let event = TriggerEvent {
            condition: input.condition,
            event_type: detection.event_type,
            value: &detection.value,
            source: input.source,
            time: input.time,
            mode: input.mode,
            data,
        };
        self.action.run(event, ctx)
    }
}

pub(crate) enum NodeKind {
    Source(Box<dyn DynSource>),
    Proc {
        input: usize,
        node: Box<dyn DynProc>,
    },
}

pub(crate) struct Node {
    pub(crate) kind: NodeKind,
    /// Source description of the node output (for conditions: of its input).
    pub(crate) info: SourceInfo,
    /// Display name.
    pub(crate) name: String,
}

// ------------------------------------------------------------------ builder

/// The graph of entities, built in code. Run it with an
/// [`Executor`](crate::executor::Executor).
#[derive(Default)]
pub struct Graph {
    pub(crate) nodes: Vec<Node>,
    pub(crate) actions: Vec<Box<dyn DynAction>>,
}

impl Graph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    fn info(&self, node: usize) -> SourceInfo {
        self.nodes.get(node).map_or_else(
            || SourceInfo::Provider {
                provider: "?".into(),
                asset: "?".into(),
                timeframe: crate::source::Timeframe::M1,
            },
            |n| n.info.clone(),
        )
    }

    /// Add a source.
    pub fn source<S: Source>(&mut self, source: S) -> Output<S::Data> {
        let info = source.info();
        self.nodes.push(Node {
            name: info.to_string(),
            info,
            kind: NodeKind::Source(Box::new(SourceNode(source))),
        });
        Output {
            node: self.nodes.len() - 1,
            types: PhantomData,
        }
    }

    /// Add a transformer on `input`. Its output forwards the input items.
    pub fn transform<T: Transformer, M>(
        &mut self,
        transformer: T,
        input: &Output<T::Input, M>,
    ) -> Output<OutputOf<T>, T::Modifier> {
        let info = SourceInfo::Transformed {
            input: Box::new(self.info(input.node)),
            transformer: transformer.name(),
        };
        self.nodes.push(Node {
            name: info.to_string(),
            info,
            kind: NodeKind::Proc {
                input: input.node,
                node: Box::new(TransformerNode {
                    transformer,
                    last: None,
                }),
            },
        });
        Output {
            node: self.nodes.len() - 1,
            types: PhantomData,
        }
    }

    /// Add a condition named `name` on `input`.
    pub fn condition<C: Condition, M>(
        &mut self,
        name: impl Into<String>,
        condition: C,
        input: &Output<C::Input, M>,
    ) -> ConditionOutput<C::Value, C::Input, C::Modifier> {
        let info = self.info(input.node);
        self.nodes.push(Node {
            name: name.into(),
            info,
            kind: NodeKind::Proc {
                input: input.node,
                node: Box::new(ConditionNode {
                    condition,
                    last: None,
                }),
            },
        });
        ConditionOutput {
            node: self.nodes.len() - 1,
            types: PhantomData,
        }
    }

    /// Connect an action to a condition. An action instance belongs to one
    /// condition; a condition can have several actions.
    pub fn action<A: Action, M>(
        &mut self,
        action: A,
        condition: &ConditionOutput<A::Value, A::Input, M>,
    ) {
        self.actions.push(Box::new(ActionNode {
            action,
            condition: condition.node,
        }));
    }

    /// Typed handle to send modifiers to the node of `output` (a transformer
    /// or a condition), e.g. to give to an action.
    pub fn modifier_handle<H: NodeHandle>(&self, output: &H) -> ModifierHandle<H::Modifier> {
        ModifierHandle {
            node: output.node(),
            modifier: PhantomData,
        }
    }
}
