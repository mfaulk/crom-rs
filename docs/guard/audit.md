# Guard audit log (`guard-audit/v1`)

#352. Spec: [MILESTONE_18](../milestones/MILESTONE_18.md) §12. Code: `crates/crom-guard/src/audit/` (`chain.rs` encodes and verifies, `sink.rs` writes). Schema: [`crates/crom-guard/schemas/guard-audit-v1.schema.json`](../../crates/crom-guard/schemas/guard-audit-v1.schema.json) (JSON Schema 2020-12).

The log gives **tamper evidence, not tamper proofing**. Any edit, deletion, insertion or reordering of records is detected. A process running as Guard's UID can still rewrite the whole chain consistently. Only a head hash a client kept (an *anchor*) detects that, or detects whole records cut off the end.

## Line format

One record per line, UTF-8, terminated by `\n`. Each line is one JSON object, written as compact `serde_json` output. Members appear in this order:

```
{"format":"guard-audit/v1","seq":<u64>,"prev_hash":"<hex64>",<AuditRecord members>,"hash":"<hex64>"}
```

- `format` is always `guard-audit/v1`.
- `seq` is 1 for the first record and goes up by exactly 1 per line.
- `prev_hash` is the previous line's `hash`. For `seq` 1 it is `GENESIS_HASH`, 64 `0`s.
- `<AuditRecord members>` are the fields of `crom_guard::AuditRecord` (§12.1), in declaration order. Every field is present; absent values are `null`. Identity fields are nested under `identity`, not flat. `reason` is `{"reason":"<code>"}`, or `{"reason":"chain_deny","grant_id":..}` for a chain deny.
- `hash` is always the last member.

All hex is lowercase.

## Hash rule

`hash = lowercase_hex(SHA-256(input))`, where `input` is the bytes of the stored line, without its trailing `\n`, minus the final member `,"hash":"<64 hex>"`. The closing `}` is kept.

So the hash covers `format`, `seq`, `prev_hash` and every record field. To check a line, an independent verifier:

1. drops the final 75 bytes (`,"hash":"` + 64 hex + `"}`),
2. appends `}`,
3. hashes the result with SHA-256.

It never re-serializes JSON, so no JSON canonicalization scheme (key order, number formatting, escapes) is involved. The writer's output is canonical by construction. The verifier treats the stored bytes as authoritative.

## Verification

`crom_guard::verify_log` / `verify_file` / `verify_log_with` (the last one gives each `AuditEntry` to a visitor) check every line in order. They stop at the first defect, with its 1-based line number:

| Check | Error (`VerifyErrorKind`) | Catches |
|---|---|---|
| Line ends in `\n` | `PartialTrailingRecord` | Torn write, byte-level truncation |
| Ends with a well-formed hash member | `Malformed` | Garbage, a stripped hash |
| Recomputed hash matches the stored one | `HashMismatch` | In-place edit |
| `format` matches | `WrongFormat` | |
| `seq` matches the expected value | `SeqMismatch` | Deletion (including of the first record), insertion, reordering |
| `prev_hash` is the previous `hash` | `PrevHashMismatch` | An edit where the attacker also recomputed the line's own hash, or a forged insertion |
| Deserializes into `AuditRecord`, with no unknown members | `Schema` | A renamed, extra or ill-typed field |
| Each anchor's `seq` has the anchor's hash | `AnchorMismatch` | A rewritten chain |
| No anchor's `seq` is past the end | `AnchorMissing` | Whole records cut off the end |

An empty log verifies, with zero records and no head.

CLI (exit 0 = intact, 1 = broken, which is the §13.2 `AuditChainBroken` probe, 2 = usage):

```sh
crom-guard audit verify /path/to/audit.jsonl [--anchor <seq>:<hash>]...
```

Anchors are `AuditReceipt { seq, head_hash }` values that `append` returned. #357 returns them in every response.

## Writer (`FileAuditSink`)

`FileAuditSink::open(path)` handles new and existing logs:

- **New log.** Creates the file with mode `0600`, then fsyncs the file and its directory.
- **Existing log.** Refuses a file that group or others can access. Also refuses one that fails `verify_log`, including one that ends in a partial record. Otherwise it resumes from the verified head.
- **Locking.** Takes an exclusive `flock` that lasts as long as the sink, so a second writer cannot fork the chain.

Each record is written as one `write_all` of the complete line to the `O_APPEND` file, followed by `sync_data`. `AuditSink::append` resolves only after that.

Appends go through a mutex, so `seq` is strictly increasing with no gaps. The write and fsync are blocking calls, run on the thread that polls the future. The crate has no async runtime, so an async server should poll `append` from `spawn_blocking`.

**Failure semantics (§12.3).**

- **Storage error.** Disk full, an I/O error, or a failed fsync returns `AuditError::Io`. The pipeline (#351) then denies with `AuditUnavailable`. The sink is now *failed closed*: every later append returns `AuditError::Unavailable` until the process restarts.
- **Restart.** `open` re-verifies the log. A torn trailing line left by the failed write makes `open` fail. Guard then stays down until an operator inspects the log.
- **Recovery.** Use `audit verify`, which names the line, then truncate the file to the last complete line. This is deliberate: silently dropping the fragment would also hide a byte-level truncation. That record was never acknowledged, so no executor ran on it.

**Logging rules.** Records hold only what `AuditRecord` allows: an args digest, the per-operation allowlisted args, and agent-supplied values kept apart under `untrusted`. As a last line of defense, the sink refuses any record whose encoded line contains a plaintext session handle (`gsh_` followed by 64 hex digits). Such a record returns `AuditError::Unavailable`, nothing is written, and the sink keeps working. Keeping credentials out of `args_logged` is the pipeline's job (#351); its credential-canary test is `crates/crom-guard-testkit/tests/canary.rs` ([pipeline.md](pipeline.md)).

## Ledger correlation

`verify_log_with` yields every `AuditEntry`. A reconciler (#361) matches each ledger effect's `X-Guard-Decision-Id` against an `outcome` record with the same `decision_id` and `outcome = executed`, and against its `downstream_ref` (§12.3 audit coverage, §13.2 `UnmediatedEffect`).

## Not yet

- Log rotation (size-based roll with a chained head) and external anchoring. Both are out of scope for #352.
- Wiring `audit verify` into the eval harness's `AuditChainBroken` probe (#357, #361).
