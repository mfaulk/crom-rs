# Guard downstream credentials

Executors call downstream APIs with credentials that stay in the Guard process (MILESTONE_18 §11, #355). The agent never receives one. Code: `crates/crom-guard/src/credential.rs` and `credential/broker.rs`.

## Leases

`CredentialBroker::lease(action)` returns a `CredentialLease` for one executor call.

- The secret is reachable only inside `with_secret(|s| ..)`, which is meant for setting a header on the outgoing request.
- A lease has no `Debug`, `Display`, `Serialize` or `Clone`; `compile_fail` doctests check this.
- The secret is held in `Zeroizing<String>`, so it is wiped when the lease is dropped. Copies the executor makes inside `with_secret`, such as an HTTP header value, are not wiped.
- `credential_id()` is the non-secret id recorded in audit and in the business ledger.
- An action with no configured credential fails with `BrokerError::NotConfigured`.

## `FileCredentialBroker`

```json
{"credentials": [
  {"id": "business-api", "secret": "…", "actions": ["customer.read", "refund.issue"]}
]}
```

`FileCredentialBroker::open(path)` refuses the file unless all of these hold. The checks run on the opened file descriptor, so the path can't be swapped between check and read:

- it is a regular file, opened with `O_NOFOLLOW` (symlinks are refused);
- its mode gives nothing to group or others (0600 or 0400);
- it is owned by Guard's effective uid;
- it is at most 64 KiB (`MAX_CREDENTIALS_FILE_BYTES`);
- it parses with no unknown fields, ids match `[A-Za-z0-9._-]+` and are unique, secrets are non-empty, every credential names at least one action, and each action maps to at most one credential.

Errors never contain secret material. A parse error reports only its line and column, because `serde_json` messages can quote the offending value, and an invalid id is reported by its position in the list.

Non-Unix platforms return `CredentialLoadError::Unsupported`; use `EnvCredentialBroker` there.

## `EnvCredentialBroker`

For containers. `EnvCredentialBroker::from_env(vec![EnvCredential { id, var, actions }])` reads each secret from Guard's own environment. A missing variable is reported by name only. The variables stay in Guard's environment, so Guard must never spawn agent processes that inherit it.

## Rotation

`reload()` on either broker re-reads its source and swaps the whole action table at once. The admin API (#357) will call it.

- New leases get the new value; leases already handed out keep the old one until dropped.
- A reload that fails any check keeps the previous table in use.

## Not supported

- A `crom-auth`-backed broker: not until #319 (file store concurrency flake) is resolved.
- OS keychain (#205), cloud secret managers, and per-resource scoped downstream tokens.

## Tests

`cargo test -p crom-guard --test credentials` covers:
- leasing by action and `NotConfigured`;
- refusing group- or world-accessible modes, symlinks and directories;
- invalid configurations and oversize files;
- a canary scan: the secret never appears in any load error, lease error, or broker `Debug` output, including parse errors where `serde_json` would quote it;
- rotation, including a failed reload keeping the last good table;
- the environment broker's leasing, rotation and missing-variable error.

The wrong-owner check is not tested, because creating a file owned by another uid needs root. The pipeline (#351) will extend the canary scan to `CallOutcome`, audit records and `tracing` output.
