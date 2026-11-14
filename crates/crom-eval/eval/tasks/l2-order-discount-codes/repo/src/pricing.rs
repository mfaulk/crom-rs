use crate::money::apply_basis_points;
use crate::order::Order;

/// Sales tax: 8.00%, expressed in basis points.
pub const TAX_RATE_BASIS_POINTS: i64 = 800;

/// Sum of every line on the order.
pub fn subtotal_cents(order: &Order) -> i64 {
    order.items().iter().map(|i| i.line_cents()).sum()
}

/// Tax owed on the order, rounded to the nearest cent (half up).
pub fn tax_cents(order: &Order) -> i64 {
    apply_basis_points(subtotal_cents(order), TAX_RATE_BASIS_POINTS)
}

/// Amount the customer pays.
pub fn total_cents(order: &Order) -> i64 {
    subtotal_cents(order) + tax_cents(order)
}
