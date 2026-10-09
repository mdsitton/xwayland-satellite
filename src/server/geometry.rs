//! Sizes and positions in the two coordinate spaces satellite converts between.
//!
//! X windows are measured in device pixels, while the compositor configures surfaces, viewports
//! and popup positioners in logical pixels. Mixing the two is the source of most of the rounding
//! bugs here, so the space is part of the type, and every conversion between them says which way
//! it rounds and why.

use std::marker::PhantomData;

/// Logical pixels: what the compositor configures, and what viewports, positioners and size
/// hints are expressed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Logical;

/// Device pixels: what X windows and the buffers backing them are measured in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Physical;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Size<Unit> {
    pub width: i32,
    pub height: i32,
    _unit: PhantomData<Unit>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Point<Unit> {
    pub x: i32,
    pub y: i32,
    _unit: PhantomData<Unit>,
}

impl<Unit> Size<Unit> {
    pub(crate) const fn new(width: i32, height: i32) -> Self {
        Self {
            width,
            height,
            _unit: PhantomData,
        }
    }
}

impl Size<Logical> {
    /// The X size of a surface configured at this size. Truncated, so that the X window never
    /// exceeds the logical size it came from and `Size::<Physical>::to_logical` returns to it
    /// exactly.
    pub(crate) fn to_physical(self, scale: f64) -> Size<Physical> {
        Size::new(
            (self.width as f64 * scale) as i32,
            (self.height as f64 * scale) as i32,
        )
    }
}

impl Size<Physical> {
    /// The logical size covering the whole of this size, rounded up so that no part of the
    /// buffer is left outside it. This is what a viewport destination and a popup's positioner
    /// must use, or the two disagree at fractional scales.
    pub(crate) fn to_logical(self, scale: f64) -> Size<Logical> {
        Size::new(
            (self.width as f64 / scale).ceil() as i32,
            (self.height as f64 / scale).ceil() as i32,
        )
    }

    /// The largest logical size that still fits within this one, for a bound that must not
    /// exceed it, such as a maximum size hint.
    pub(crate) fn to_logical_bound(self, scale: f64) -> Size<Logical> {
        Size::new(
            (self.width as f64 / scale) as i32,
            (self.height as f64 / scale) as i32,
        )
    }

    /// The size as X sends and expects it, clamped to what the protocol can carry.
    pub(crate) fn to_x(self) -> (u16, u16) {
        (
            self.width.clamp(0, u16::MAX as i32) as u16,
            self.height.clamp(0, u16::MAX as i32) as u16,
        )
    }

    pub(crate) fn from_x(width: u16, height: u16) -> Self {
        Self::new(width.into(), height.into())
    }
}

impl<Unit> Point<Unit> {
    pub(crate) const fn new(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            _unit: PhantomData,
        }
    }
}

impl Point<Logical> {
    /// Truncates towards zero, so a position above or left of its reference keeps the same
    /// distance from it as the mirrored position below or right.
    pub(crate) fn to_physical(self, scale: f64) -> Point<Physical> {
        Point::new(
            (self.x as f64 * scale) as i32,
            (self.y as f64 * scale) as i32,
        )
    }
}

impl Point<Physical> {
    /// Moves the point so that a window of the given size placed there keeps at least an edge
    /// inside a screen of `bounds`. X delivers no pointer input to the parts of a window that
    /// fall outside the root window, so a window placed beyond the screen is dead to the
    /// mouse; an overhang is left alone, as it would be on an X server with a real root.
    pub(crate) fn clamp_into(self, size: Size<Physical>, bounds: Size<Physical>) -> Self {
        /// How much of the window has to stay on screen.
        const VISIBLE: i32 = 64;
        let limit = |pos: i32, extent: i32, bound: i32| {
            // With no screen to speak of, there is nothing to clamp into.
            if bound <= 0 {
                return pos;
            }
            pos.clamp(0, (bound - extent.min(VISIBLE)).max(0))
        };
        Self::new(
            limit(self.x, size.width, bounds.width),
            limit(self.y, size.height, bounds.height),
        )
    }

    /// This position, which is relative to the given output, in the X root's coordinates.
    pub(crate) fn to_root(self, output: Point<Physical>) -> Self {
        Self::new(self.x + output.x, self.y + output.y)
    }

    /// This position in logical pixels, for talking to the compositor about it.
    ///
    /// The compositor works in fractions of a logical pixel, so this keeps the fraction rather
    /// than rounding to whole pixels as the conversions towards X do.
    pub(crate) fn to_logical_f64(self, scale: f64) -> (f64, f64) {
        (f64::from(self.x) / scale, f64::from(self.y) / scale)
    }

    /// How far this position is from another in the same space.
    pub(crate) fn delta_from(self, origin: Self) -> Self {
        Self::new(self.x - origin.x, self.y - origin.y)
    }

    /// The position as X sends and expects it, clamped to what the protocol can carry.
    pub(crate) fn to_x(self) -> (i16, i16) {
        (
            self.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            self.y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        )
    }

    pub(crate) fn from_x(x: i16, y: i16) -> Self {
        Self::new(x.into(), y.into())
    }
}
