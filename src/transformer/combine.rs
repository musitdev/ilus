//! Combined transformer and adapters (spec F4).
//!
//! - [`Combine`] runs 2 or 3 transformers with the same input as one node;
//!   its value is the tuple of their values.
//! - [`OnMain`] / [`OnOther`] run a transformer on one side of a merged item.
//! - [`OnValue`] runs a transformer on the value of a transformer output.
//!
//! An adapter never processes the same value twice: when the part it reads
//! has the same time as the last one processed, the inner transformer is not
//! called and the last value is returned.

use std::fmt;
use std::marker::PhantomData;

use chrono::{DateTime, Utc};

use crate::error::ModifierError;
use crate::modifier::{ModifierValue, NoModifier};
use crate::source::merge::Merged;
use crate::source::{Item, SourceData};
use crate::transformer::{Derived, Transformer};

/// Several transformers on the same input, as one node.
pub struct Combine<T>(T);

impl<T> Combine<T> {
    /// Combine a tuple of 2 or 3 transformers.
    pub fn new(transformers: T) -> Self {
        Self(transformers)
    }
}

/// Modifier for one transformer of a [`Combine`].
#[derive(Debug)]
pub enum CombineModifier<A, B, C = NoModifier> {
    /// For the first transformer.
    First(A),
    /// For the second transformer.
    Second(B),
    /// For the third transformer.
    Third(C),
}

impl<A: ModifierValue, B: ModifierValue, C: ModifierValue> ModifierValue
    for CombineModifier<A, B, C>
{
}

impl<A, B> Transformer for Combine<(A, B)>
where
    A: Transformer,
    B: Transformer<Input = A::Input>,
{
    type Input = A::Input;
    type Value = (Option<A::Value>, Option<B::Value>);
    type Modifier = CombineModifier<A::Modifier, B::Modifier>;

    fn name(&self) -> String {
        format!("combine({}, {})", self.0.0.name(), self.0.1.name())
    }
    fn warm_up(&self) -> usize {
        self.0.0.warm_up().max(self.0.1.warm_up())
    }
    fn on_data(&mut self, item: Item<'_, A::Input>) -> Option<Self::Value> {
        Some((self.0.0.on_data(item), self.0.1.on_data(item)))
    }
    fn on_modifier(&mut self, m: Self::Modifier) -> Result<(), ModifierError> {
        match m {
            CombineModifier::First(m) => self.0.0.on_modifier(m),
            CombineModifier::Second(m) => self.0.1.on_modifier(m),
            CombineModifier::Third(m) => match m {},
        }
    }
}

impl<A, B, C> Transformer for Combine<(A, B, C)>
where
    A: Transformer,
    B: Transformer<Input = A::Input>,
    C: Transformer<Input = A::Input>,
{
    type Input = A::Input;
    type Value = (Option<A::Value>, Option<B::Value>, Option<C::Value>);
    type Modifier = CombineModifier<A::Modifier, B::Modifier, C::Modifier>;

    fn name(&self) -> String {
        format!(
            "combine({}, {}, {})",
            self.0.0.name(),
            self.0.1.name(),
            self.0.2.name()
        )
    }
    fn warm_up(&self) -> usize {
        self.0
            .0
            .warm_up()
            .max(self.0.1.warm_up())
            .max(self.0.2.warm_up())
    }
    fn on_data(&mut self, item: Item<'_, A::Input>) -> Option<Self::Value> {
        Some((
            self.0.0.on_data(item),
            self.0.1.on_data(item),
            self.0.2.on_data(item),
        ))
    }
    fn on_modifier(&mut self, m: Self::Modifier) -> Result<(), ModifierError> {
        match m {
            CombineModifier::First(m) => self.0.0.on_modifier(m),
            CombineModifier::Second(m) => self.0.1.on_modifier(m),
            CombineModifier::Third(m) => self.0.2.on_modifier(m),
        }
    }
}

/// Runs `inner` on `part` unless it is the same as the last part processed.
struct Dedup<T: Transformer> {
    inner: T,
    last_time: Option<DateTime<Utc>>,
    last_value: Option<T::Value>,
}

impl<T: Transformer> Dedup<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            last_time: None,
            last_value: None,
        }
    }

    fn run(&mut self, part: Item<'_, T::Input>) -> Option<T::Value> {
        let time = part.data.time();
        if self.last_time.is_none_or(|t| time > t) {
            self.last_time = Some(time);
            self.last_value = self.inner.on_data(part);
        }
        self.last_value.clone()
    }
}

