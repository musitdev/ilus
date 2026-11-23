//! Chart transformer: shows candles, an optional indicator (e.g. RSI) and
//! trade markers in a native window (spec F7, cargo feature `chart`).
//!
//! ```text
//!            ┌──> Chart  (side branch: fan-out, nothing downstream needed)
//! source ──> RSI ──> condition ──> action
//!                                     │ ChartModifier::Marker
//!            Chart <──────────────────┘
//! ```
//!
//! - [`chart`] creates the transformer ([`Chart`]) and the window ([`ChartWindow`]).
//! - The transformer forwards its input (like every transformer) and sends each
//!   candle to the window.
//! - Actions send trade markers with [`ChartModifier::Marker`] through a
//!   handle from [`Graph::modifier_handle`](crate::graph::Graph::modifier_handle).
//! - [`ChartWindow::run`] must be called on the **main thread** (GUI requirement);
//!   run the executor in another thread.
//!
//! Prices are converted to `f64` for display only.

use std::marker::PhantomData;
use std::sync::mpsc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eframe::egui::{self, Color32, Id, Stroke, Vec2};
use egui_plot::{
    BoxElem, BoxPlot, BoxSpread, HLine, HoverPosition, Legend, Line, MarkerShape, Plot, PlotPoint,
    PlotPoints, Points, Text,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::candle::Candle;
use crate::error::ModifierError;
use crate::modifier::ModifierValue;
use crate::source::{Item, SourceData};
use crate::transformer::{Derived, Transformer};

/// Items the chart can display: a candle and an optional indicator value.
pub trait ChartData: SourceData {
    /// The candle to draw.
    fn candle(&self) -> &Candle;
    /// Indicator value drawn in a second panel (e.g. RSI).
    fn indicator(&self) -> Option<Decimal> {
        None
    }
}

impl ChartData for Candle {
    fn candle(&self) -> &Candle {
        self
    }
}

/// A candle with an indicator value, e.g. [`RsiCandle`](crate::transformer::rsi::RsiCandle).
impl ChartData for Derived<Candle, Decimal> {
    fn candle(&self) -> &Candle {
        &self.data
    }
    fn indicator(&self) -> Option<Decimal> {
        self.value
    }
}

/// Kind of trade marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkerKind {
    /// Buy / open long (green triangle up).
    Buy,
    /// Sell / open short (red triangle down).
    Sell,
    /// Position closed (grey circle).
    Close,
    /// Stop-loss hit (orange cross).
    Stop,
    /// Liquidation (purple asterisk).
    Liquidation,
}

/// How a marker kind is drawn.
struct MarkerStyle {
    name: &'static str,
    shape: MarkerShape,
    color: Color32,
    radius: f32,
    filled: bool,
    /// Label on the right of the point (opens) or on the left (exits).
    label_right: bool,
}

impl MarkerKind {
    /// Drawing order: exits first, opens last (on top). A flip closes and
    /// opens at the same time and price: the open marker must stay visible.
    const DRAW_ORDER: [Self; 5] = [
        Self::Close,
        Self::Stop,
        Self::Liquidation,
        Self::Buy,
        Self::Sell,
    ];

    fn style(self) -> MarkerStyle {
        let (name, shape, color, radius, filled, label_right) = match self {
            Self::Buy => (
                "buy / long",
                MarkerShape::Up,
                Color32::from_rgb(0, 200, 80),
                9.0,
                true,
                true,
            ),
            Self::Sell => (
                "sell / short",
                MarkerShape::Down,
                Color32::from_rgb(230, 50, 50),
                9.0,
                true,
                true,
            ),
            Self::Close => (
                "close",
                MarkerShape::Circle,
                Color32::LIGHT_GRAY,
                5.0,
                false,
                false,
            ),
            Self::Stop => (
                "stop-loss",
                MarkerShape::Cross,
                Color32::from_rgb(255, 150, 0),
                8.0,
                true,
                false,
            ),
            Self::Liquidation => (
                "liquidation",
                MarkerShape::Asterisk,
                Color32::from_rgb(170, 60, 220),
                9.0,
                true,
                false,
            ),
        };
        MarkerStyle {
            name,
            shape,
            color,
            radius,
            filled,
            label_right,
        }
    }
}

/// A trade marker drawn on the price chart.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    /// Time of the trade (x position).
    pub time: DateTime<Utc>,
    /// Trade price (y position).
    pub price: Decimal,
    /// Kind of marker.
    pub kind: MarkerKind,
    /// Short text shown next to the marker.
    pub label: String,
}

