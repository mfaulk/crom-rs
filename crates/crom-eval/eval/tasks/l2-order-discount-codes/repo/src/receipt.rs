use crate::money::format_cents;
use crate::order::Order;
use crate::pricing::{subtotal_cents, tax_cents, total_cents};

/// Render a plain-text receipt. One line per item, then the totals block:
///
/// ```text
/// 2 x Widget @ $3.00 = $6.00
/// Subtotal: $6.00
/// Tax: $0.48
/// Total: $6.48
/// ```
pub fn render_receipt(order: &Order) -> String {
    let mut out = String::new();
    for item in order.items() {
        out.push_str(&format!(
            "{} x {} @ {} = {}\n",
            item.qty,
            item.name,
            format_cents(item.unit_cents),
            format_cents(item.line_cents())
        ));
    }
    out.push_str(&format!(
        "Subtotal: {}\n",
        format_cents(subtotal_cents(order))
    ));
    out.push_str(&format!("Tax: {}\n", format_cents(tax_cents(order))));
    out.push_str(&format!("Total: {}\n", format_cents(total_cents(order))));
    out
}
