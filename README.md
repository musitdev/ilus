# ilus

**ilus** is a market-monitoring bot written in Rust. It watches market data
streams, detects **conditions** on them (for example candlestick patterns or
RSI divergences) and runs an **action** each time a condition triggers.

ilus is **not a trading-strategy engine**. Its core idea is generic:
*condition → action*. Placing an order is just one possible action, among
others like printing an alert, sending a notification or simulating a trade.

> Status: proof of concept. Not for production use, and nothing here is
> financial advice. The backtest examples are simulations on past data.

The full specification is in [`SPECS.md`](SPECS.md). Guidance for AI coding
agents is in [`AGENTS.md`](AGENTS.md).

---

## Concepts

| Entity | Role |
|---|---|
| **Source** | Produces closed data items (e.g. Binance candles for one asset and one timeframe). Each item carries the source's **running mode**. |
| **Transformer** | Turns one input into a new output (e.g. candles → candles + RSI). It **always forwards its input**, adding a computed value (`None` while it warms up). Transformers can be chained. |
| **Condition** | Reads one input and emits a **detection** when it sees its situation (e.g. a bullish divergence). |
| **Action** | Reacts to the detections of **one** condition (print, simulate a trade, …). It can keep state; network work (orders, webhooks) is started on the async runtime. |
| **Modifier** | A message sent (usually by an action) to a condition or a transformer to change how it works, e.g. "switch threshold" or "draw this trade marker". |
| **Graph** | The entities and their links, built in Rust code with a typed builder. |
| **Executor** | Runs the graph, tick by tick. |

```text
            ┌──> Transformer ──> Condition ──> Action
Source ─────┤         ^              ^            │
            └──> ...  └──────────────┴─ modifier ─┘
```

### How it runs

The executor runs **ticks**. A tick is fully sequential:

1. call the due sources (each returns zero or one new item);
2. run the transformers and conditions fed by the new items, in graph order;
3. run the actions of the conditions that triggered;
4. apply the modifiers produced during the tick. A modifier is one of the
   rules of a condition: if it makes the condition trigger, its actions run in
   the same tick (and their modifiers are applied in turn);
5. wait for the next tick.

Each source has a **running mode**, carried by every item it produces:

| Mode | Data | Actions | Ticks |
|---|---|---|---|
| **Bootstrap** | history before the start, to warm up the indicators | **not run** | global tick, every 10 ms |
| **Replay** | history over a chosen period (backtests) | run | global tick, every 10 ms (or as fast as possible) |
| **Normal** | live data | run | just after each candle close (+2 s), then every half period until the candle is there |

A source always starts with **bootstrap**, then goes to **normal** (live) or to
**replay** (backtest; the source then ends). The bootstrap length is computed
from the graph: each transformer and condition declares its warm-up, and each
source bootstraps the largest total warm-up found along the paths downstream
of it.

Two kinds of threads, never mixed:

- the **executor thread** (the one calling `Executor::run`) runs the ticks:
  sources' calls, transformers, conditions and actions, all sync. A slow
  entity only makes the tick longer;
- an **async runtime** owned by the executor, on its own threads, runs the
  network work: sources' fetches, and orders or webhooks started by actions
  (`ActionContext::spawn`). Their results come back as modifiers.

Key properties:

- **Typed graph**: connecting an entity to an output of the wrong type is a
  compile error.
- **Fan-out**: one output can feed any number of transformers and conditions;
  a condition can feed several actions.
- **No event is lost**, and **replays are deterministic**: there are no queues
  between entities, and a slow action simply makes the tick longer.
- **Everything is wired in Rust code**: there is no configuration file.
- **Money is `Decimal`**: prices and volumes never use floats (the chart window
  converts to `f64` for display only).

---

## What is implemented

- **Sources**
  - `BinanceSource`: Binance public spot market data over REST (no account or
    API key). History is fetched in the background by pages; live candles are
    requested just after each close. `with_bootstrap(n)` (at least `n` warm-up
    candles), `replay(from, to)` (backtest), `with_endpoint(url)`.
  - `MergeSource`: combines a driving source and a slower one (e.g. 15m + 1h)
    into one source. Each driving item comes with the last slower item
    already closed (no look-ahead); driving items wait until the slower data
    is there, so pairing is correct even when the sources are not aligned.
  - `MockSource`: items from memory (`replay` or `live`), for tests and
    examples.
