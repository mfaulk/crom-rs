use rect::Rect;

/// `area` multiplies width by height, including zero-sized rectangles.
#[test]
fn area_is_width_times_height() {
    assert_eq!(Rect::new(0, 5).area(), 0);
    assert_eq!(Rect::new(7, 1).area(), 7);
    assert_eq!(Rect::new(10, 20).area(), 200);
}

/// The public fields keep their new `width` / `height` names.
#[test]
fn fields_keep_new_names() {
    let r = Rect {
        width: 2,
        height: 3,
    };
    assert_eq!((r.width, r.height), (2, 3));
    assert_eq!(r.area(), 6);
}
