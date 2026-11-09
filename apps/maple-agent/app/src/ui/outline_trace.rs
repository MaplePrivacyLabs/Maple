//! A travelling trace along a rounded box's outline: two comets, opposite
//! each other, run clockwise around the box, each fading from a bright
//! head to a clear tail. It says "working" for a whole surface (a task row)
//! without a glyph of its own.
//!
//! The trace fills its parent absolutely and draws stroked paths only, so
//! it adds no layout and takes no mouse input. It loops on the shared
//! clock at [`TRACE_FPS`]: a moving arc steps visibly at the default loop
//! rate. Paused loops (reduced motion, background window) draw the rest
//! frame.

use std::f32::consts::{FRAC_PI_2, PI};
use std::time::Duration;

use gpui::{Bounds, ElementId, IntoElement, Pixels, Point, Styled, Window, canvas, point, px};

use super::motion;

/// One lap of a comet around the outline.
const PERIOD: Duration = Duration::from_millis(2400);

/// Repaints per second while the trace runs. At 30 a comet on a sidebar
/// row moves about seven pixels a frame, which its fading tail hides.
pub const TRACE_FPS: u32 = 30;

const STROKE: Pixels = px(1.25);

/// Share of the outline one comet covers, front to clear tail.
const COMET: f32 = 0.24;

/// Share of a comet spent rising to full strength ahead of its head, so
/// the front reads as light arriving rather than a cut line.
const LEAD: f32 = 0.1;

/// Bands a comet fades through. Each is one stroke of one color; fewer
/// show as steps along the tail.
const BANDS: usize = 24;

/// Longest straight step when sampling a band along a corner.
const SAMPLE: f32 = 2.;

/// Opacity of a comet's head.
const HEAD_ALPHA: f32 = 0.95;

/// A trace over the whole parent, which must be `relative` and rounded by
/// `radius`. `color` is a theme token read at build time, so a theme
/// switch takes effect on the next frame. `id` must be stable and unique
/// among animations in the view.
pub fn outline_trace(
    id: impl Into<ElementId>,
    radius: Pixels,
    color: fn() -> u32,
) -> impl IntoElement {
    motion::ticker(id, PERIOD, move |phase| {
        let color = color();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, _| paint(bounds, radius, color, phase, window),
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    })
    .fps(TRACE_FPS)
}

fn paint(bounds: Bounds<Pixels>, radius: Pixels, color: u32, phase: f32, window: &mut Window) {
    let Some(outline) = Outline::inside(bounds, radius) else {
        return;
    };
    let length = outline.length() * COMET;
    for front in [phase, phase + 0.5] {
        let front = front * outline.length();
        for band in 0..BANDS {
            let end = front - length * band as f32 / BANDS as f32;
            let start = front - length * (band + 1) as f32 / BANDS as f32;
            let mut path = gpui::PathBuilder::stroke(STROKE);
            path.move_to(outline.at(start));
            let steps = ((end - start) / SAMPLE).ceil().max(1.) as usize;
            for step in 1..=steps {
                path.line_to(outline.at(start + (end - start) * step as f32 / steps as f32));
            }
            if let Ok(path) = path.build() {
                let alpha = HEAD_ALPHA * strength((band as f32 + 0.5) / BANDS as f32);
                window.paint_path(path, gpui::rgb(color).alpha(alpha));
            }
        }
    }
}

/// A comet's strength at `along` its length, front (0) to tail (1): a
/// short linear rise to the head, then a quadratic fall, so the head reads
/// as a point and the tail as air.
fn strength(along: f32) -> f32 {
    if along < LEAD {
        along / LEAD
    } else {
        let fall = (1. - along) / (1. - LEAD);
        fall * fall
    }
}

/// The stroke's centerline: the box inset by half a stroke, so the list's
/// clip never shaves the outer edge, with the radius shrunk to match.
struct Outline {
    origin: Point<f32>,
    width: f32,
    height: f32,
    radius: f32,
}