/// Modifiers accepted by the [`Chart`] transformer.
#[derive(Debug, Clone, PartialEq)]
pub enum ChartModifier {
    /// Draw a trade marker.
    Marker(Marker),
}

impl ModifierValue for ChartModifier {}

/// Messages from the transformer to the window.
enum Msg {
    Candle {
        x: f64,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        indicator: Option<f64>,
    },
    Marker {
        x: f64,
        price: f64,
        kind: MarkerKind,
        label: String,
    },
}

fn f(v: Decimal) -> f64 {
    v.to_f64().unwrap_or(f64::NAN)
}

fn x(t: DateTime<Utc>) -> f64 {
    t.timestamp() as f64
}

/// Create a chart transformer and its window.
/// `indicator` names the indicator panel (e.g. `"RSI"`); it is shown only if
/// the items carry an indicator value.
pub fn chart<I: ChartData>(
    title: impl Into<String>,
    indicator: impl Into<String>,
) -> (Chart<I>, ChartWindow) {
    let (tx, rx) = mpsc::channel();
    let title = title.into();
    (
        Chart {
            tx,
            title: title.clone(),
            input: PhantomData,
        },
        ChartWindow {
            rx,
            title,
            indicator: indicator.into(),
        },
    )
}

/// Transformer sending every item to a [`ChartWindow`]. Its value is `()`:
/// it only forwards its input.
pub struct Chart<I> {
    tx: mpsc::Sender<Msg>,
    title: String,
    input: PhantomData<fn(I)>,
}

impl<I: ChartData> Transformer for Chart<I> {
    type Input = I;
    type Value = ();
    type Modifier = ChartModifier;

    fn name(&self) -> String {
        format!("chart({})", self.title)
    }

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, item: Item<'_, I>) -> Option<()> {
        let item = item.data;
        let c = item.candle();
        // If the window was closed, keep forwarding data anyway.
        let _ = self.tx.send(Msg::Candle {
            x: x(c.open_time),
            open: f(c.open),
            high: f(c.high),
            low: f(c.low),
            close: f(c.close),
            indicator: item.indicator().map(f),
        });
        Some(())
    }

    fn on_modifier(&mut self, modifier: ChartModifier) -> Result<(), ModifierError> {
        match modifier {
            ChartModifier::Marker(m) => {
                let _ = self.tx.send(Msg::Marker {
                    x: x(m.time),
                    price: f(m.price),
                    kind: m.kind,
                    label: m.label,
                });
                Ok(())
            }
        }
    }
}

/// Error raised by the chart window.
#[derive(Debug, thiserror::Error)]
#[error("chart window: {0}")]
pub struct ChartError(String);

/// Native window showing the chart. Call [`ChartWindow::run`] on the main thread.
pub struct ChartWindow {
    rx: mpsc::Receiver<Msg>,
    title: String,
    indicator: String,
}

impl ChartWindow {
    /// Open the window and block until it is closed.
    pub fn run(self) -> Result<(), ChartError> {
        let title = self.title.clone();
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 800.0]),
            ..Default::default()
        };
        eframe::run_native(
            &title,
            options,
            Box::new(|_cc| {
                Ok(Box::new(App {
                    window: self,
                    candles: Vec::new(),
                    markers: Vec::new(),
                    step: None,
                    fit_price: true,
                    pending: None,
                }))
            }),
        )
        .map_err(|e| ChartError(e.to_string()))
    }
}

struct CandlePoint {
    x: f64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    indicator: Option<f64>,
}

struct MarkerPoint {
    x: f64,
    price: f64,
    kind: MarkerKind,
    label: String,
}

/// View commands from the toolbar, applied on the next frame.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ViewCmd {
    /// Zoom the time axis around its center (> 1 zooms in).
    Zoom(f64),
    /// Show every candle.
    FitAll,
    /// Show the last `0` seconds.
    Last(f64),
}

/// Wheel zoom speed, per scrolled point.
const WHEEL_ZOOM: f32 = 0.002;

struct App {
    window: ChartWindow,
    candles: Vec<CandlePoint>,
    markers: Vec<MarkerPoint>,
    /// Smallest time step between candles (seconds), for the candle width.
    step: Option<f64>,
    /// Fit the price axis to the visible candles.
    fit_price: bool,
    pending: Option<ViewCmd>,
}

fn time_label(secs: f64) -> String {
    DateTime::from_timestamp(secs as i64, 0)
        .map_or_else(String::new, |t| t.format("%m-%d %H:%M").to_string())
}

