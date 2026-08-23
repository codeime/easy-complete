//! Logical/physical screen geometry for overlay placement.
//!
//! These used to be `tao::dpi` types left over from the wry host. The GPUI
//! host only converts at scale 1.0 in overlay layout, and each caret source
//! already knows whether it emits logical or physical pixels.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LogicalPosition {
    pub x: f64,
    pub y: f64,
}

impl LogicalPosition {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PhysicalPosition {
    pub x: i32,
    pub y: i32,
}

impl PhysicalPosition {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    pub fn to_logical(self, scale_factor: f64) -> LogicalPosition {
        LogicalPosition {
            x: self.x as f64 / scale_factor,
            y: self.y as f64 / scale_factor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Position {
    Physical(PhysicalPosition),
    Logical(LogicalPosition),
}

impl Position {
    pub fn to_logical(self, scale_factor: f64) -> LogicalPosition {
        match self {
            Position::Logical(p) => p,
            Position::Physical(p) => p.to_logical(scale_factor),
        }
    }
}

impl From<LogicalPosition> for Position {
    fn from(position: LogicalPosition) -> Self {
        Position::Logical(position)
    }
}

impl From<PhysicalPosition> for Position {
    fn from(position: PhysicalPosition) -> Self {
        Position::Physical(position)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LogicalSize {
    pub width: f64,
    pub height: f64,
}

impl LogicalSize {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PhysicalSize {
    pub width: i32,
    pub height: i32,
}

impl PhysicalSize {
    pub const fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }

    pub fn to_logical(self, scale_factor: f64) -> LogicalSize {
        LogicalSize {
            width: self.width as f64 / scale_factor,
            height: self.height as f64 / scale_factor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Size {
    Physical(PhysicalSize),
    Logical(LogicalSize),
}

impl Size {
    pub fn to_logical(self, scale_factor: f64) -> LogicalSize {
        match self {
            Size::Logical(s) => s,
            Size::Physical(s) => s.to_logical(scale_factor),
        }
    }
}

impl From<LogicalSize> for Size {
    fn from(size: LogicalSize) -> Self {
        Size::Logical(size)
    }
}

impl From<PhysicalSize> for Size {
    fn from(size: PhysicalSize) -> Self {
        Size::Physical(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_to_logical_at_identity_scale_keeps_integer_coords() {
        let position = PhysicalPosition::new(100, 200).to_logical(1.0);
        assert_eq!(position, LogicalPosition::new(100.0, 200.0));
        let size = PhysicalSize::new(8, 16).to_logical(1.0);
        assert_eq!(size, LogicalSize::new(8.0, 16.0));
    }

    #[test]
    fn physical_to_logical_divides_by_scale() {
        let position = Position::Physical(PhysicalPosition::new(100, 200)).to_logical(2.0);
        assert_eq!(position, LogicalPosition::new(50.0, 100.0));
        let size = Size::Physical(PhysicalSize::new(8, 16)).to_logical(2.0);
        assert_eq!(size, LogicalSize::new(4.0, 8.0));
    }

    #[test]
    fn logical_to_logical_is_a_copy() {
        let position = Position::Logical(LogicalPosition::new(12.5, 18.0)).to_logical(2.0);
        assert_eq!(position, LogicalPosition::new(12.5, 18.0));
    }
}
