//! The viewer's surface and how the target's desktop is drawn on it.
//!
//! The viewer's surface is the size the viewer last asked for: Warpgate answers every
//! request itself, whether or not the target can resize. While the target's desktop has
//! another size it is scaled into the surface, keeping its aspect ratio and centred between
//! black bars; once the target follows the viewer the two match and it is drawn 1:1.

use ironrdp_server::DesktopSize;

/// A desktop within this factor of the surface it fits in is drawn 1:1 between bars: a
/// target that followed the viewer only up to its own rounding is not worth a blur.
const UNIT_SCALE_SLACK: (u32, u32) = (11, 10);

/// The fixed-point one of a resampling weight. Two axes multiply to `WEIGHT_ONE²`, which
/// times 255 still fits a `u32` accumulator.
const WEIGHT_ONE: u32 = 256;

/// The weight of a whole pixel in both axes.
const WEIGHT_AREA: u32 = WEIGHT_ONE * WEIGHT_ONE;

const BLACK: [u8; 4] = [0, 0, 0, 0xff];

/// Where the desktop goes on the viewer's surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Placement {
    /// The viewer's surface.
    pub output: DesktopSize,
    /// The target's desktop.
    pub desktop: DesktopSize,
    /// The desktop's size on the surface.
    pub scaled: DesktopSize,
    /// The scaled desktop's top-left corner on the surface.
    pub origin: (u16, u16),
}

impl Placement {
    /// The desktop as it is, on a surface of its own size.
    pub(super) const fn identity(desktop: DesktopSize) -> Self {
        Self {
            output: desktop,
            desktop,
            scaled: desktop,
            origin: (0, 0),
        }
    }

    /// The largest size of the desktop's shape that fits `output`, centred in it; 1:1 when
    /// the desktop already fits with little to spare.
    pub(super) fn fit(desktop: DesktopSize, output: DesktopSize) -> Self {
        if desktop.width == 0 || desktop.height == 0 || output.width == 0 || output.height == 0 {
            return Self::identity(desktop);
        }
        let (dw, dh) = (u32::from(desktop.width), u32::from(desktop.height));
        let (ow, oh) = (u32::from(output.width), u32::from(output.height));
        let fits = dw <= ow && dh <= oh;
        let (slack_num, slack_den) = UNIT_SCALE_SLACK;
        let close = dw * slack_num >= ow * slack_den || dh * slack_num >= oh * slack_den;
        let (sw, sh) = if fits && close {
            (dw, dh)
        } else if ow * dh <= oh * dw {
            // Width-bound.
            (ow, ((dh * ow + dw / 2) / dw).clamp(1, oh))
        } else {
            (((dw * oh + dh / 2) / dh).clamp(1, ow), oh)
        };
        // Both are at most the output's sides, which are `u16`.
        let scaled = DesktopSize {
            width: u16::try_from(sw).unwrap_or(output.width),
            height: u16::try_from(sh).unwrap_or(output.height),
        };
        Self {
            output,
            desktop,
            scaled,
            origin: (
                (output.width - scaled.width) / 2,
                (output.height - scaled.height) / 2,
            ),
        }
    }

    /// The desktop is drawn pixel for pixel (bars or not).
    pub(super) fn is_unit(self) -> bool {
        self.scaled == self.desktop
    }

    /// The desktop is the whole surface, pixel for pixel.
    pub(super) fn is_identity(self) -> bool {
        self.is_unit() && self.output == self.desktop
    }

    /// The scale factor, for logs.
    pub(super) fn scale(self) -> f32 {
        if self.desktop.width == 0 {
            return 1.0;
        }
        f32::from(self.scaled.width) / f32::from(self.desktop.width)
    }

    /// The surface pixel the viewer pointed at, on the desktop. Points on the bars go to the
    /// nearest edge of the desktop.
    pub(super) fn to_desktop(self, x: u16, y: u16) -> (u16, u16) {
        let axis = |at: u16, origin: u16, scaled: u16, desktop: u16| -> u16 {
            if scaled == 0 || desktop == 0 {
                return 0;
            }
            let at = u32::from(at.saturating_sub(origin).min(scaled - 1));
            if scaled == desktop {
                return at as u16;
            }
            let mapped = ((2 * at + 1) * u32::from(desktop)) / (2 * u32::from(scaled));
            mapped.min(u32::from(desktop) - 1) as u16
        };
        (
            axis(x, self.origin.0, self.scaled.width, self.desktop.width),
            axis(y, self.origin.1, self.scaled.height, self.desktop.height),
        )
    }

