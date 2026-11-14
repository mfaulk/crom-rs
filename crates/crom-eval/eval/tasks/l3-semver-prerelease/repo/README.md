# pkgsolve

A toy package resolver.

- `versions` — semantic version parsing, ordering, and requirement matching.
- `registry` — an in-memory package index.
- `resolver` — picks the newest compatible version for every dependency,
  transitively.

`cargo test --workspace` runs everything.