- **Transformers**
  - `Rsi`: RSI (Wilder smoothing, default period 14). Output: `RsiCandle`
    (candle + RSI).
  - `Ema`: exponential moving average of the closes (seeded with the mean of
    the first `period` closes). Output: `EmaCandle` (candle + EMA).
  - `Macd`: MACD line, signal line and histogram (default 12 / 26 / 9).
  - `Aggregate`: builds candles of a longer timeframe (e.g. 1m → 5m); only
    complete periods are sent.
  - `Combine`: runs 2 or 3 transformers on the same input as one node.
    Adapters `OnMain`, `OnOther` (sides of a merged item) and `OnValue` (value
    of a transformer output) never process the same value twice.
  - `Chart` (feature `chart`): shows candles, RSI and trade markers in a native
    window.
- **Conditions**
  - `PatternCondition`: the 13 classic candlestick patterns (engulfing,
    hammer, hanging man, inverted hammer, shooting star, doji, morning and
    evening star, harami, piercing line, dark cloud cover), with a default
    threshold set that can be overridden. Several patterns matching the same
    candle are reported in one detection.
  - EMA buy / sell rules (`condition::ema_rules`):
    - `PriceCrossEma`: the close crosses its EMA;
    - `EmaCross`: a fast EMA crosses a slow one (9/21, 50/200…);
    - `EmaPullback`: in a trend, the price dips to the short EMA and closes
      back on the trend side;
    - `EmaRibbon`: three EMAs become aligned;
    - `MacdCross`: the MACD line crosses its signal line.

    A cross triggers once, on the candle where it happens.
  - `RsiDivergence`: detects divergences between price and RSI:
    - *bullish*: price makes a lower low while RSI makes a higher low;
    - *bearish*: price makes a higher high while RSI makes a lower high.

    Lows and highs are *pivots* (`left` / `right` candles around them).
    Defaults depend on the timeframe (`DivergenceParams::for_timeframe`).
- **Actions**
  - `Alert`: prints the trigger event. Works with any condition.

Splitting a source needs no special entity: one output can feed several
consumers (fan-out). A transformer or a condition never processes the same
value twice.

---

## Getting started

Requirements: a recent stable Rust toolchain (edition 2024).
The Binance examples need internet access. The chart example needs a desktop
session (X11 or Wayland).

```sh
cargo build
cargo test
```

### Minimal graph

```rust
use ilus::action::Alert;
use ilus::condition::divergence::{DivergenceParams, RsiDivergence};
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::source::Timeframe;
use ilus::source::binance::BinanceSource;
use ilus::transformer::rsi::Rsi;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut g = Graph::new();

    // Source: SOL/USDT 15-minute candles (bootstrap, then live).
    let sol = g.source(BinanceSource::new("SOLUSDT", Timeframe::M15));
    // Transformer: candles -> candles + RSI(14).
    let sol_rsi = g.transform(Rsi::new(14)?, &sol);
    // Condition: RSI divergence; action: print an alert.
    let divergence = g.condition(
        "sol-divergence",
        RsiDivergence::new(DivergenceParams::for_timeframe(Timeframe::M15))?,
        &sol_rsi,
    );
    g.action(Alert::new(), &divergence);

    // Blocking: runs the ticks on this thread (async work on its own threads).
    Executor::new(g).run()?;
    Ok(())
}
```

---

## Examples

Run any example with `cargo run --example <name>`. Arguments come after `--`.

### How data flows (mock data, no network)

These examples print a timeline of every step, to show how data moves
through the graph.

