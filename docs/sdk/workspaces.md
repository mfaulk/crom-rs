# Workspaces

`Workspace` is the abstraction the SDK uses for "where the model reads and
writes." Every path the model touches is a `WorkspacePath`: relative,
forward-slash, no `..` traversal.

| Source                                         | Use case                                            |
| ---------------------------------------------- | --------------------------------------------------- |
| `WorkspaceSource::filesystem(path)`            | A real on-disk directory.                           |
| `WorkspaceSource::in_memory(InMemoryWorkspace)`| Files held in memory; useful in web services.       |
| `WorkspaceSource::read_only(inner)`            | Wraps any workspace and denies writes.              |
| `WorkspaceSource::patch_only(inner)`           | Records writes as patches; base is never mutated.   |
| `WorkspaceSource::overlay(base)`               | Reads from base, writes to an in-memory overlay.    |
| `WorkspaceSource::custom(inner)`               | Plug in your own `Workspace` implementation.        |

## Path safety

- Absolute paths are rejected at `WorkspacePath::new`.
- `..` traversal is rejected.
- Symlinks that escape the root are rejected by the filesystem
  implementation.
- NUL bytes are rejected.

## Capabilities

Each workspace exposes a `WorkspaceCapabilities` snapshot:

- `can_read`
- `can_write`
- `can_list`
- `can_search`
- `can_execute_commands` — true only for filesystem-backed workspaces with
  an explicit command-execution opt-in.
- `can_apply_patch`

The runtime gates operations on capabilities and on the configured
`SideEffectPolicy`.

## Built-in tool I/O and the `Workspace` trait

The built-in tools (`Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash`) work
against a filesystem `cwd`. The SDK never lets them fall back to the host
process's working directory. Where they are bound depends on the request's
`PatchPolicy`:

| Patch policy | Workspace with a filesystem root | Rootless (`InMemoryWorkspace`, pure `Custom`) |
| --- | --- | --- |
| `ReturnOnly` (default) | Private scratch copy of the workspace's files | Private scratch copy of the workspace's files |
| `RejectMutations` | Read-only scratch copy; `Write`/`Edit`/`Bash` not registered | Same |
| `ApplyToWorkspace` / `ApplyAndReturn` | The real root | No built-in file/shell tools are registered |

The scratch copy is a temp dir populated from a snapshot of the
workspace's `list_files()` set (for a `FilesystemWorkspace` that set
honours `.gitignore` and skips hidden paths, so `.git/` and build outputs
are not copied). The returned patch is the diff of the scratch copy
against that snapshot; the temp dir is removed when the run ends.
Validation commands under these policies run in the scratch copy.

Custom tools are always registered and are responsible for routing their
own I/O through the SDK `Workspace` trait.
