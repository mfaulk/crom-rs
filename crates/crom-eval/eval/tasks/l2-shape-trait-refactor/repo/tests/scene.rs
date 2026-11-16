use shapes::{Circle, Rect, Scene, Shape, Square, Triangle};

fn scene() -> Scene {
    let mut s = Scene::new();
    s.add(Rect::new(2.0, 3.0));
    s.add(Circle::new(1.0));
    s.add(Triangle::new(3.0, 4.0, 5.0).unwrap());
    s.add(Square::new(2.0));
    s
}

#[test]
fn areas() {
    assert_eq!(Rect::new(2.0, 3.0).area(), 6.0);
    assert_eq!(Triangle::new(3.0, 4.0, 5.0).unwrap().area(), 6.0);
    assert!((Circle::new(1.0).area() - std::f64::consts::PI).abs() < 1e-9);
    assert_eq!(Square::new(2.0).area(), 4.0);
}

#[test]
fn invalid_triangle_is_rejected() {
    assert!(Triangle::new(1.0, 1.0, 3.0).is_none());
}

#[test]
fn largest_prefers_first_on_tie() {
    assert_eq!(scene().largest(), Some("rect".to_string()));
    assert_eq!(Scene::new().largest(), None);
}

#[test]
fn names() {
    let names: Vec<String> = vec![
        Circle::new(1.0).name(),
        Rect::new(1.0, 1.0).name(),
        Square::new(1.0).name(),
    ];
    assert_eq!(names, ["circle", "rect", "square"]);
}

#[test]
fn describe_lists_every_shape() {
    let text = scene().describe();
    assert_eq!(text.lines().count(), 4);
    assert!(text.starts_with("rect: area=6.00"));
}
