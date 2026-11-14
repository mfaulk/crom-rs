use crate::rounding::mills_to_cents;

/// `qty` units at `unit_mills` each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub description: String,
    pub unit_mills: i64,
    pub qty: i64,
}

impl Line {
    /// Line total in cents, rounded once (half to even) from exact mills.
    pub fn total_cents(&self) -> i64 {
        mills_to_cents(self.unit_mills * self.qty)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Invoice {
    pub lines: Vec<Line>,
}

impl Invoice {
    pub fn add(&mut self, description: &str, unit_mills: i64, qty: i64) -> &mut Self {
        self.lines.push(Line {
            description: description.to_string(),
            unit_mills,
            qty,
        });
        self
    }

    /// Sum of the already-rounded line totals.
    pub fn total_cents(&self) -> i64 {
        self.lines.iter().map(Line::total_cents).sum()
    }
}
