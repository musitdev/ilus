//! The bot: runs sources (including transformed ones) and rules concurrently.

use std::pin::Pin;

use tokio::task::JoinSet;

use crate::action::Action;
use crate::condition::Condition;
use crate::error::Error;
use crate::rule::Rule;
use crate::source::Source;

type Task = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;

/// Collects sources and rules, then runs them all concurrently.
///
/// Take every stream (`get_source_stream`) before calling [`Bot::run`].
#[derive(Default)]
pub struct Bot {
    tasks: Vec<Task>,
}

impl Bot {
    /// Create an empty bot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a source (provider or [`Transformed`](crate::transformer::Transformed)).
    pub fn source<S: Source>(&mut self, source: S) {
        self.tasks.push(Box::pin(
            async move { source.run().await.map_err(Error::from) },
        ));
    }

    /// Register a rule.
    pub fn rule<C: Condition, A: Action<C>>(&mut self, rule: Rule<C, A>) {
        self.tasks.push(Box::pin(rule.run()));
    }

    /// Run everything until all sources have ended and all actions have finished.
    /// A failing task is logged and does not stop the others.
    pub async fn run(self) {
        let mut set = JoinSet::new();
        for task in self.tasks {
            set.spawn(task);
        }
        while let Some(result) = set.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("[bot] {e}"),
                Err(e) => eprintln!("[bot] task panicked: {e}"),
            }
        }
    }
}