| Example | What it shows |
|---|---|
| `simple` | Every building block together: one source feeding two branches (fan-out), a transformer (SMA), and an action that sends a modifier back to its condition. |
| `flow_chain` | A chain source → transformer → transformer → condition within one tick; bootstrap items go through the chain without action events. |
| `flow_modifier` | The action → modifier → condition loop (the condition switches between oversold and overbought); the modifier is applied before the next candle; an invalid one is rejected. |
| `flow_cross` | Two independent chains: an action of one chain arms a condition of the other with a modifier, and that condition triggers in the same tick. |
| `flow_slow_action` | A slow (blocking) action makes the tick longer: nothing is queued, nothing is lost; a failing action does not stop the next tick. |
| `flow_parallel` | Two sources in one graph: one item each per tick, called one after the other. |
| `flow_modes` | Bootstrap, then normal (live) mode with calls just after each candle close (simulated clock, instant). |

```sh
cargo run --example simple
cargo run --example flow_chain
cargo run --example flow_modifier
cargo run --example flow_cross
cargo run --example flow_slow_action
cargo run --example flow_parallel
cargo run --example flow_modes
```

### Live Binance data

| Example | What it does |
|---|---|
| `binance_sol_rsi` | SOL/USDT 1m → RSI(14); prints every candle with its RSI (the bootstrap candles first), and alerts on live candles when RSI < 30 or > 70. |
| `rsi_divergence` | SOL/USDT 1m → RSI(14) → divergence detection; prints each divergence detected on the live feed. |
| `merge_rsi` | SOL/USDT 15m + 1h merged, with an RSI on each timeframe; prints when RSI 15m < 30 and RSI 1h < 40 (replay of the last days). |
| `candle_patterns` | The 13 candlestick patterns on SOL/USDT history; prints each candle with its patterns, then a count per pattern. |
| `ema_signals` | The five EMA rules on SOL/USDT history, all at once on the same source; prints each buy / sell signal, then a count per rule. |

```sh
cargo run --example binance_sol_rsi     # Ctrl+C to stop
cargo run --example rsi_divergence      # Ctrl+C to stop
cargo run --example merge_rsi -- 7      # last 7 days
cargo run --example candle_patterns -- 3 15m   # last 3 days, 15m
cargo run --example ema_signals -- 2 15m       # last 2 days, 15m
```

### Backtests on Binance history

Backtests use the **replay** mode: the source first bootstraps the candles
needed by the indicators (no trade during the warm-up), then replays the
period as fast as possible.

**`backtest_divergence`** (spot). Starts with 10 SOL. On a bearish
divergence it sells all its SOL; on a bullish divergence it buys back with all
its USDT. It runs one simulator per fee, all on the same condition.

```sh
cargo run --example backtest_divergence -- [--days N] [timeframe] [fees %...]

# last 7 days, 15-minute candles, without fee and with a 0.2 % fee
cargo run --example backtest_divergence -- --days 7 15m 0 0.2
```

**`backtest_futures`** (futures with leverage). It starts with 10 SOL of
capital, converted to USDT at the first candle. A bullish divergence opens a
long and a bearish divergence opens a short; any opposite position is closed
first. It also simulates fees (on the position size), an optional stop-loss
and liquidation. Funding is ignored.

```sh
cargo run --example backtest_futures -- [options] [timeframe] [fees %...]

# a given week, 10x, 3 % stop-loss, 1 confirmation candle, three fee levels
cargo run --example backtest_futures -- --from 2026-09-22 --to 2026-09-29 --stop 3 --right 1 15m 0 0.05 0.2
```

| Option | Meaning | Default |
|---|---|---|
| `--days N` | from N days ago (00:00 UTC) until now | 1 |
| `--from YYYY-MM-DD` / `--to YYYY-MM-DD` | explicit period (00:00 UTC, `--to` excluded) | — / now |
| `--leverage L` | leverage | 10 |
| `--stop P` | stop-loss, in % of price against the entry | none |
| `--left N` / `--right N` | pivot candles before / after (`right` = confirmation delay) | 3 / 3 |
| `--min-gap N` | minimum distance between the two pivots of a divergence, in candles | 5 at 1m, 3 at 5m, 2 from 15m |
| `--rsi-low R` / `--rsi-high R` | RSI level of the first low / high of a divergence (100 / 0: any level) | 30 / 70 |
| `--lookahead N` | pivots after the first one in which the diverging one is searched | 3 |
| `--tick-ms N` | replay tick, in ms (0: as fast as possible) | 0 |
| `timeframe` | `1m` `5m` `15m` `1h` `4h` `1d` | `1m` |
| `fees %...` | one simulation per fee (on position size, per transaction) | 0 |

