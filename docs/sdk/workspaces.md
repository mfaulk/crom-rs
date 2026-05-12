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
against a filesystem `cwd`. The SDK sets `cwd` to the workspace's
`filesystem_root()` when available. Consequences:

- `FilesystemWorkspace`, `ReadOnlyWorkspace`/`PatchOnlyWorkspace`/
  `OverlayWorkspace` wrapping a filesystem base — built-in tools read and
  write under the workspace root as expected.
- `InMemoryWorkspace` or pure `Custom` workspaces — built-in tools fall
  back to the process's current working directory. For headless services,
  configure `ToolPolicy::enable_builtin_tools` to disable Bash/Edit/Write
  (the default already does this), or only register custom tools that
  route through the SDK `Workspace` trait.

In other words, the SDK `Workspace` trait is the contract for *patches*
and *diffs*; the built-in tools are the contract for *agent I/O*. They
overlap fully on filesystem workspaces and partially elsewhere.
