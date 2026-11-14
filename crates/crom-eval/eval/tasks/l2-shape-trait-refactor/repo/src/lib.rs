//! Geometric shapes and a scene that aggregates them.

pub mod circle;
pub mod rect;
pub mod scene;
pub mod shape;
pub mod triangle;

pub use circle::Circle;
pub use rect::{Rect, Square};
pub use scene::Scene;
pub use shape::Shape;
pub use triangle::Triangle;
