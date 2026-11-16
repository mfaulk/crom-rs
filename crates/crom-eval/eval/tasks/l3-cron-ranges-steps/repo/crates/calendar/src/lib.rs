//! Minute-resolution UTC date-times.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
}

pub fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => panic!("invalid month {month}"),
    }
}

impl DateTime {
    pub fn new(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> Option<Self> {
        if !(1..=12).contains(&month)
            || day == 0
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
        {
            return None;
        }
        Some(DateTime {
            year,
            month,
            day,
            hour,
            minute,
        })
    }

    /// Parse `YYYY-MM-DD HH:MM`.
    pub fn parse(s: &str) -> Option<Self> {
        let (date, time) = s.trim().split_once(' ')?;
        let mut d = date.splitn(3, '-');
        let year = d.next()?.parse().ok()?;
        let month = d.next()?.parse().ok()?;
        let day = d.next()?.parse().ok()?;
        let (h, m) = time.split_once(':')?;
        DateTime::new(year, month, day, h.parse().ok()?, m.parse().ok()?)
    }

    /// Days since 1970-01-01 (negative before).
    pub fn days_since_epoch(&self) -> i64 {
        // Howard Hinnant's days_from_civil.
        let y = if self.month <= 2 {
            self.year - 1
        } else {
            self.year
        } as i64;
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let m = self.month as i64;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + self.day as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// 0 = Sunday … 6 = Saturday.
    pub fn weekday(&self) -> u8 {
        // 1970-01-01 was a Thursday.
        ((self.days_since_epoch() + 4).rem_euclid(7)) as u8
    }

    /// The next minute.
    pub fn succ(&self) -> DateTime {
        let mut t = *self;
        t.minute += 1;
        if t.minute == 60 {
            t.minute = 0;
            t.hour += 1;
        }
        if t.hour == 24 {
            t.hour = 0;
            t.day += 1;
        }
        if t.day > days_in_month(t.year, t.month) {
            t.day = 1;
            t.month += 1;
        }
        if t.month == 13 {
            t.month = 1;
            t.year += 1;
        }
        t
    }

    /// Midnight at the start of the following day.
    pub fn next_day(&self) -> DateTime {
        let mut t = *self;
        t.hour = 23;
        t.minute = 59;
        t.succ()
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekdays() {
        assert_eq!(DateTime::parse("1970-01-01 00:00").unwrap().weekday(), 4);
        assert_eq!(DateTime::parse("2024-02-29 12:00").unwrap().weekday(), 4);
        assert_eq!(DateTime::parse("2026-11-15 00:00").unwrap().weekday(), 0);
    }

    #[test]
    fn rollover() {
        let t = DateTime::parse("2023-12-31 23:59").unwrap();
        assert_eq!(t.succ().to_string(), "2024-01-01 00:00");
        let t = DateTime::parse("2024-02-28 23:59").unwrap();
        assert_eq!(t.succ().to_string(), "2024-02-29 00:00");
        assert_eq!(t.next_day().to_string(), "2024-02-29 00:00");
    }

    #[test]
    fn rejects_invalid() {
        assert!(DateTime::parse("2023-02-29 00:00").is_none());
        assert!(DateTime::parse("2023-01-01 24:00").is_none());
    }
}
