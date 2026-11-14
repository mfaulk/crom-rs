/// One line on an order: `qty` units of a product at `unit_cents` each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineItem {
    pub sku: String,
    pub name: String,
    pub unit_cents: i64,
    pub qty: u32,
}

impl LineItem {
    pub fn line_cents(&self) -> i64 {
        self.unit_cents * i64::from(self.qty)
    }
}

/// A customer order. Adding an existing SKU increases its quantity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Order {
    items: Vec<LineItem>,
}

impl Order {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_item(&mut self, sku: &str, name: &str, unit_cents: i64, qty: u32) {
        if let Some(existing) = self.items.iter_mut().find(|i| i.sku == sku) {
            existing.qty += qty;
            return;
        }
        self.items.push(LineItem {
            sku: sku.to_string(),
            name: name.to_string(),
            unit_cents,
            qty,
        });
    }

    pub fn items(&self) -> &[LineItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
