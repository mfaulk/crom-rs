# cronlite

Cron-style job scheduling without a system clock.

- `calendar` — a proleptic-Gregorian `DateTime` at minute resolution (UTC).
- `cronexpr` — parses five-field cron expressions
  (`minute hour day-of-month month day-of-week`) into explicit value sets.
- `scheduler` — computes next run times and drives jobs from a caller-supplied
  clock.

Supported field syntax today: `*`, a single number, or a comma-separated list
of numbers. Day-of-week uses 0 = Sunday … 6 = Saturday.
