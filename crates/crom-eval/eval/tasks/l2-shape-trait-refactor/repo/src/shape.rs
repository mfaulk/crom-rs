/// A closed 2D shape.
pub trait Shape {
    /// Area in square units.
    fn area(&self) -> f64;

    /// Lower-case kind name, e.g. `"circle"`.
    fn name(&self) -> String;
}
