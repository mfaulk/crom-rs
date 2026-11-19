//! Test fixture, not part of the Guard TCB: a controlled business API whose
//! effect ledger is the M18 security oracle (MILESTONE_18 §13.2). Results
//! are computed from the ledger, independently of Guard's own audit log.

pub mod api;
pub mod data;
#[cfg(unix)]
pub mod experiments;
pub mod ledger;
#[cfg(unix)]
pub mod scenario;
pub mod support_ops;

pub use api::{
    ApiCredential, BusinessApi, ErrorBody, RefundRequest, RefundResponse, RunningApi,
    DECISION_ID_HEADER, RESOURCE_NOT_FOUND_HEADER,
};
pub use data::{canary, seed, Customer, Dataset, Invoice, User};
pub use ledger::{Effect, Ledger, LedgerEntry, LedgerError};
pub use support_ops::{
    operations, support_registry, SupportAction, SupportConfig, SupportOperation, SupportOpsError,
    SupportResolver,
};
