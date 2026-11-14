//! Next-run computation and a clock-driven job runner.

use calendar::DateTime;
use cronexpr::{CronError, CronExpr};

/// Upper bound on the search for the next run (a little over four years, so
/// `0 0 29 2 *` is always found).
const MAX_DAYS: usize = 366 * 4 + 2;

/// The first time strictly after `after` that matches `expr`.
pub fn next_run(expr: &CronExpr, after: DateTime) -> Option<DateTime> {
    let mut t = after.succ();
    for _ in 0..MAX_DAYS {
        let day_ok = expr.months.contains(&t.month)
            && expr.matches(
                *expr.minutes.iter().next()?,
                *expr.hours.iter().next()?,
                t.day,
                t.month,
                t.weekday(),
            );
        if day_ok {
            loop {
                if expr.matches(t.minute, t.hour, t.day, t.month, t.weekday()) {
                    return Some(t);
                }
                let n = t.succ();
                if n.day != t.day {
                    t = n;
                    break;
                }
                t = n;
            }
        } else {
            t = t.next_day();
        }
    }
    None
}

/// The next `n` run times after `after`.
pub fn upcoming(expr: &CronExpr, after: DateTime, n: usize) -> Vec<DateTime> {
    let mut out = Vec::with_capacity(n);
    let mut t = after;
    while out.len() < n {
        match next_run(expr, t) {
            Some(next) => {
                out.push(next);
                t = next;
            }
            None => break,
        }
    }
    out
}

pub struct Job {
    pub name: String,
    pub expr: CronExpr,
    pub next: Option<DateTime>,
}

#[derive(Default)]
pub struct Scheduler {
    jobs: Vec<Job>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: &str, expr: &str, now: DateTime) -> Result<(), CronError> {
        let expr = CronExpr::parse(expr)?;
        let next = next_run(&expr, now);
        self.jobs.push(Job {
            name: name.to_string(),
            expr,
            next,
        });
        Ok(())
    }

    /// Names of jobs due at or before `now`, in registration order; each due
    /// job is rescheduled after `now`.
    pub fn tick(&mut self, now: DateTime) -> Vec<String> {
        let mut due = Vec::new();
        for job in &mut self.jobs {
            if matches!(job.next, Some(n) if n <= now) {
                due.push(job.name.clone());
                job.next = next_run(&job.expr, now);
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime {
        DateTime::parse(s).unwrap()
    }

    #[test]
    fn daily_at_nine_thirty() {
        let e = CronExpr::parse("30 9 * * *").unwrap();
        assert_eq!(
            next_run(&e, t("2026-03-01 08:00")),
            Some(t("2026-03-01 09:30"))
        );
        assert_eq!(
            next_run(&e, t("2026-03-01 09:30")),
            Some(t("2026-03-02 09:30"))
        );
    }

    #[test]
    fn leap_day() {
        let e = CronExpr::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            next_run(&e, t("2025-01-01 00:00")),
            Some(t("2028-02-29 00:00"))
        );
    }

    #[test]
    fn scheduler_ticks() {
        let mut s = Scheduler::new();
        s.add("report", "0 12 * * *", t("2026-01-05 11:00"))
            .unwrap();
        s.add("monday", "0 8 * * 1", t("2026-01-05 11:00")).unwrap();
        assert!(s.tick(t("2026-01-05 11:59")).is_empty());
        assert_eq!(s.tick(t("2026-01-05 12:00")), ["report"]);
        assert_eq!(s.tick(t("2026-01-12 08:00")), ["report", "monday"]);
    }
}
