/// A rectangle. The field was renamed from `w`/`h` to `width`/`height`, but
/// `area` below still references the old names, so the crate does not compile.
pub struct Rect {
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub fn area(&self) -> u32 {
        // Bug: uses the old field names `w` and `h`.
        self.w * self.h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_area() {
        assert_eq!(Rect::new(3, 4).area(), 12);
    }
}
