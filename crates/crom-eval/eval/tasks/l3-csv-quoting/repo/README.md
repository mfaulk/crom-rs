# tabular

Load CSV into typed tables and summarize columns.

- `csvread` — CSV tokenizer: `parse(&str) -> Vec<Vec<String>>`.
- `schema` — infers column types (integer, float, bool, text) from a header
  row plus data and produces typed rows.
- `colstats` — per-column summaries (count, nulls, min/max/mean, distinct).

Empty fields are treated as nulls.
