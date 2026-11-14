use crate::shape::Shape;

/// A triangle given by its three side lengths.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triangle {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

impl Triangle {
    /// Returns `None` when the sides violate the triangle inequality.
    pub fn new(a: f64, b: f64, c: f64) -> Option<Self> {
        let ok = a > 0.0 && b > 0.0 && c > 0.0 && a + b > c && a + c > b && b + c > a;
        ok.then_some(Self { a, b, c })
    }
}

impl Shape for Triangle {
    /// Heron's formula.
    fn area(&self) -> f64 {
        let s = (self.a + self.b + self.c) / 2.0;
        (s * (s - self.a) * (s - self.b) * (s - self.c)).sqrt()
    }

    fn name(&self) -> String {
        "triangle".to_string()
    }
}
