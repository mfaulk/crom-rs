//! Five-field cron expressions.

use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Minute,
    Hour,
    DayOfMonth,
    Month,
    DayOfWeek,
}

impl Field {
    pub const ALL: [Field; 5] = [
        Field::Minute,
        Field::Hour,
        Field::DayOfMonth,
        Field::Month,
        Field::DayOfWeek,
    ];

    /// Inclusive bounds.
    pub fn bounds(self) -> (u8, u8) {
        match self {
            Field::Minute => (0, 59),
            Field::Hour => (0, 23),
            Field::DayOfMonth => (1, 31),
            Field::Month => (1, 12),
            Field::DayOfWeek => (0, 6),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Field::Minute => "minute",
            Field::Hour => "hour",
            Field::DayOfMonth => "day-of-month",
            Field::Month => "month",
            Field::DayOfWeek => "day-of-week",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronError {
    WrongFieldCount(usize),
    Invalid { field: Field, token: String },
    OutOfRange { field: Field, value: u8 },
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CronError::WrongFieldCount(n) => write!(f, "expected 5 fields, found {n}"),
            CronError::Invalid { field, token } => {
                write!(f, "invalid {} token `{token}`", field.name())
            }
            CronError::OutOfRange { field, value } => {
                let (lo, hi) = field.bounds();
                write!(f, "{} value {value} outside {lo}-{hi}", field.name())
            }
        }
    }
}

impl std::error::Error for CronError {}

/// A parsed expression: the explicit set of allowed values per field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    pub minutes: BTreeSet<u8>,
    pub hours: BTreeSet<u8>,
    pub days_of_month: BTreeSet<u8>,
    pub months: BTreeSet<u8>,
    pub days_of_week: BTreeSet<u8>,
    /// Whether day-of-month / day-of-week were `*`. Cron semantics: when both
    /// are restricted, a day matches if EITHER matches.
    pub dom_star: bool,
    pub dow_star: bool,
}

impl CronExpr {
    pub fn parse(expr: &str) -> Result<CronExpr, CronError> {
        let tokens: Vec<&str> = expr.split_whitespace().collect();
        if tokens.len() != 5 {
            return Err(CronError::WrongFieldCount(tokens.len()));
        }
        let mut sets = Vec::with_capacity(5);
        for (tok, field) in tokens.iter().zip(Field::ALL) {
            sets.push(parse_field(tok, field)?);
        }
        let mut it = sets.into_iter();
        Ok(CronExpr {
            minutes: it.next().unwrap(),
            hours: it.next().unwrap(),
            days_of_month: it.next().unwrap(),
            months: it.next().unwrap(),
            days_of_week: it.next().unwrap(),
            dom_star: tokens[2] == "*",
            dow_star: tokens[4] == "*",
        })
    }

    /// Whether the given calendar fields match.
    pub fn matches(&self, minute: u8, hour: u8, day: u8, month: u8, weekday: u8) -> bool {
        if !self.minutes.contains(&minute)
            || !self.hours.contains(&hour)
            || !self.months.contains(&month)
        {
            return false;
        }
        let dom = self.days_of_month.contains(&day);
        let dow = self.days_of_week.contains(&weekday);
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }
}

fn parse_field(token: &str, field: Field) -> Result<BTreeSet<u8>, CronError> {
    let (lo, hi) = field.bounds();
    let mut set = BTreeSet::new();
    for part in token.split(',') {
        if part == "*" {
            set.extend(lo..=hi);
            continue;
        }
        let value = parse_value(part, field)?;
        set.insert(value);
    }
    Ok(set)
}

fn parse_value(s: &str, field: Field) -> Result<u8, CronError> {
    let value: u8 = s.parse().map_err(|_| CronError::Invalid {
        field,
        token: s.to_string(),
    })?;
    let (lo, hi) = field.bounds();
    if value < lo || value > hi {
        return Err(CronError::OutOfRange { field, value });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_and_lists() {
        let e = CronExpr::parse("0,30 9 * * *").unwrap();
        assert_eq!(e.minutes.iter().copied().collect::<Vec<_>>(), [0, 30]);
        assert_eq!(e.hours.len(), 1);
        assert_eq!(e.days_of_month.len(), 31);
        assert!(e.dom_star && e.dow_star);
    }

    #[test]
    fn errors() {
        assert_eq!(CronExpr::parse("* * *"), Err(CronError::WrongFieldCount(3)));
        assert_eq!(
            CronExpr::parse("60 * * * *"),
            Err(CronError::OutOfRange {
                field: Field::Minute,
                value: 60
            })
        );
        assert!(matches!(
            CronExpr::parse("x * * * *"),
            Err(CronError::Invalid {
                field: Field::Minute,
                ..
            })
        ));
    }

    #[test]
    fn dom_dow_union() {
        let e = CronExpr::parse("0 0 1 * 1").unwrap();
        assert!(e.matches(0, 0, 1, 5, 3));
        assert!(e.matches(0, 0, 9, 5, 1));
        assert!(!e.matches(0, 0, 9, 5, 2));
    }
}
