//! Zoom and pan math for images in the preview pane (pure, no drawing).

/// Largest zoom factor.
pub const MAX_SCALE: f32 = 16.0;
/// Smallest zoom factor, unless Fit is smaller.
pub const MIN_SCALE: f32 = 0.1;
/// One zoom step for the buttons and one wheel line.
pub const STEP: f32 = 1.25;
/// Wheel pixels that make one zoom step.
pub const WHEEL_STEP_PX: f32 = 50.0;

/// The scale that shows the whole image, never above 100%.
pub fn fit_scale(natural: (f32, f32), viewport: (f32, f32)) -> f32 {
    (viewport.0 / natural.0).min(viewport.1 / natural.1).min(1.0)
}

/// The zoom factor of a wheel movement of `delta_y_px` pixels (negative zooms in).
pub fn wheel_factor(delta_y_px: f32) -> f32 {
    STEP.powf(-delta_y_px / WHEEL_STEP_PX)
}

/// Keeps one axis in view: centered when the scaled image is smaller than the viewport, else within the edges.
fn clamp_axis(origin: f32, scaled: f32, viewport: f32) -> f32 {
    if scaled <= viewport { (viewport - scaled) / 2.0 } else { origin.clamp(viewport - scaled, 0.0) }
}

/// How an image is placed in the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Zoom {
    /// Viewport pixels per image pixel.
    pub scale: f32,
    /// The image's top-left corner in viewport pixels.
    pub origin: (f32, f32),
    /// Whether the image follows the viewport (Fit) instead of a chosen zoom.
    pub fit: bool,
}

impl Zoom {
    /// The Fit placement: centered, never enlarged.
    pub fn fit(natural: (f32, f32), viewport: (f32, f32)) -> Zoom {
        let scale = fit_scale(natural, viewport);
        Zoom {
            scale,
            origin: ((viewport.0 - natural.0 * scale) / 2.0, (viewport.1 - natural.1 * scale) / 2.0),
            fit: true,
        }
    }

    /// Adapts to a new viewport size: Fit is recomputed, any other zoom is clamped.
    pub fn relayout(&mut self, natural: (f32, f32), viewport: (f32, f32)) {
        if self.fit {
            *self = Zoom::fit(natural, viewport);
        } else {
            self.clamp(natural, viewport);
        }
    }

    /// Multiplies the scale by `factor`, keeping the image point under `pointer` still.
    pub fn zoom_at(&mut self, factor: f32, pointer: (f32, f32), natural: (f32, f32), viewport: (f32, f32)) {
        let min = fit_scale(natural, viewport).min(MIN_SCALE);
        let scale = (self.scale * factor).clamp(min, MAX_SCALE);
        let point = ((pointer.0 - self.origin.0) / self.scale, (pointer.1 - self.origin.1) / self.scale);
        self.origin = (pointer.0 - point.0 * scale, pointer.1 - point.1 * scale);
        self.scale = scale;
        self.fit = false;
        self.clamp(natural, viewport);
    }

    /// Moves the image by `delta` viewport pixels, stopping at the edges.
    pub fn pan(&mut self, delta: (f32, f32), natural: (f32, f32), viewport: (f32, f32)) {
        self.origin = (self.origin.0 + delta.0, self.origin.1 + delta.1);
        self.clamp(natural, viewport);
    }

    /// Fit or any zoom other than 100% goes to 100% around `pointer`; 100% goes back to Fit.
    pub fn toggle(&mut self, pointer: (f32, f32), natural: (f32, f32), viewport: (f32, f32)) {
        if self.fit || (self.scale - 1.0).abs() > 1e-3 {
            self.zoom_at(1.0 / self.scale, pointer, natural, viewport);
        } else {
            *self = Zoom::fit(natural, viewport);
        }
    }

    /// The zoom as a rounded percentage.
    pub fn percent(&self) -> u32 {
        (self.scale * 100.0).round() as u32
    }

    /// The visible part as `[x, y, w, h]` fractions of the image; `None` while all of it is visible.
    pub fn minimap(&self, natural: (f32, f32), viewport: (f32, f32)) -> Option<[f32; 4]> {
        let axis = |origin: f32, size: f32, view: f32| {
            let scaled = size * self.scale;
            if scaled <= view + 1e-3 {
                (0.0, 1.0)
            } else {
                ((-origin / scaled).clamp(0.0, 1.0), (view / scaled).min(1.0))
            }
        };
        let (x, w) = axis(self.origin.0, natural.0, viewport.0);
        let (y, h) = axis(self.origin.1, natural.1, viewport.1);
        (w < 1.0 || h < 1.0).then_some([x, y, w, h])
    }

