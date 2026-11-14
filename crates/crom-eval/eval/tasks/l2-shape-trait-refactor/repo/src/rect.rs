use crate::shape::Shape;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(width: f64, height: f64) -> Self {
        assert!(width >= 0.0 && height >= 0.0, "sides must be non-negative");
        Self { width, height }
    }
}

impl Shape for Rect {
    fn area(&self) -> f64 {
        self.width * self.height
    }

    fn name(&self) -> String {
        "rect".to_string()
    }
}

/// A square is a rectangle with equal sides; it delegates to [`Rect`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Square(Rect);

impl Square {
    pub fn new(side: f64) -> Self {
        Self(Rect::new(side, side))
    }

    pub fn side(&self) -> f64 {
        self.0.width
    }
}

impl Shape for Square {
    fn area(&self) -> f64 {
        self.0.area()
    }

    fn name(&self) -> String {
        "square".to_string()
    }
}