    /// The surface rectangle (exclusive edges) that depends on the desktop rectangle
    /// `[left, right) x [top, bottom)`.
    pub(super) fn to_output(self, left: u16, top: u16, right: u16, bottom: u16) -> Rect {
        let axis = |from: u16, to: u16, scaled: u16, desktop: u16, origin: u16| -> (u16, u16) {
            if scaled == desktop {
                return (from + origin, to + origin);
            }
            let (scaled, desktop) = (u32::from(scaled), u32::from(desktop));
            // One source pixel of margin on each side covers both filters' reach.
            let first = (u32::from(from).saturating_sub(1) * scaled) / desktop;
            let last = ((u32::from(to) + 1) * scaled).div_ceil(desktop).min(scaled);
            (first as u16 + origin, last as u16 + origin)
        };
        let (left, right) = axis(
            left,
            right,
            self.scaled.width,
            self.desktop.width,
            self.origin.0,
        );
        let (top, bottom) = axis(
            top,
            bottom,
            self.scaled.height,
            self.desktop.height,
            self.origin.1,
        );
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }
}

/// A rectangle on the surface, exclusive edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Rect {
    pub(super) const fn is_empty(self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    pub(super) const fn width(self) -> u16 {
        self.right.saturating_sub(self.left)
    }

    pub(super) const fn height(self) -> u16 {
        self.bottom.saturating_sub(self.top)
    }
}

/// One surface column's (or row's) source pixels: `weights[i]` is the share of source pixel
/// `first + i`, and the shares add up to [`WEIGHT_ONE`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tap {
    first: usize,
    weights: Vec<u32>,
}

/// The taps of every pixel along one axis: an area average when shrinking (every source
/// pixel counts by how much of it the surface pixel covers), bilinear when growing.
fn taps(source: u16, scaled: u16) -> Vec<Tap> {
    let (source, scaled) = (usize::from(source), usize::from(scaled));
    if source == 0 || scaled == 0 {
        return Vec::new();
    }
    let one = WEIGHT_ONE as usize;
    (0..scaled)
        .map(|at| {
            // (source pixel, share), shares adding up to `total`.
            let (shares, total): (Vec<(usize, usize)>, usize) = match scaled.cmp(&source) {
                core::cmp::Ordering::Equal => (vec![(at, 1)], 1),
                core::cmp::Ordering::Less => {
                    // In units of 1/scaled of a source pixel: source pixel i spans
                    // [i * scaled, (i + 1) * scaled), this surface pixel [at * source, (at + 1) * source).
                    let (from, to) = (at * source, (at + 1) * source);
                    let shares = (from / scaled..to.div_ceil(scaled).min(source))
                        .map(|i| (i, to.min((i + 1) * scaled) - from.max(i * scaled)))
                        .collect();
                    (shares, source)
                }
                core::cmp::Ordering::Greater => {
                    // The centre of this pixel on the source, less half a pixel, over 2 * scaled.
                    let span = 2 * scaled;
                    let centre = ((2 * at + 1) * source).saturating_sub(scaled);
                    let (i, rest) = (centre / span, centre % span);
                    if i + 1 >= source {
                        (vec![(source - 1, 1)], 1)
                    } else {
                        (vec![(i, span - rest), (i + 1, rest)], span)
                    }
                }
            };
            let shares: Vec<_> = shares.into_iter().filter(|(_, share)| *share > 0).collect();
            let first = shares.first().map_or(0, |(i, _)| *i);
            let mut weights = vec![0u32; shares.last().map_or(0, |(i, _)| i + 1 - first)];
            for (i, share) in &shares {
                if let Some(weight) = weights.get_mut(i - first) {
                    *weight = u32::try_from((share * one + total / 2) / total).unwrap_or(0);
                }
            }
            // Rounding can miss the total by a step; the largest share absorbs it.
            let sum: u32 = weights.iter().sum();
            if let Some(largest) = weights.iter_mut().max() {
                *largest = (*largest + WEIGHT_ONE).saturating_sub(sum);
            }
            Tap { first, weights }
        })
        .collect()
}

/// Draws the desktop on the viewer's surface.
#[derive(Debug, Clone)]
pub(super) struct Painter {
    placement: Placement,
    columns: Vec<Tap>,
    rows: Vec<Tap>,
}

impl Painter {
    pub(super) fn new(placement: Placement) -> Self {
        let (columns, rows) = if placement.is_unit() {
            (Vec::new(), Vec::new())
        } else {
            (
                taps(placement.desktop.width, placement.scaled.width),
                taps(placement.desktop.height, placement.scaled.height),
            )
        };
        Self {
            placement,
            columns,
            rows,
        }
    }

