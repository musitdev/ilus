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
| **Source** | A stream of closed data items. A *provider source* (e.g. Binance) delivers candles for one asset and one timeframe. Each source has exactly one consumer. |
| **Transformer** | Turns one source into another (e.g. candles → candles + RSI). It **always forwards its input**, adding a computed value (`None` while it warms up). Transformers can be chained. |
| **Condition** | Reads one source and emits a **detection** when it sees its situation (e.g. a bullish divergence). |
| **Action** | Receives each **trigger event** and does something with it (print, simulate a trade, …). |
| **Modifier** | A message sent (usually by an action) to a condition or a transformer to change how it works, e.g. "switch threshold" or "draw this trade marker". |
| **Rule** | Binds a source stream, a condition and an action. |
| **Bot** | Runs all sources and rules concurrently. |

```text
Provider ──> Source ──> [Transformer]* ──> Condition ──trigger event──> Action
                              ^                ^                          │
                              └────────────────┴───────── modifier ───────┘
```

Key behaviors:

- **Closed data only**: a candle is processed only once it is closed.
- **No event is lost**: every detection is queued and processed by the action,
  in order. If the action is slower than the feed, the rule pauses its input
  until the queue is empty, which slows down the source (backpressure).
- **Everything is wired in Rust code**: there is no configuration file.
  Mismatched types (e.g. a candlestick condition on an RSI source) are compile
  errors.
- **Money is `Decimal`**: prices and volumes never use floats (the chart window
  converts to `f64` for display only).

---

## What is implemented

- **Sources**
  - `BinanceSource`: Binance public spot market data (no account or API key).
    History over REST, live candles over WebSocket, automatic reconnection with
    gap filling. `with_history(n)`, `with_start(time)` and `with_end(time)`
    (history replay only).
  - `MockSource`: items from memory, for tests and examples.
- **Transformers**
  - `Rsi`: RSI (Wilder smoothing, default period 14). Output: `RsiCandle`
    (candle + RSI).
  - `Chart` (feature `chart`): shows candles, RSI and trade markers in a native
    window.
- **Conditions**
  - `RsiDivergence`: detects divergences between price and RSI:
    - *bullish*: price makes a lower low while RSI makes a higher low;
    - *bearish*: price makes a higher high while RSI makes a lower high.

    Lows and highs are *pivots* (`left` / `right` candles around them).
    Defaults depend on the timeframe (`DivergenceParams::for_timeframe`).
- **Actions**
  - `Alert`: prints the trigger event. Works with any condition.

Planned in the spec but not implemented yet: the 13 candlestick pattern
conditions (spec F0).

---

## Getting started

Requirements: a recent stable Rust toolchain (edition 2024).
The Binance examples need internet access. The chart example needs a desktop
session (X11 or Wayland).

```sh
cargo build
cargo test
```

### Minimal pipeline

```rust
use ilus::action::Alert;
use ilus::bot::Bot;
use ilus::condition::divergence::{DivergenceParams, RsiDivergence};
use ilus::rule::Rule;
use ilus::source::binance::BinanceSource;
use ilus::source::{Source, Timeframe};
use ilus::transformer::Transformed;
use ilus::transformer::rsi::Rsi;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bot = Bot::new();

    // Source: SOL/USDT 15-minute candles, last 200 candles then live.
    let mut sol = BinanceSource::new("SOLUSDT", Timeframe::M15).with_history(200);
    // Transformer: candles -> candles + RSI(14).
    let mut sol_rsi = Transformed::new(Rsi::new(14)?, sol.get_source_stream()?);

    // Rule: RSI divergence -> print an alert.
    bot.rule(Rule::new(
        "sol-divergence",
        sol_rsi.get_source_stream()?,
        RsiDivergence::new(DivergenceParams::for_timeframe(Timeframe::M15))?,
        Alert,
    ));
    bot.source(sol);
    bot.source(sol_rsi);

    bot.run().await;
    Ok(())
}
```

---

## Examples

Run any example with `cargo run --example <name>`. Arguments come after `--`.

### Data flow between entities (mock data, no network)

These examples print a timeline of every step, to show how data moves between
entities.

| Example | What it shows |
|---|---|
| `simple` | Every building block together: source, transformer (SMA), conditions, an action that sends a modifier back to its condition. |
| `flow_chain` | A chain source → transformer → transformer → condition: nested values and warm-up `None` passed down the chain. |
| `flow_modifier` | The action → modifier → condition loop (the condition switches between oversold and overbought); an invalid modifier is rejected. |
| `flow_slow_action` | A slow action: events are queued, none is lost, and the input pauses when the queue is full. |
| `flow_parallel` | Independent rules running in parallel. |

```sh
cargo run --example simple
cargo run --example flow_chain
cargo run --example flow_modifier
cargo run --example flow_slow_action
cargo run --example flow_parallel
```

### Live Binance data

| Example | What it does |
|---|---|
| `binance_sol_rsi` | SOL/USDT 1m → RSI(14); prints every candle with its RSI and alerts when RSI < 30 or > 70. |
| `rsi_divergence` | SOL/USDT 1m → RSI(14) → divergence detection; prints each divergence (the last 500 candles first, then live). |

```sh
cargo run --example binance_sol_rsi     # Ctrl+C to stop
cargo run --example rsi_divergence      # Ctrl+C to stop
```

### Backtests on Binance history

**`backtest_divergence`** (spot). Starts with 10 SOL. On a bearish
divergence it sells all its SOL; on a bullish divergence it buys back with all
its USDT. It runs one simulation per fee.

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
| `--min-gap N` / `--max-gap N` | distance between two compared pivots, in candles | 60 max; min 5 at 1m, 3 at 5m, 2 from 15m |
| `timeframe` | `1m` `5m` `15m` `1h` `4h` `1d` | `1m` |
| `fees %...` | one simulation per fee (on position size, per transaction) | 0 |

### Chart window

**`chart_futures`** runs the futures simulation and shows the result in a
window: candles, the RSI panel and every trade. It needs the `chart` feature
and takes the same options as `backtest_futures`. It uses one fee only, the
first one given.

```sh
cargo run --features chart --example chart_futures -- --from 2026-09-22 --to 2026-09-29 --stop 3 --right 1 15m 0.05
```

- Markers: green ▲ long, red ▼ short, grey ○ close, orange ✕ stop-loss,
  purple ✱ liquidation.
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
  source.rs           Source trait, SourceChannel / SourceStream, Timeframe
  source/binance.rs   Binance source
  source/mock.rs      mock source
  candle.rs           Candle (OHLCV)
  transformer.rs      Transformer trait, Transformed, Derived
  transformer/rsi.rs  RSI transformer
  transformer/chart.rs chart transformer + window (feature `chart`)
  condition.rs        Condition trait
  condition/divergence.rs  RSI divergence condition
  trigger.rs          Detection, TriggerEvent
  action.rs           Action trait, Alert
  modifier.rs         modifiers
  rule.rs             Rule (event queue, backpressure)
  bot.rs              Bot
  error.rs            error types
examples/             runnable examples (see above)
  common/             helpers for the flow_* examples
  support/futures.rs  futures simulation shared by backtest_futures and chart_futures
tests/runtime.rs      runtime behavior tests (queue, backpressure, modifiers, transformers)
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

Automated tests never use the network: they use `MockSource` and hand-made
data.

---

## License

GNU GPL v3.0 or later (`GPL-3.0-or-later`).
