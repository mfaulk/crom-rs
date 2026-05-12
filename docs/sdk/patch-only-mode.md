# Patch-only mode

`PatchPolicy::ReturnOnly` (the default) makes the SDK produce a `PatchSet`
without mutating the underlying workspace. Hosts receive a diff plus a
typed list of `FilePatch` entries — create/modify/delete/rename — and decide
what to do.

## How it works

| Workspace type | Behavior |
| -------------- | -------- |
| `InMemoryWorkspace` / overlay / custom | SDK wraps in `PatchOnlyWorkspace`. Writes route to a pending map. `take_patch()` consumes the patch. |
| `FilesystemWorkspace`                  | SDK snapshots the workspace **before** the run, lets the agent mutate disk through the built-in tools, then computes the diff and **rolls back** every file to the snapshot. The host gets a `PatchSet`; disk is unchanged. |

The rollback is best-effort: it only restores files that were present in
the snapshot and removes files that were created during the run. If your
host needs guaranteed isolation, run on a copy or a container-mounted
overlay directory.

## `PatchPolicy` semantics

| Policy                 | Behavior                                                                 |
| ---------------------- | ------------------------------------------------------------------------ |
| `ReturnOnly`           | Compute patch, do not mutate (rollback for filesystem).                  |
| `ApplyToWorkspace`     | Apply the patch directly to the workspace. Do not return it.             |
| `ApplyAndReturn`       | Apply AND return the patch.                                              |
| `RejectMutations`      | Refuse to compute mutations entirely; raises an invalid-request error    |
|                        | when paired with a mutating `AgentMode`.                                 |

## Determinism

`PatchSet::unified_diff` is deterministic for the same input. Diff
generation uses the `similar` crate with 3-line context. Binary file changes
record only the size/hash; the unified-diff body is empty so binary noise
does not appear in patches.
