# Patch-only mode

`PatchPolicy::ReturnOnly` (the default) makes the SDK produce a `PatchSet`
without mutating the underlying workspace. Hosts receive a diff plus a
typed list of `FilePatch` entries — create/modify/delete/rename — and decide
what to do.

## How it works

| Workspace type | Behavior |
| -------------- | -------- |
| Any workspace | SDK snapshots the workspace's `list_files()` set **before** the run and writes it into a private temp dir. Built-in tools (including `Bash`) run with that temp dir as `cwd`. The `PatchSet` is the diff of the temp dir against the snapshot; the caller's workspace is never handed to the tools. |

Files outside the snapshot (for a `FilesystemWorkspace`: gitignored and
hidden paths such as `.git/` or `target/`) are not present in the scratch
copy. This is policy-level isolation, not an OS sandbox: a `Bash` command
that writes to an absolute path outside the scratch dir is not contained
(see [`docs/security/threat-model.md`](../security/threat-model.md)).

## `PatchPolicy` semantics

| Policy                 | Behavior                                                                 |
| ---------------------- | ------------------------------------------------------------------------ |
| `ReturnOnly`           | Compute patch against a scratch copy; never mutate the workspace.        |
| `ApplyToWorkspace`     | Apply the patch directly to the workspace. Do not return it.             |
| `ApplyAndReturn`       | Apply AND return the patch.                                              |
| `RejectMutations`      | Refuse to compute mutations entirely; raises an invalid-request error    |
|                        | when paired with a mutating `AgentMode`.                                 |

## Determinism

`PatchSet::unified_diff` is deterministic for the same input. Diff
generation uses the `similar` crate with 3-line context. Binary file changes
record only the size/hash; the unified-diff body is empty so binary noise
does not appear in patches.
