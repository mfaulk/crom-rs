use calendar::DateTime;
use cronexpr::CronExpr;
use scheduler::{next_run, upcoming, Scheduler};

fn t(s: &str) -> DateTime {
    DateTime::parse(s).unwrap()
}

#[test]
fn weekday_business_hours() {
    // 2026-11-13 is a Friday.
    let e = CronExpr::parse("*/20 9-10 * * 1-5").unwrap();
    let got: Vec<String> = upcoming(&e, t("2026-11-13 10:30"), 4)
        .iter()
        .map(|d| d.to_string())
        .collect();
    assert_eq!(
        got,
        [
            "2026-11-13 10:40",
            "2026-11-16 09:00",
            "2026-11-16 09:20",
            "2026-11-16 09:40"
        ]
    );
}

#[test]
fn quarterly_first_of_month() {
    let e = CronExpr::parse("0 6 1 */3 *").unwrap();
    assert_eq!(
        next_run(&e, t("2026-02-10 00:00")),
        Some(t("2026-04-01 06:00"))
    );
}

#[test]
fn scheduler_accepts_new_syntax() {
    let mut s = Scheduler::new();
    s.add("poll", "*/5 * * * *", t("2026-01-01 00:02")).unwrap();
    assert!(s.tick(t("2026-01-01 00:04")).is_empty());
    assert_eq!(s.tick(t("2026-01-01 00:05")), ["poll"]);
}
