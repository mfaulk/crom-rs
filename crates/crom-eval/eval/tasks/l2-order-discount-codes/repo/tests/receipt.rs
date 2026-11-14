use orders::{render_receipt, total_cents, Order};

fn sample() -> Order {
    let mut o = Order::new();
    o.add_item("W-1", "Widget", 300, 2);
    o.add_item("G-7", "Gadget", 1250, 1);
    o
}

#[test]
fn totals_include_tax() {
    // subtotal 1850, tax 148
    assert_eq!(total_cents(&sample()), 1998);
}

#[test]
fn repeated_sku_merges_quantity() {
    let mut o = sample();
    o.add_item("W-1", "Widget", 300, 3);
    assert_eq!(o.items().len(), 2);
    assert_eq!(o.items()[0].qty, 5);
}

#[test]
fn receipt_layout() {
    let expected = "\
2 x Widget @ $3.00 = $6.00
1 x Gadget @ $12.50 = $12.50
Subtotal: $18.50
Tax: $1.48
Total: $19.98
";
    assert_eq!(render_receipt(&sample()), expected);
}
