use shapes::{Circle, Rect, Scene, Shape, Square, Triangle};

fn static_name(s: &dyn Shape) -> &'static str {
    s.name()
}

struct Unit;

// Only the three required methods; `describe` must be a provided method.
impl Shape for Unit {
    fn area(&self) -> f64 {
        1.0
    }
    fn name(&self) -> &'static str {
        "unit"
    }
    fn perimeter(&self) -> f64 {
        4.0
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn names_are_static() {
    assert_eq!(static_name(&Circle::new(1.0)), "circle");
    assert_eq!(static_name(&Rect::new(1.0, 2.0)), "rect");
    assert_eq!(static_name(&Square::new(1.0)), "square");
    assert_eq!(
        static_name(&Triangle::new(3.0, 4.0, 5.0).unwrap()),
        "triangle"
    );
}

#[test]
fn perimeters() {
    assert!(close(
        Circle::new(1.0).perimeter(),
        2.0 * std::f64::consts::PI
    ));
    assert!(close(Rect::new(2.0, 3.0).perimeter(), 10.0));
    assert!(close(Square::new(2.5).perimeter(), 10.0));
    assert!(close(
        Triangle::new(3.0, 4.0, 5.0).unwrap().perimeter(),
        12.0
    ));
}

#[test]
fn default_describe() {
    assert_eq!(Unit.describe(), "unit: area=1.00 perimeter=4.00");
    assert_eq!(
        Rect::new(2.0, 3.0).describe(),
        "rect: area=6.00 perimeter=10.00"
    );
}

#[test]
fn scene_aggregates() {
    let mut s = Scene::new();
    s.add(Rect::new(2.0, 3.0));
    s.add(Unit);
    s.add(Square::new(3.0));
    assert!(close(s.total_perimeter(), 10.0 + 4.0 + 12.0));
    assert_eq!(s.largest(), Some("square"));
    assert_eq!(
        s.describe(),
        "rect: area=6.00 perimeter=10.00\nunit: area=1.00 perimeter=4.00\nsquare: area=9.00 perimeter=12.00\n"
    );
}
