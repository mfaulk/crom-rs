//! Crom Guard: the enforcement point for agent-initiated operations.
//!
//! Trusted computing base. This crate must not depend on any other `crom-*`
//! crate so it can be used without the Crom runtime and extracted to its own
//! repository (ADR 0002, `docs/security/decisions/0002-guard-architecture.md`).
