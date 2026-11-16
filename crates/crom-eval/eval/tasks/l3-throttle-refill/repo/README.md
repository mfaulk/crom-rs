# throttle

Per-client request rate limiting.

- `clock` — a millisecond `Clock` trait with a manual clock for tests.
- `bucket` — a token bucket: `capacity` tokens, one token added every
  `interval_ms`.
- `gateway` — keeps one bucket per client, answers allow/limit decisions with
  `Retry-After` hints, and evicts idle clients.