macro_rules! adapter {
    ($(#[$doc:meta])* $name:ident, $input:ty, $other:ident, |$item:ident| $part:expr) => {
        $(#[$doc])*
        pub struct $name<T: Transformer, $other> {
            dedup: Dedup<T>,
            types: PhantomData<fn($other)>,
        }

        impl<T: Transformer, $other> $name<T, $other> {
            /// Wrap `inner`.
            pub fn new(inner: T) -> Self {
                Self {
                    dedup: Dedup::new(inner),
                    types: PhantomData,
                }
            }
        }

        impl<T, $other> Transformer for $name<T, $other>
        where
            T: Transformer,
            $other: fmt::Debug + Clone + Send + Sync + 'static,
            $input: SourceData,
        {
            type Input = $input;
            type Value = T::Value;
            type Modifier = T::Modifier;

            fn name(&self) -> String {
                format!("{}({})", stringify!($name), self.dedup.inner.name())
            }
            fn warm_up(&self) -> usize {
                self.dedup.inner.warm_up()
            }
            fn on_data(&mut self, $item: Item<'_, Self::Input>) -> Option<T::Value> {
                let mode = $item.mode;
                let data = $part?;
                self.dedup.run(Item { data, mode })
            }
            fn on_modifier(&mut self, m: T::Modifier) -> Result<(), ModifierError> {
                self.dedup.inner.on_modifier(m)
            }
        }
    };
}

adapter!(
    /// Runs a transformer on the driving side of a merged item.
    OnMain, Merged<T::Input, O>, O, |item| Some(&item.data.main)
);
adapter!(
    /// Runs a transformer on the slower side of a merged item, only when it
    /// is a new item (it repeats until a newer one is available).
    OnOther, Merged<M, T::Input>, M, |item| item.data.other.as_ref()
);
adapter!(
    /// Runs a transformer on the value of a transformer output (e.g. the
    /// candle of an aggregation), only when it is a new value.
    OnValue, Derived<D, T::Input>, D, |item| item.data.value.as_ref()
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::Candle;
    use crate::source::Mode;
    use chrono::TimeDelta;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn candle(min: i64, close: i64) -> Candle {
        let close = Decimal::from(close);
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(min),
            open: close,
            high: close,
            low: close,
            close,
            volume: dec!(1),
        }
    }

    fn item<D>(data: &D) -> Item<'_, D> {
        Item {
            data,
            mode: Mode::Replay,
        }
    }

    /// Counts its calls; value = close * factor.
    struct Count {
        factor: Decimal,
        calls: usize,
        warm_up: usize,
    }

    #[derive(Debug)]
    struct SetFactor(Decimal);
    impl ModifierValue for SetFactor {}

    impl Transformer for Count {
        type Input = Candle;
        type Value = Decimal;
        type Modifier = SetFactor;
        fn name(&self) -> String {
            "count".into()
        }
        fn warm_up(&self) -> usize {
            self.warm_up
        }
        fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Decimal> {
            self.calls += 1;
            Some(item.data.close * self.factor)
        }
        fn on_modifier(&mut self, m: SetFactor) -> Result<(), ModifierError> {
            self.factor = m.0;
            Ok(())
        }
    }

    fn count(factor: i64, warm_up: usize) -> Count {
        Count {
            factor: Decimal::from(factor),
            calls: 0,
            warm_up,
        }
    }

    #[test]
    fn combine_runs_every_transformer_and_routes_modifiers() {
        let mut c = Combine::new((count(1, 3), count(10, 7)));
        assert_eq!(c.warm_up(), 7);
        assert_eq!(
            c.on_data(item(&candle(0, 2))),
            Some((Some(dec!(2)), Some(dec!(20))))
        );
        c.on_modifier(CombineModifier::Second(SetFactor(dec!(100))))
            .unwrap();
        assert_eq!(
            c.on_data(item(&candle(1, 3))),
            Some((Some(dec!(3)), Some(dec!(300))))
        );
        let mut c3 = Combine::new((count(1, 0), count(2, 0), count(3, 9)));
        assert_eq!(c3.warm_up(), 9);
        assert_eq!(
            c3.on_data(item(&candle(0, 1))),
            Some((Some(dec!(1)), Some(dec!(2)), Some(dec!(3))))
        );
    }

    #[test]
    fn on_other_processes_each_slower_item_once() {
        let mut t = OnOther::<_, Candle>::new(count(1, 0));
        let h0 = candle(0, 50);
        let h1 = candle(60, 60);
        // Four 15m items with the same 1h candle, then a new 1h candle.
        for (m, other) in [(0, &h0), (15, &h0), (30, &h0), (45, &h0), (60, &h1)] {
            let merged = Merged {
                main: candle(m, 1),
                other: Some(other.clone()),
            };
            let v = t.on_data(item(&merged));
            assert_eq!(v, Some(other.close));
        }
        assert_eq!(t.dedup.inner.calls, 2, "one call per new 1h candle");
        // No slower item yet: no value.
        let none: Merged<Candle, Candle> = Merged {
            main: candle(75, 1),
            other: None,
        };
        let mut fresh = OnOther::<_, Candle>::new(count(1, 0));
        assert_eq!(fresh.on_data(item(&none)), None);
    }

    #[test]
    fn on_main_and_on_value_read_their_part() {
        let mut main = OnMain::<_, Candle>::new(count(2, 0));
        let merged = Merged {
            main: candle(0, 5),
            other: None::<Candle>,
        };
        assert_eq!(main.on_data(item(&merged)), Some(dec!(10)));

        let mut value = OnValue::<_, Candle>::new(count(1, 0));
        let five = candle(0, 42);
        for m in 5..8 {
            let d = Derived {
                data: candle(m, 1),
                value: Some(five.clone()),
            };
            assert_eq!(value.on_data(item(&d)), Some(dec!(42)));
        }
        assert_eq!(
            value.dedup.inner.calls, 1,
            "the repeated 5m candle is processed once"
        );
    }
}