/// Hover tooltip: time and value with `decimals` digits.
fn hover_label(pos: &HoverPosition<'_>, decimals: usize) -> Option<String> {
    let p = match pos {
        HoverPosition::NearDataPoint { position, .. } | HoverPosition::Elsewhere { position } => {
            position
        }
    };
    Some(format!("{}\n{:.decimals$}", time_label(p.x), p.y))
}

/// Time range to show for a view command, given the current range.
fn target_x(
    cmd: ViewCmd,
    candles: &[CandlePoint],
    step: f64,
    current: (f64, f64),
) -> Option<(f64, f64)> {
    let first = candles.first()?.x;
    let last = candles.last()?.x;
    Some(match cmd {
        ViewCmd::Zoom(factor) => {
            let center = (current.0 + current.1) / 2.0;
            let half = (current.1 - current.0) / 2.0 / factor;
            (center - half, center + half)
        }
        ViewCmd::FitAll => (first - step, last + step),
        ViewCmd::Last(secs) => ((last - secs).max(first) - step, last + step),
    })
}

/// Price range (with a 5 % margin) of the candles and markers in `[x0, x1]`.
fn visible_price_range(
    candles: &[CandlePoint],
    markers: &[MarkerPoint],
    x0: f64,
    x1: f64,
) -> Option<(f64, f64)> {
    let visible = |x: f64| x >= x0 && x <= x1;
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for c in candles.iter().filter(|c| visible(c.x)) {
        lo = lo.min(c.low);
        hi = hi.max(c.high);
    }
    for m in markers.iter().filter(|m| visible(m.x)) {
        lo = lo.min(m.price);
        hi = hi.max(m.price);
    }
    (lo.is_finite() && hi.is_finite() && lo <= hi).then(|| {
        let margin = ((hi - lo) * 0.05).max(hi.abs() * 1e-4);
        (lo - margin, hi + margin)
    })
}

impl App {
    fn drain(&mut self) {
        while let Ok(msg) = self.window.rx.try_recv() {
            match msg {
                Msg::Candle {
                    x,
                    open,
                    high,
                    low,
                    close,
                    indicator,
                } => {
                    if let Some(prev) = self.candles.last() {
                        let d = x - prev.x;
                        if d > 0.0 {
                            self.step = Some(self.step.map_or(d, |s| s.min(d)));
                        }
                    }
                    self.candles.push(CandlePoint {
                        x,
                        open,
                        high,
                        low,
                        close,
                        indicator,
                    });
                }
                Msg::Marker {
                    x,
                    price,
                    kind,
                    label,
                } => self.markers.push(MarkerPoint {
                    x,
                    price,
                    kind,
                    label,
                }),
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("➕ Zoom in").clicked() {
                self.pending = Some(ViewCmd::Zoom(2.0));
            }
            if ui.button("➖ Zoom out").clicked() {
                self.pending = Some(ViewCmd::Zoom(0.5));
            }
            if ui.button("Fit all").clicked() {
                self.pending = Some(ViewCmd::FitAll);
            }
            for (name, hours) in [("Last 6h", 6.0), ("Last 1d", 24.0), ("Last 3d", 72.0)] {
                if ui.button(name).clicked() {
                    self.pending = Some(ViewCmd::Last(hours * 3600.0));
                }
            }
            ui.separator();
            ui.checkbox(&mut self.fit_price, "Auto-fit price");
            ui.separator();
            ui.label(
                "wheel: zoom time · left-drag: pan · right-drag: box zoom · double-click: reset",
            );
        });
    }