    pub(super) const fn placement(&self) -> &Placement {
        &self.placement
    }

    /// The surface's BGRA pixels in `rect`, drawn from `desktop` (BGRA rows of
    /// `placement.desktop`); black around the desktop.
    pub(super) fn paint(&self, desktop: &[u8], rect: Rect) -> Vec<u8> {
        let width = usize::from(rect.width());
        let mut out = BLACK.repeat(width * usize::from(rect.height()));
        let Placement {
            desktop: size,
            scaled,
            origin: (left, top),
            ..
        } = self.placement;
        let stride = usize::from(size.width) * 4;
        if desktop.len() < stride * usize::from(size.height) {
            return out;
        }
        // The surface columns the desktop covers in `rect`.
        let from = rect.left.max(left);
        let to = rect.right.min(left + scaled.width);
        if from >= to {
            return out;
        }
        let into = usize::from(from - rect.left) * 4;
        let columns = usize::from(to - from) * 4;
        for (row, y) in (rect.top..rect.bottom).enumerate() {
            let Some(v) = y.checked_sub(top).filter(|v| *v < scaled.height) else {
                continue;
            };
            let start = row * width * 4 + into;
            let Some(dst) = out.get_mut(start..start + columns) else {
                continue;
            };
            if self.placement.is_unit() {
                let at = usize::from(v) * stride + usize::from(from - left) * 4;
                if let Some(src) = desktop.get(at..at + columns) {
                    dst.copy_from_slice(src);
                }
                continue;
            }
            let Some(row_tap) = self.rows.get(usize::from(v)) else {
                continue;
            };
            for (pixel, u) in dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(from - left..to - left)
            {
                let Some(column_tap) = self.columns.get(usize::from(u)) else {
                    continue;
                };
                let mut sum = [0u32; 3];
                for (j, wy) in row_tap.weights.iter().enumerate() {
                    let base = (row_tap.first + j) * stride + column_tap.first * 4;
                    for (i, wx) in column_tap.weights.iter().enumerate() {
                        let weight = wy * wx;
                        if let Some(src) = desktop.get(base + i * 4..base + i * 4 + 3) {
                            for (channel, value) in sum.iter_mut().zip(src) {
                                *channel += weight * u32::from(*value);
                            }
                        }
                    }
                }
                for (channel, value) in pixel.iter_mut().zip(sum) {
                    *channel = ((value + WEIGHT_AREA / 2) / WEIGHT_AREA).min(255) as u8;
                }
                pixel[3] = 0xff;
            }
        }
        out
    }
}

/// What the viewer asked for its surface to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Request {
    /// The size it connected with; the connection is negotiated at it, so nothing is sent.
    Initial,
    /// A Display Control monitor layout.
    Layout,
}

/// How a viewer's request is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Answer {
    /// Nothing to send.
    Keep,
    /// Reset the viewer's graphics to the surface (a ResetGraphics on the Graphics Pipeline;
    /// on the bitmap path a deactivation-reactivation, only if the size `changed`).
    Reset { changed: bool },
}

/// The viewer's surface: always the size the viewer last asked for.
///
/// Every layout is answered with a reset at exactly its size, even one of the size the
/// surface already has, since a viewer can take a layout as pending until a reset answers
/// it. Only a repeat of the layout just answered is let go, so a viewer that re-sends its
/// layout after every reset cannot start a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ViewerSurface {
    size: DesktopSize,
    answered: Option<DesktopSize>,
}

impl ViewerSurface {
    pub(super) const fn new(size: DesktopSize) -> Self {
        Self {
            size,
            answered: None,
        }
    }

    pub(super) const fn size(&self) -> DesktopSize {
        self.size
    }

