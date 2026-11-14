//! Order pricing and receipt rendering for a small shop.

pub mod money;
pub mod order;
pub mod pricing;
pub mod receipt;

pub use money::format_cents;
pub use order::{LineItem, Order};
pub use pricing::{subtotal_cents, tax_cents, total_cents, TAX_RATE_BASIS_POINTS};
pub use receipt::render_receipt;
