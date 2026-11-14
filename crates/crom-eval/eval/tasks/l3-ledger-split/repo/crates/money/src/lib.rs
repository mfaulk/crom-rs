//! Fixed-point money. Amounts are stored as an integer count of minor units
//! (cents for USD/EUR, yen for JPY) so arithmetic is exact.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Currency {
    Usd,
    Eur,
    Jpy,
}

impl Currency {
    /// Number of digits after the decimal point.
    pub fn minor_digits(self) -> u32 {
        match self {
            Currency::Usd | Currency::Eur => 2,
            Currency::Jpy => 0,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Currency::Usd => "USD",
            Currency::Eur => "EUR",
            Currency::Jpy => "JPY",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoneyError {
    CurrencyMismatch(Currency, Currency),
    Overflow,
    Parse(String),
    EmptyRatios,
    ZeroRatios,
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoneyError::CurrencyMismatch(a, b) => {
                write!(f, "currency mismatch: {} vs {}", a.code(), b.code())
            }
            MoneyError::Overflow => write!(f, "arithmetic overflow"),
            MoneyError::Parse(s) => write!(f, "cannot parse amount: {s}"),
            MoneyError::EmptyRatios => write!(f, "allocation needs at least one ratio"),
            MoneyError::ZeroRatios => write!(f, "allocation ratios sum to zero"),
        }
    }
}

impl std::error::Error for MoneyError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Money {
    minor: i64,
    currency: Currency,
}

impl Money {
    pub fn from_minor(minor: i64, currency: Currency) -> Self {
        Money { minor, currency }
    }

    pub fn zero(currency: Currency) -> Self {
        Money::from_minor(0, currency)
    }

    /// Parse a decimal string such as `"12.34"` or `"-0.5"`.
    pub fn parse(s: &str, currency: Currency) -> Result<Self, MoneyError> {
        let s = s.trim();
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let digits = currency.minor_digits() as usize;
        let (whole, frac) = match body.split_once('.') {
            Some((w, f)) => (w, f),
            None => (body, ""),
        };
        if whole.is_empty() || frac.len() > digits {
            return Err(MoneyError::Parse(s.to_string()));
        }
        if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
            return Err(MoneyError::Parse(s.to_string()));
        }
        let mut frac_padded = frac.to_string();
        while frac_padded.len() < digits {
            frac_padded.push('0');
        }
        let scale = 10i64.pow(digits as u32);
        let whole: i64 = whole
            .parse()
            .map_err(|_| MoneyError::Parse(s.to_string()))?;
        let frac: i64 = if frac_padded.is_empty() {
            0
        } else {
            frac_padded
                .parse()
                .map_err(|_| MoneyError::Parse(s.to_string()))?
        };
        let minor = whole
            .checked_mul(scale)
            .and_then(|w| w.checked_add(frac))
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::from_minor(
            if neg { -minor } else { minor },
            currency,
        ))
    }

    pub fn minor(&self) -> i64 {
        self.minor
    }

    pub fn currency(&self) -> Currency {
        self.currency
    }

    pub fn is_zero(&self) -> bool {
        self.minor == 0
    }

    pub fn negate(&self) -> Money {
        Money::from_minor(-self.minor, self.currency)
    }

    fn same_currency(&self, other: &Money) -> Result<(), MoneyError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch(self.currency, other.currency))
        }
    }

    pub fn checked_add(&self, other: &Money) -> Result<Money, MoneyError> {
        self.same_currency(other)?;
        let minor = self
            .minor
            .checked_add(other.minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::from_minor(minor, self.currency))
    }

    pub fn checked_sub(&self, other: &Money) -> Result<Money, MoneyError> {
        self.checked_add(&other.negate())
    }

    /// Split this amount into parts proportional to `ratios`.
    ///
    /// The returned vector has one entry per ratio, in the same order.
    pub fn allocate(&self, ratios: &[u32]) -> Result<Vec<Money>, MoneyError> {
        if ratios.is_empty() {
            return Err(MoneyError::EmptyRatios);
        }
        let total: i64 = ratios.iter().map(|&r| r as i64).sum();
        if total == 0 {
            return Err(MoneyError::ZeroRatios);
        }
        let parts = ratios
            .iter()
            .map(|&r| Money::from_minor(self.minor * r as i64 / total, self.currency))
            .collect();
        Ok(parts)
    }

    /// Split evenly into `n` parts.
    pub fn split(&self, n: usize) -> Result<Vec<Money>, MoneyError> {
        self.allocate(&vec![1; n])
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.currency.minor_digits();
        let sign = if self.minor < 0 { "-" } else { "" };
        let abs = self.minor.unsigned_abs();
        if digits == 0 {
            return write!(f, "{sign}{abs} {}", self.currency.code());
        }
        let scale = 10u64.pow(digits);
        write!(
            f,
            "{sign}{}.{:0width$} {}",
            abs / scale,
            abs % scale,
            self.currency.code(),
            width = digits as usize
        )
    }
}

impl FromStr for Currency {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "USD" => Ok(Currency::Usd),
            "EUR" => Ok(Currency::Eur),
            "JPY" => Ok(Currency::Jpy),
            other => Err(MoneyError::Parse(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays() {
        let m = Money::parse("12.5", Currency::Usd).unwrap();
        assert_eq!(m.minor(), 1250);
        assert_eq!(m.to_string(), "12.50 USD");
        assert_eq!(
            Money::parse("-0.05", Currency::Eur).unwrap().to_string(),
            "-0.05 EUR"
        );
        assert_eq!(
            Money::parse("500", Currency::Jpy).unwrap().to_string(),
            "500 JPY"
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Money::parse("1.234", Currency::Usd).is_err());
        assert!(Money::parse("abc", Currency::Usd).is_err());
        assert!(Money::parse(".5", Currency::Usd).is_err());
    }

    #[test]
    fn add_requires_same_currency() {
        let a = Money::from_minor(100, Currency::Usd);
        let b = Money::from_minor(100, Currency::Eur);
        assert!(matches!(
            a.checked_add(&b),
            Err(MoneyError::CurrencyMismatch(..))
        ));
    }

    #[test]
    fn even_split() {
        let parts = Money::from_minor(1000, Currency::Usd).split(4).unwrap();
        assert_eq!(
            parts.iter().map(Money::minor).collect::<Vec<_>>(),
            vec![250; 4]
        );
    }

    #[test]
    fn weighted_allocation_exact() {
        let parts = Money::from_minor(1000, Currency::Usd)
            .allocate(&[3, 1])
            .unwrap();
        assert_eq!(parts[0].minor(), 750);
        assert_eq!(parts[1].minor(), 250);
    }
}