    pub(super) fn request(&mut self, size: DesktopSize, request: Request) -> Answer {
        match request {
            Request::Initial => {
                self.size = size;
                self.answered = None;
                Answer::Keep
            }
            Request::Layout if self.answered == Some(size) && self.size == size => Answer::Keep,
            Request::Layout => {
                let changed = self.size != size;
                self.size = size;
                self.answered = Some(size);
                Answer::Reset { changed }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u16, height: u16) -> DesktopSize {
        DesktopSize { width, height }
    }

    #[test]
    fn a_landscape_desktop_on_a_portrait_phone_is_shrunk_to_its_width() {
        let placement = Placement::fit(size(1440, 900), size(560, 1058));
        assert_eq!(placement.scaled, size(560, 350));
        assert_eq!(placement.origin, (0, 354));
        assert!((placement.scale() - 0.3889).abs() < 0.001);
    }

    #[test]
    fn a_landscape_desktop_on_a_landscape_phone_is_shrunk_to_its_height() {
        let placement = Placement::fit(size(1440, 900), size(1058, 560));
        assert_eq!(placement.scaled, size(896, 560));
        assert_eq!(placement.origin, (81, 0));
    }

    #[test]
    fn a_desktop_of_the_surface_size_is_the_surface() {
        let placement = Placement::fit(size(1058, 560), size(1058, 560));
        assert!(placement.is_identity());
        assert_eq!(placement, Placement::identity(size(1058, 560)));
    }

    #[test]
    fn a_desktop_that_nearly_fits_is_drawn_one_to_one_between_bars() {
        // A target that followed up to an even width.
        let placement = Placement::fit(size(560, 1058), size(561, 1058));
        assert!(placement.is_unit());
        assert_eq!(placement.origin, (0, 0));
        let placement = Placement::fit(size(1000, 700), size(1050, 760));
        assert!(placement.is_unit());
        assert_eq!(placement.origin, (25, 30));
    }

    #[test]
    fn a_small_desktop_on_a_large_surface_is_grown() {
        let placement = Placement::fit(size(1024, 768), size(2560, 1600));
        assert_eq!(placement.scaled, size(2133, 1600));
        assert_eq!(placement.origin, (213, 0));
    }

    #[test]
    fn degenerate_sizes_are_drawn_as_they_are() {
        assert!(Placement::fit(size(0, 0), size(560, 1058)).is_identity());
        assert!(Placement::fit(size(800, 600), size(0, 0)).is_identity());
    }

    #[test]
    fn the_pointer_lands_on_the_desktop_pixel_under_it() {
        let placement = Placement::fit(size(1440, 900), size(560, 1058));
        assert_eq!(placement.to_desktop(0, 354), (1, 1));
        assert_eq!(placement.to_desktop(559, 703), (1438, 898));
        // On the bars: the nearest edge.
        assert_eq!(placement.to_desktop(280, 0).1, 1);
        assert_eq!(placement.to_desktop(280, 1057).1, 898);
        let identity = Placement::identity(size(800, 600));
        assert_eq!(identity.to_desktop(799, 599), (799, 599));
        assert_eq!(identity.to_desktop(900, 700), (799, 599));
    }

    #[test]
    fn every_axis_tap_adds_up_to_one() {
        for (source, scaled) in [(1440, 560), (900, 350), (900, 1600), (7, 3), (3, 7), (5, 5)] {
            let taps = taps(source, scaled);
            assert_eq!(taps.len(), usize::from(scaled));
            for tap in &taps {
                assert_eq!(tap.weights.iter().sum::<u32>(), WEIGHT_ONE);
                assert!(tap.first + tap.weights.len() <= usize::from(source));
            }
        }
    }

    fn bgra(width: u16, height: u16, seed: u32) -> Vec<u8> {
        (0..u32::from(width) * u32::from(height))
            .flat_map(|i| {
                let v = i.wrapping_mul(2_654_435_761).wrapping_add(seed);
                [(v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8, 0xff]
            })
            .collect()
    }

    fn full(placement: &Placement) -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: placement.output.width,
            bottom: placement.output.height,
        }
    }

    #[test]
    fn a_flat_colour_stays_that_colour_at_any_scale() {
        for (desktop, output) in [
            (size(1440, 900), size(560, 1058)),
            (size(320, 200), size(1058, 560)),
        ] {
            let painter = Painter::new(Placement::fit(desktop, output));
            let canvas = [10u8, 120, 250, 0xff]
                .repeat(usize::from(desktop.width) * usize::from(desktop.height));
            let out = painter.paint(&canvas, full(painter.placement()));
            let Placement {
                scaled,
                origin: (left, top),
                ..
            } = *painter.placement();
            for y in top..top + scaled.height {
                for x in left..left + scaled.width {
                    let at = (usize::from(y) * usize::from(output.width) + usize::from(x)) * 4;
                    assert_eq!(out[at..at + 4], [10, 120, 250, 0xff], "at {x},{y}");
                }
            }
            // The bars are black.
            assert_eq!(out[..4], BLACK);
        }
    }

    #[test]
    fn halving_averages_each_two_by_two_block() {
        let painter = Painter::new(Placement::fit(size(4, 2), size(2, 1)));
        let canvas = [
            [0, 0, 0, 0xff],
            [100, 100, 100, 0xff],
            [200, 200, 200, 0xff],
            [40, 40, 40, 0xff],
            [100, 100, 100, 0xff],
            [100, 100, 100, 0xff],
            [0, 0, 0, 0xff],
            [0, 0, 0, 0xff],
        ]
        .concat();
        let out = painter.paint(&canvas, full(painter.placement()));
        assert_eq!(out, [[75, 75, 75, 0xff], [60, 60, 60, 0xff]].concat());
    }

    #[test]
    fn a_unit_placement_copies_the_desktop_between_bars() {
        let desktop = size(4, 2);
        let painter = Painter::new(Placement::fit(desktop, size(4, 4)));
        assert!(painter.placement().is_unit());
        let canvas = bgra(4, 2, 1);
        let out = painter.paint(&canvas, full(painter.placement()));
        assert_eq!(out[4 * 4..12 * 4], canvas[..]);
        assert!(out[..4 * 4].chunks_exact(4).all(|pixel| pixel == BLACK));
    }

    /// Repainting only the surface rectangle a desktop change maps to gives the same surface
    /// as repainting all of it, at every scale.
    #[test]
    fn repainting_the_mapped_rectangle_matches_a_full_repaint() {
        for (desktop, output) in [
            (size(144, 90), size(56, 106)),
            (size(144, 90), size(106, 56)),
            (size(50, 30), size(213, 160)),
            (size(64, 48), size(64, 48)),
            (size(60, 40), size(64, 48)),
        ] {
            let painter = Painter::new(Placement::fit(desktop, output));
            let placement = *painter.placement();
            let mut canvas = bgra(desktop.width, desktop.height, 0);
            let mut surface = painter.paint(&canvas, full(&placement));
            for (step, (x, y, w, h)) in [
                (0, 0, 3, 2),
                (10, 7, 9, 5),
                (desktop.width - 1, desktop.height - 1, 1, 1),
                (20, 0, 1, desktop.height),
            ]
            .into_iter()
            .enumerate()
            {
                let patch = bgra(w, h, 99 + step as u32);
                for row in 0..usize::from(h) {
                    let at =
                        ((usize::from(y) + row) * usize::from(desktop.width) + usize::from(x)) * 4;
                    let len = usize::from(w) * 4;
                    canvas[at..at + len].copy_from_slice(&patch[row * len..(row + 1) * len]);
                }
                let rect = placement.to_output(x, y, x + w, y + h);
                let pixels = painter.paint(&canvas, rect);
                for row in 0..usize::from(rect.height()) {
                    let at = ((usize::from(rect.top) + row) * usize::from(output.width)
                        + usize::from(rect.left))
                        * 4;
                    let len = usize::from(rect.width()) * 4;
                    surface[at..at + len].copy_from_slice(&pixels[row * len..(row + 1) * len]);
                }
                assert_eq!(
                    surface,
                    painter.paint(&canvas, full(&placement)),
                    "{desktop:?} on {output:?}, step {step}"
                );
            }
        }
    }

    #[test]
    fn every_layout_is_answered_with_a_reset_at_its_size() {
        let mut surface = ViewerSurface::new(size(1024, 768));
        assert_eq!(
            surface.request(size(560, 1058), Request::Initial),
            Answer::Keep
        );
        assert_eq!(surface.size(), size(560, 1058));
        // The layout Windows App sends right after connecting repeats its size: it is
        // answered anyway.
        assert_eq!(
            surface.request(size(560, 1058), Request::Layout),
            Answer::Reset { changed: false }
        );
        assert_eq!(
            surface.request(size(560, 1058), Request::Layout),
            Answer::Keep
        );
        // Turned to landscape and back.
        assert_eq!(
            surface.request(size(1058, 560), Request::Layout),
            Answer::Reset { changed: true }
        );
        assert_eq!(surface.size(), size(1058, 560));
        assert_eq!(
            surface.request(size(560, 1058), Request::Layout),
            Answer::Reset { changed: true }
        );
        assert_eq!(surface.size(), size(560, 1058));
    }

    #[test]
    fn a_layout_back_at_an_answered_size_after_another_is_answered_again() {
        let mut surface = ViewerSurface::new(size(800, 600));
        surface.request(size(1058, 560), Request::Layout);
        surface.request(size(560, 1058), Request::Layout);
        assert_eq!(
            surface.request(size(1058, 560), Request::Layout),
            Answer::Reset { changed: true }
        );
    }
}