    fn price_plot(&self, ui: &mut egui::Ui, link: Id, pending: Option<ViewCmd>, scroll: f32) {
        let step = self.step.unwrap_or(60.0);
        let up = Color32::from_rgb(38, 166, 154);
        let down = Color32::from_rgb(239, 83, 80);
        let boxes = self
            .candles
            .iter()
            .map(|c| {
                let color = if c.close >= c.open { up } else { down };
                let (lo, hi) = (c.open.min(c.close), c.open.max(c.close));
                BoxElem::new(c.x, BoxSpread::new(c.low, lo, (lo + hi) / 2.0, hi, c.high))
                    .box_width(step * 0.7)
                    .whisker_width(0.0)
                    .fill(color)
                    .stroke(Stroke::new(1.0, color))
            })
            .collect();
        // With auto-fit, the price axis is driven by the visible candles.
        let free_y = !self.fit_price;

        Plot::new("price")
            .link_axis(link, [true, false])
            .allow_scroll(false)
            .allow_zoom([true, free_y])
            .allow_drag([true, free_y])
            .x_axis_formatter(|mark, _| time_label(mark.value))
            .label_formatter(|pos| hover_label(pos, 4))
            .legend(Legend::default())
            .show(ui, |plot| {
                let bounds = plot.plot_bounds();
                let (x0, x1) = (bounds.min()[0], bounds.max()[0]);
                if let Some(cmd) = pending
                    && let Some((a, b)) = target_x(cmd, &self.candles, step, (x0, x1))
                {
                    plot.set_plot_bounds_x(a..=b);
                }
                if scroll != 0.0 && plot.response().hovered() {
                    plot.zoom_bounds_around_hovered(Vec2::new((scroll * WHEEL_ZOOM).exp(), 1.0));
                }
                if self.fit_price
                    && let Some((lo, hi)) =
                        visible_price_range(&self.candles, &self.markers, x0, x1)
                {
                    plot.set_plot_bounds_y(lo..=hi);
                }

                plot.box_plot(BoxPlot::new("candles", boxes).allow_hover(false));
                for kind in MarkerKind::DRAW_ORDER {
                    let style = kind.style();
                    let points: Vec<[f64; 2]> = self
                        .markers
                        .iter()
                        .filter(|m| m.kind == kind)
                        .map(|m| [m.x, m.price])
                        .collect();
                    if points.is_empty() {
                        continue;
                    }
                    plot.points(
                        Points::new(style.name, PlotPoints::new(points))
                            .shape(style.shape)
                            .color(style.color)
                            .filled(style.filled)
                            .radius(style.radius),
                    );
                }
                for m in &self.markers {
                    let style = m.kind.style();
                    let (text, anchor) = if style.label_right {
                        (format!("   {}", m.label), egui::Align2::LEFT_CENTER)
                    } else {
                        (format!("{}   ", m.label), egui::Align2::RIGHT_CENTER)
                    };
                    plot.text(
                        Text::new("", PlotPoint::new(m.x, m.price), text)
                            .color(style.color)
                            .anchor(anchor),
                    );
                }
            });
    }

