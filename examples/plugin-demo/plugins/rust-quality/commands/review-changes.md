---
description: Review changed Rust files since base
argument-hint: <base-ref>
allowed-tools:
  - Read
  - Grep
  - Glob
---
Review the Rust changes since `$1`. Look for API breaks, missing tests,
and unclear naming. Report findings with file paths and line numbers.
