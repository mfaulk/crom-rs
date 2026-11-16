use crate::shape::Shape;

/// An ordered collection of shapes.
#[derive(Default)]
pub struct Scene {
    shapes: Vec<Box<dyn Shape>>,
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, shape: impl Shape + 'static) {
        self.shapes.push(Box::new(shape));
    }

    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    pub fn total_area(&self) -> f64 {
        self.shapes.iter().map(|s| s.area()).sum()
    }

    /// Name of the shape with the largest area (first one wins ties).
    pub fn largest(&self) -> Option<String> {
        let mut best: Option<&dyn Shape> = None;
        for s in &self.shapes {
            if best.is_none_or(|b| s.area() > b.area()) {
                best = Some(s.as_ref());
            }
        }
        best.map(|s| s.name())
    }

    /// One line per shape: `<name>: area=<area>` with two decimals.
    pub fn describe(&self) -> String {
        self.shapes
            .iter()
            .map(|s| format!("{}: area={:.2}\n", s.name(), s.area()))
            .collect()
    }
}