    fn clamp(&mut self, natural: (f32, f32), viewport: (f32, f32)) {
        self.origin = (
            clamp_axis(self.origin.0, natural.0 * self.scale, viewport.0),
            clamp_axis(self.origin.1, natural.1 * self.scale, viewport.1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: (f32, f32) = (1000.0, 1000.0);

    #[test]
    fn fit_never_enlarges() {
        let z = Zoom::fit((100.0, 50.0), VP);
        assert_eq!(z.scale, 1.0);
        assert_eq!(z.origin, (450.0, 475.0));
        assert!(z.fit);
    }

    #[test]
    fn fit_shrinks_to_the_tighter_side() {
        assert_eq!(Zoom::fit((2000.0, 1000.0), VP).scale, 0.5);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        let nat = (2000.0, 1500.0);
        let before = Zoom::fit(nat, VP);
        let mut z = before;
        let p = (300.0, 400.0);
        z.zoom_at(2.0, p, nat, VP);
        let img_before = ((p.0 - before.origin.0) / before.scale, (p.1 - before.origin.1) / before.scale);
        let img_after = ((p.0 - z.origin.0) / z.scale, (p.1 - z.origin.1) / z.scale);
        assert!((img_before.0 - img_after.0).abs() < 0.01);
        assert!((img_before.1 - img_after.1).abs() < 0.01);
        assert!(!z.fit);
    }

    #[test]
    fn zoom_is_clamped() {
        let nat = (100.0, 50.0);
        let mut z = Zoom::fit(nat, VP);
        for _ in 0..16 {
            z.zoom_at(STEP, (500.0, 500.0), nat, VP);
        }
        assert_eq!(z.scale, MAX_SCALE);
        for _ in 0..100 {
            z.zoom_at(1.0 / STEP, (500.0, 500.0), nat, VP);
        }
        assert_eq!(z.scale, fit_scale(nat, VP).min(MIN_SCALE));
        let big = (40000.0, 40000.0);
        let mut z = Zoom::fit(big, VP);
        for _ in 0..100 {
            z.zoom_at(1.0 / STEP, (500.0, 500.0), big, VP);
        }
        assert_eq!(z.scale, fit_scale(big, VP));
    }

    #[test]
    fn pan_stops_at_the_edges() {
        let nat = (2000.0, 1000.0);
        let mut z = Zoom::fit(nat, VP);
        z.zoom_at(2.0, (500.0, 500.0), nat, VP);
        z.pan((-1e6, -1e6), nat, VP);
        assert_eq!(z.origin.0 + nat.0 * z.scale, VP.0);
        assert_eq!(z.origin.1 + nat.1 * z.scale, VP.1);
        z.pan((1e6, 1e6), nat, VP);
        assert_eq!(z.origin, (0.0, 0.0));
    }

    #[test]
    fn a_small_image_stays_centered_when_panned() {
        let nat = (100.0, 50.0);
        let mut z = Zoom::fit(nat, VP);
        z.pan((300.0, -200.0), nat, VP);
        assert_eq!(z.origin, (450.0, 475.0));
    }

    #[test]
    fn toggle_goes_to_100_then_back_to_fit() {
        let nat = (2000.0, 1000.0);
        let mut z = Zoom::fit(nat, VP);
        z.toggle((500.0, 500.0), nat, VP);
        assert_eq!(z.percent(), 100);
        assert!(!z.fit);
        z.toggle((500.0, 500.0), nat, VP);
        assert_eq!(z, Zoom::fit(nat, VP));
        z.zoom_at(1.25, (500.0, 500.0), nat, VP);
        z.toggle((500.0, 500.0), nat, VP);
        assert_eq!(z.percent(), 100);
    }

    #[test]
    fn relayout_keeps_fit_on_resize() {
        let nat = (2000.0, 1000.0);
        let mut z = Zoom::fit(nat, VP);
        z.relayout(nat, (500.0, 500.0));
        assert_eq!(z, Zoom::fit(nat, (500.0, 500.0)));
        z.zoom_at(4.0, (250.0, 250.0), nat, (500.0, 500.0));
        z.relayout(nat, (800.0, 800.0));
        assert!(!z.fit);
        assert!(z.origin.0 <= 0.0 && z.origin.0 + nat.0 * z.scale >= 800.0);
    }

    #[test]
    fn minimap_is_none_at_fit_and_a_fraction_zoomed_in() {
        let nat = (2000.0, 1000.0);
        let mut z = Zoom::fit(nat, VP);
        assert_eq!(z.minimap(nat, VP), None);
        z.toggle((500.0, 500.0), nat, VP);
        let m = z.minimap(nat, VP).unwrap();
        assert!((m[2] - 0.5).abs() < 1e-4);
        assert_eq!(m[3], 1.0);
    }

    #[test]
    fn wheel_factor_one_line_is_one_step() {
        assert_eq!(wheel_factor(-50.0), 1.25);
        assert!((wheel_factor(50.0) - 0.8).abs() < 1e-6);
    }
}
