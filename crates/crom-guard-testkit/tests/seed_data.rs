use std::collections::HashSet;

use crom_guard_testkit::data::{INJECTED_INSTRUCTION, TENANTS};
use crom_guard_testkit::{canary, seed};

/// Seeding twice yields identical data.
#[test]
fn seed_is_deterministic() {
    assert_eq!(seed(), seed());
}

/// Each tenant has 3 users, 10 customers, and invoices that reference only same-tenant customers.
#[test]
fn seed_shape_and_tenant_consistency() {
    let d = seed();
    assert_eq!(d.users.len(), 6);
    for t in TENANTS {
        assert_eq!(d.customers.iter().filter(|c| c.tenant == t).count(), 10);
        assert_eq!(d.users.iter().filter(|u| u.tenant == t).count(), 3);
    }
    let customers: HashSet<(&str, &str)> = d
        .customers
        .iter()
        .map(|c| (c.tenant.as_str(), c.id.as_str()))
        .collect();
    for i in &d.invoices {
        assert!(
            customers.contains(&(i.tenant.as_str(), i.customer_id.as_str())),
            "{}",
            i.id
        );
    }
    let ids: HashSet<&str> = d
        .invoices
        .iter()
        .map(|i| i.id.as_str())
        .chain(d.customers.iter().map(|c| c.id.as_str()))
        .collect();
    assert_eq!(
        ids.len(),
        d.invoices.len() + d.customers.len(),
        "ids are globally unique"
    );
}

/// Every record's free text carries its own canary, and some invoice memos carry the injected instruction in both tenants.
#[test]
fn canaries_and_injections_present() {
    let d = seed();
    for c in &d.customers {
        assert!(c.notes.contains(&canary(&c.tenant, &c.id)));
    }
    for i in &d.invoices {
        assert!(i.memo.contains(&canary(&i.tenant, &i.id)));
    }
    for t in TENANTS {
        assert!(d
            .invoices
            .iter()
            .any(|i| i.tenant == t && i.memo.contains(INJECTED_INSTRUCTION)));
        assert!(d
            .invoices
            .iter()
            .any(|i| i.tenant == t && !i.memo.contains(INJECTED_INSTRUCTION)));
    }
}
