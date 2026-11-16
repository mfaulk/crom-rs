use cronexpr::{CronError, CronExpr, Field};

fn v(set: &std::collections::BTreeSet<u8>) -> Vec<u8> {
    set.iter().copied().collect()
}

#[test]
fn steps_from_star() {
    let e = CronExpr::parse("*/15 */6 */10 */5 */2").unwrap();
    assert_eq!(v(&e.minutes), [0, 15, 30, 45]);
    assert_eq!(v(&e.hours), [0, 6, 12, 18]);
    assert_eq!(v(&e.days_of_month), [1, 11, 21, 31]);
    assert_eq!(v(&e.months), [1, 6, 11]);
    assert_eq!(v(&e.days_of_week), [0, 2, 4, 6]);
}

#[test]
fn ranges_and_stepped_ranges() {
    let e = CronExpr::parse("5-10,30 9-17/2 1-3 6-8 1-5").unwrap();
    assert_eq!(v(&e.minutes), [5, 6, 7, 8, 9, 10, 30]);
    assert_eq!(v(&e.hours), [9, 11, 13, 15, 17]);
    assert_eq!(v(&e.days_of_month), [1, 2, 3]);
    assert_eq!(v(&e.months), [6, 7, 8]);
    assert_eq!(v(&e.days_of_week), [1, 2, 3, 4, 5]);
    assert!(!e.dom_star && !e.dow_star);
    let e = CronExpr::parse("0-59/20 0 * * *").unwrap();
    assert_eq!(v(&e.minutes), [0, 20, 40]);
    assert!(e.dom_star && e.dow_star);
}

#[test]
fn invalid_items() {
    for (expr, field) in [
        ("*/0 * * * *", Field::Minute),
        ("* 10-5 * * *", Field::Hour),
        ("* * 1-2-3 * *", Field::DayOfMonth),
        ("* * * 1-/2 *", Field::Month),
        ("* * * * a-b", Field::DayOfWeek),
    ] {
        match CronExpr::parse(expr) {
            Err(CronError::Invalid { field: f, .. }) => assert_eq!(f, field, "{expr}"),
            other => panic!("{expr}: expected Invalid, got {other:?}"),
        }
    }
}

#[test]
fn out_of_range_endpoints() {
    assert_eq!(
        CronExpr::parse("0-60 * * * *"),
        Err(CronError::OutOfRange {
            field: Field::Minute,
            value: 60
        })
    );
    assert_eq!(
        CronExpr::parse("* * 0-5/2 * *"),
        Err(CronError::OutOfRange {
            field: Field::DayOfMonth,
            value: 0
        })
    );
}