### Chart window

**`chart_futures`** runs the futures simulation and shows the result in a
window: candles, the RSI panel and every trade. The chart is a side branch of
the RSI output, and the simulation sends it each trade as a modifier. It needs
the `chart` feature and takes the same options as `backtest_futures`. It uses
one fee only, the first one given. Use `--tick-ms 10` (or more) to watch the
replay unfold.

```sh
cargo run --features chart --example chart_futures -- --from 2026-09-22 --to 2026-09-29 --stop 3 --right 1 15m 0.05
```

- Markers: green ▲ long, red ▼ short, grey ○ close, orange ✕ stop-loss,
  purple ✱ liquidation.

**`chart_patterns`** shows the candlestick patterns detected on SOL/USDT
history, to inspect them: a blue ▲ under the low for bullish patterns, a pink ▼
above the high for bearish ones, on the candle that confirms them. The label
lists the patterns (several can match the same candle).

```sh
cargo run --features chart --example chart_patterns -- [days] [timeframe]
cargo run --features chart --example chart_patterns -- 2 15m
```

**`chart_signals`** shows everything on one chart, on SOL/USDT history:
- curves EMA 9 (yellow) and EMA 21 (purple) over the candles, RSI(14) in the
  bottom panel;
- EMA rules (price × EMA 9, EMA 9 × 21, pullback 9/21): big green ▲ buy / red
  ▼ sell at the close, labelled with the rule;
- RSI divergences: a segment between the two pivots on the price **and** on
  the RSI (blue bullish, pink bearish);
- candlestick patterns: small blue ▲ / pink ▼.

```sh
cargo run --features chart --example chart_signals -- [days] [timeframe]
cargo run --features chart --example chart_signals -- 2 15m
```
- Navigation:
  - mouse wheel: zoom the time axis;
  - left-drag: pan;
  - right-drag: box zoom;
  - double-click: reset.
- Toolbar: zoom in / out, *Fit all*, *Last 6h / 1d / 3d*, and *Auto-fit price*
  (fits the price axis to the visible candles).

---

## Project layout

```text
src/
  lib.rs              crate overview
  graph.rs            Graph builder (typed handles), nodes
  executor.rs         Executor (ticks), Clock
  source.rs           Source trait, Mode, Item, Timeframe, aligned schedule
  source/binance.rs   Binance source (REST)
  source/merge.rs     merge source (two timeframes)
  source/mock.rs      mock source
  candle.rs           Candle (OHLCV)
  transformer.rs      Transformer trait, Derived
  transformer/rsi.rs  RSI transformer
  transformer/ema.rs  EMA transformer
  transformer/macd.rs MACD transformer
  transformer/aggregate.rs  aggregation (1m -> 5m, ...)
  transformer/combine.rs    combined transformer and adapters
  transformer/chart.rs chart transformer + window (feature `chart`)
  condition.rs        Condition trait
  condition/divergence.rs  RSI divergence condition
  condition/patterns.rs    candlestick pattern condition (13 patterns)
  condition/ema_rules.rs   EMA buy / sell rules
  trigger.rs          Detection, TriggerEvent
  action.rs           Action trait, ActionContext, Alert
  modifier.rs         modifiers and handles
  error.rs            error types
examples/             runnable examples (see above)
  common/             helpers for the flow_* examples and simple
  support/futures.rs  futures simulation shared by backtest_futures and chart_futures
tests/executor.rs     executor tests (tick order, modes, bootstrap length, modifiers, determinism)
```

---

## Development

All four commands must pass before a change is done:

```sh
cargo build
cargo test                      # add --features chart to include the chart tests
cargo fmt --all
cargo clippy --all-targets -- -D warnings   # also with --features chart
```

Automated tests never use the network: they use `MockSource`, hand-made
data and a simulated clock.

---

## License

GNU GPL v3.0 or later (`GPL-3.0-or-later`).