impl Outline {
    fn inside(bounds: Bounds<Pixels>, radius: Pixels) -> Option<Self> {
        let inset = f32::from(STROKE) / 2.;
        let width = f32::from(bounds.size.width) - 2. * inset;
        let height = f32::from(bounds.size.height) - 2. * inset;
        if width <= 0. || height <= 0. {
            return None;
        }
        Some(Self {
            origin: point(
                f32::from(bounds.origin.x) + inset,
                f32::from(bounds.origin.y) + inset,
            ),
            width,
            height,
            radius: (f32::from(radius) - inset).clamp(0., width.min(height) / 2.),
        })
    }

    fn straight_x(&self) -> f32 {
        self.width - 2. * self.radius
    }

    fn straight_y(&self) -> f32 {
        self.height - 2. * self.radius
    }

    fn corner(&self) -> f32 {
        FRAC_PI_2 * self.radius
    }

    fn length(&self) -> f32 {
        2. * (self.straight_x() + self.straight_y()) + 4. * self.corner()
    }

    /// The point `distance` along the outline, clockwise from where the
    /// top edge leaves the top-left corner; any distance wraps.
    fn at(&self, distance: f32) -> Point<Pixels> {
        let (x, y) = (self.origin.x, self.origin.y);
        let (w, h, r) = (self.width, self.height, self.radius);
        let (sx, sy, c) = (self.straight_x(), self.straight_y(), self.corner());
        let arc = |cx: f32, cy: f32, from: f32, along: f32| {
            let angle = from + if r > 0. { along / r } else { 0. };
            (cx + r * angle.cos(), cy + r * angle.sin())
        };
        let mut d = distance.rem_euclid(self.length());
        let (at_x, at_y) = 'found: {
            if d < sx {
                break 'found (x + r + d, y);
            }
            d -= sx;
            if d < c {
                break 'found arc(x + w - r, y + r, -FRAC_PI_2, d);
            }
            d -= c;
            if d < sy {
                break 'found (x + w, y + r + d);
            }
            d -= sy;
            if d < c {
                break 'found arc(x + w - r, y + h - r, 0., d);
            }
            d -= c;
            if d < sx {
                break 'found (x + w - r - d, y + h);
            }
            d -= sx;
            if d < c {
                break 'found arc(x + r, y + h - r, FRAC_PI_2, d);
            }
            d -= c;
            if d < sy {
                break 'found (x, y + h - r - d);
            }
            d -= sy;
            arc(x + r, y + r, PI, d.min(c))
        };
        point(px(at_x), px(at_y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn outline() -> Outline {
        Outline::inside(
            Bounds::new(point(px(10.), px(20.)), size(px(200.), px(30.))),
            px(6.),
        )
        .expect("a box larger than the stroke")
    }

    fn close(a: Point<Pixels>, b: (f32, f32)) -> bool {
        (f32::from(a.x) - b.0).abs() < 0.01 && (f32::from(a.y) - b.1).abs() < 0.01
    }

    #[test]
    fn the_outline_is_continuous_and_wraps() {
        let outline = outline();
        let length = outline.length();
        let mut last = outline.at(0.);
        let steps = 2000;
        for step in 1..=steps {
            let next = outline.at(length * step as f32 / steps as f32);
            let gap = ((f32::from(next.x) - f32::from(last.x)).powi(2)
                + (f32::from(next.y) - f32::from(last.y)).powi(2))
            .sqrt();
            assert!(
                gap <= length / steps as f32 + 0.01,
                "jump of {gap} at step {step}"
            );
            last = next;
        }
        let start = outline.at(0.);
        assert!(close(
            outline.at(length),
            (f32::from(start.x), f32::from(start.y))
        ));
        assert!(close(outline.at(-1.), {
            let p = outline.at(length - 1.);
            (f32::from(p.x), f32::from(p.y))
        }));
    }

    #[test]
    fn the_stroke_stays_inside_the_box() {
        let outline = outline();
        let half = f32::from(STROKE) / 2.;
        for step in 0..500 {
            let p = outline.at(outline.length() * step as f32 / 500.);
            let (x, y) = (f32::from(p.x), f32::from(p.y));
            assert!((10. + half - 0.01..=210. - half + 0.01).contains(&x));
            assert!((20. + half - 0.01..=50. - half + 0.01).contains(&y));
        }
    }
}
