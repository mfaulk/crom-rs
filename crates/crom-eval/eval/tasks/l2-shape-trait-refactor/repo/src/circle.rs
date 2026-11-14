use crate::shape::Shape;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Circle {
    pub radius: f64,
}

impl Circle {
    pub fn new(radius: f64) -> Self {
        assert!(radius >= 0.0, "radius must be non-negative");
        Self { radius }
    }
}

impl Shape for Circle {
    fn area(&self) -> f64 {
        std::f64::consts::PI * self.radius * self.radius
    }

    fn name(&self) -> String {
        "circle".to_string()
    }
}
