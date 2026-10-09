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
