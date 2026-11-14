use orders::{discount_cents, render_receipt, subtotal_cents, tax_cents, total_cents};
use orders::{Order, PricingError};

fn sample() -> Order {
    let mut o = Order::new();
    o.add_item("W-1", "Widget", 300, 2);
    o.add_item("G-7", "Gadget", 1250, 1);
    o
}

#[test]
fn no_code_means_no_discount() {
    let o = sample();
    assert_eq!(o.discount_code(), None);
    assert_eq!(discount_cents(&o), 0);
    assert_eq!(total_cents(&o), 1998);
}

#[test]
fn percent_code_discounts_and_taxes_discounted_subtotal() {
    let mut o = sample();
    o.apply_discount("save10").unwrap();
    assert_eq!(o.discount_code(), Some("SAVE10"));
    assert_eq!(subtotal_cents(&o), 1850);
    assert_eq!(discount_cents(&o), 185);
    // 8% of 1665 = 133.2 -> 133
    assert_eq!(tax_cents(&o), 133);
    assert_eq!(total_cents(&o), 1665 + 133);
}

#[test]
fn flat_code_is_capped_at_subtotal() {
    let mut o = Order::new();
    o.add_item("P-1", "Pencil", 120, 3);
    o.apply_discount("FLAT5").unwrap();
    assert_eq!(discount_cents(&o), 360);
    assert_eq!(tax_cents(&o), 0);
    assert_eq!(total_cents(&o), 0);
}

#[test]
fn unknown_code_is_rejected_and_keeps_previous_code() {
    let mut o = sample();
    o.apply_discount("FLAT5").unwrap();
    let err = o.apply_discount("BOGUS").unwrap_err();
    assert_eq!(err, PricingError::UnknownCode("BOGUS".to_string()));
    assert_eq!(o.discount_code(), Some("FLAT5"));
    assert_eq!(discount_cents(&o), 500);
}

#[test]
fn second_code_replaces_first() {
    let mut o = sample();
    o.apply_discount("FLAT5").unwrap();
    o.apply_discount("Save10").unwrap();
    assert_eq!(o.discount_code(), Some("SAVE10"));
    assert_eq!(discount_cents(&o), 185);
}

#[test]
fn receipt_shows_discount_line_only_when_applied() {
    let mut o = sample();
    assert!(!render_receipt(&o).contains("Discount"));
    o.apply_discount("SAVE10").unwrap();
    let expected = "\
2 x Widget @ $3.00 = $6.00
1 x Gadget @ $12.50 = $12.50
Subtotal: $18.50
Discount (SAVE10): -$1.85
Tax: $1.33
Total: $17.98
";
    assert_eq!(render_receipt(&o), expected);
}