    fn indicator_plot(&self, ui: &mut egui::Ui, link: Id, scroll: f32) {
        let points: Vec<[f64; 2]> = self
            .candles
            .iter()
            .filter_map(|c| c.indicator.map(|v| [c.x, v]))
            .collect();
        Plot::new("indicator")
            .link_axis(link, [true, false])
            .allow_scroll(false)
            .allow_zoom([true, false])
            .allow_drag([true, false])
            .x_axis_formatter(|mark, _| time_label(mark.value))
            .label_formatter(|pos| hover_label(pos, 2))
            .show(ui, |plot| {
                if scroll != 0.0 && plot.response().hovered() {
                    plot.zoom_bounds_around_hovered(Vec2::new((scroll * WHEEL_ZOOM).exp(), 1.0));
                }
                plot.set_plot_bounds_y(-2.0..=102.0);
                plot.hline(HLine::new("70", 70.0).color(Color32::DARK_GRAY));
                plot.hline(HLine::new("30", 30.0).color(Color32::DARK_GRAY));
                plot.line(
                    Line::new(self.window.indicator.clone(), PlotPoints::new(points))
                        .color(Color32::from_rgb(120, 140, 255)),
                );
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain();
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        let pending = self.pending.take();
        if scroll != 0.0 || pending.is_some() {
            // Apply the zoom and refit the price axis without waiting.
            ui.ctx().request_repaint();
        } else {
            // Data arrives from another thread: refresh regularly.
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }

        let link = Id::unique("ilus-chart-x-axis");
        let has_indicator = self.candles.iter().any(|c| c.indicator.is_some());
        if has_indicator {
            egui::Panel::bottom("indicator-panel")
                .resizable(true)
                .default_size(220.0)
                .show(ui, |ui| {
                    ui.label(&self.window.indicator);
                    self.indicator_plot(ui, link, scroll);
                });
        }
        egui::CentralPanel::default_margins().show(ui, |ui| {
            ui.label(format!(
                "{} — {} candles, {} markers",
                self.window.title,
                self.candles.len(),
                self.markers.len()
            ));
            self.toolbar(ui);
            self.price_plot(ui, link, pending, scroll);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use rust_decimal_macros::dec;

    fn replay<D>(data: &D) -> Item<'_, D> {
        Item {
            data,
            mode: crate::source::Mode::Replay,
        }
    }

    fn candle(i: i64, close: Decimal) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
            open: close,
            high: close + dec!(1),
            low: close - dec!(1),
            close,
            volume: Decimal::ONE,
        }
    }

    #[test]
    fn sends_candles_with_indicator_and_markers() {
        let (mut chart, window) = chart::<Derived<Candle, Decimal>>("test", "RSI");
        let item = Derived {
            data: candle(1, dec!(100)),
            value: Some(dec!(42)),
        };
        assert_eq!(chart.on_data(replay(&item)), Some(()));
        chart
            .on_modifier(ChartModifier::Marker(Marker {
                time: item.data.open_time,
                price: dec!(100),
                kind: MarkerKind::Buy,
                label: "long".into(),
            }))
            .unwrap();

        match window.rx.try_recv().unwrap() {
            Msg::Candle {
                x, high, indicator, ..
            } => {
                assert_eq!(x, 60.0);
                assert_eq!(high, 101.0);
                assert_eq!(indicator, Some(42.0));
            }
            Msg::Marker { .. } => panic!("expected a candle"),
        }
        match window.rx.try_recv().unwrap() {
            Msg::Marker {
                kind, price, label, ..
            } => {
                assert_eq!(kind, MarkerKind::Buy);
                assert_eq!(price, 100.0);
                assert_eq!(label, "long");
            }
            Msg::Candle { .. } => panic!("expected a marker"),
        }
    }

    #[test]
    fn keeps_forwarding_when_window_is_closed() {
        let (mut chart, window) = chart::<Candle>("test", "");
        drop(window);
        assert_eq!(chart.on_data(replay(&candle(1, dec!(10)))), Some(()));
    }

    fn point(x: f64, low: f64, high: f64) -> CandlePoint {
        CandlePoint {
            x,
            open: low,
            high,
            low,
            close: high,
            indicator: None,
        }
    }

    #[test]
    fn view_commands_compute_the_time_range() {
        let candles = [
            point(0.0, 1.0, 2.0),
            point(60.0, 1.0, 2.0),
            point(86_400.0, 1.0, 2.0),
        ];
        let step = 60.0;
        // Zoom in x2 around the center of the current range.
        assert_eq!(
            target_x(ViewCmd::Zoom(2.0), &candles, step, (0.0, 100.0)),
            Some((25.0, 75.0))
        );
        // Zoom out x2.
        assert_eq!(
            target_x(ViewCmd::Zoom(0.5), &candles, step, (25.0, 75.0)),
            Some((0.0, 100.0))
        );
        // Fit all: first to last candle, plus one step on each side.
        assert_eq!(
            target_x(ViewCmd::FitAll, &candles, step, (0.0, 1.0)),
            Some((-60.0, 86_460.0))
        );
        // Last 6h, and never before the first candle.
        assert_eq!(
            target_x(ViewCmd::Last(21_600.0), &candles, step, (0.0, 1.0)),
            Some((64_740.0, 86_460.0))
        );
        assert_eq!(
            target_x(ViewCmd::Last(1e9), &candles, step, (0.0, 1.0)),
            Some((-60.0, 86_460.0))
        );
        assert_eq!(target_x(ViewCmd::FitAll, &[], step, (0.0, 1.0)), None);
    }

    #[test]
    fn price_range_fits_the_visible_candles_and_markers() {
        let candles = [
            point(0.0, 100.0, 110.0),
            point(60.0, 90.0, 95.0),
            point(120.0, 50.0, 200.0),
        ];
        let markers = [MarkerPoint {
            x: 30.0,
            price: 120.0,
            kind: MarkerKind::Buy,
            label: String::new(),
        }];
        // Candles 0 and 60 + the marker: 90..120, margin 5 % of 30 = 1.5.
        assert_eq!(
            visible_price_range(&candles, &markers, 0.0, 60.0),
            Some((88.5, 121.5))
        );
        // Nothing visible.
        assert_eq!(visible_price_range(&candles, &markers, 500.0, 600.0), None);
    }

    #[test]
    fn open_markers_are_drawn_on_top_of_exits() {
        // A flip closes and opens at the same point: opens must come last.
        let pos = |k| MarkerKind::DRAW_ORDER.iter().position(|&x| x == k).unwrap();
        for open in [MarkerKind::Buy, MarkerKind::Sell] {
            for exit in [MarkerKind::Close, MarkerKind::Stop, MarkerKind::Liquidation] {
                assert!(
                    pos(open) > pos(exit),
                    "{open:?} must be drawn after {exit:?}"
                );
            }
        }
        // Every kind is drawn exactly once.
        assert_eq!(MarkerKind::DRAW_ORDER.len(), 5);
        // The close ring is smaller and hollow, so a triangle at the same point stays visible.
        let (close, buy) = (MarkerKind::Close.style(), MarkerKind::Buy.style());
        assert!(!close.filled && close.radius < buy.radius);
        // Labels: opens on the right, exits on the left (no overlap on a flip).
        assert!(buy.label_right && !close.label_right);
    }
}
