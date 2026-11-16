//! Plain-text trial balance.

use ledger::{Ledger, LedgerError};
use money::Money;

/// Render a two-column trial balance followed by a total line.
pub fn trial_balance(ledger: &Ledger) -> Result<String, LedgerError> {
    let rows = ledger.balances()?;
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0).max(5);
    let mut out = String::new();
    let mut total = Money::zero(ledger.currency());
    for (name, bal) in &rows {
        out.push_str(&format!("{name:<width$}  {:>16}\n", bal.to_string()));
        total = total.checked_add(bal)?;
    }
    out.push_str(&format!("{:-<w$}\n", "", w = width + 18));
    out.push_str(&format!("{:<width$}  {:>16}\n", "TOTAL", total.to_string()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger::AccountKind;
    use money::Currency;

    #[test]
    fn renders_balanced_total() {
        let mut l = Ledger::new(Currency::Usd);
        l.open("cash", AccountKind::Asset).unwrap();
        l.open("food", AccountKind::Expense).unwrap();
        l.split_expense(
            "lunch",
            "cash",
            Money::parse("12.00", Currency::Usd).unwrap(),
            &[("food", 1)],
        )
        .unwrap();
        let out = trial_balance(&l).unwrap();
        assert!(out.contains("TOTAL"));
        assert!(out.lines().last().unwrap().ends_with("0.00 USD"));
    }
}
