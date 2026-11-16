# L4 real-world issue tasks (#296)

**Frozen:** 2026-11-15. No live L4 run happened before this date: the M17
baseline (#297) is still open.
**Owner sign-off:** 2026-11-16 (see [Sign-off](#sign-off)).

The `l4` suite is five real, closed issues from permissively licensed Rust
repositories. Each one is frozen as a v3 `level = "L4"` task:

- the fixture is the repository at the parent of the commit that resolved the
  issue, with its dependencies vendored;
- the prompt is the issue text exactly as filed;
- hidden verification is the upstream fix's tests, or an owner-written
  acceptance test in one case (tempfile, explained below);
- the reference is the upstream fix's non-test diff.

Snapshots follow the same layout as the [L3 corpus](../eval.md#l3-corpus-289).
They live in
[`mfaulk/crom-eval-fixtures`](https://github.com/mfaulk/crom-eval-fixtures)
at commit `d506fca9fda166ac6b5fdaa47c8e283d93a965fc`. Only `task.toml`,
`hidden/`, and `oracle.patch` are in this repository, under
`crates/crom-eval/eval/tasks/l4-*`.

## Selection criteria

A candidate had to meet all of these:

1. **Real and closed.** A GitHub issue in the upstream repository, closed by
   a merged PR.
2. **Permissive license.** MIT, Apache-2.0, or a dual license of the two.
3. **Behavior-only fix.** The upstream tests must use only APIs that exist at
   the base commit. A fix that adds a new public name the issue does not ask
   for (`dangerous::insecure_decode_claims`, `PatchFormatter::suppress_blank_empty`)
   cannot be graded fairly from the issue text alone.
4. **Reproducible.** The base fails the hidden tests, the base plus the fix
   passes public and hidden tests, and both run offline in
   `docker.io/library/rust:1-slim` with `--network none`.
5. **Small enough to vendor.** `vendor/` is under 100 MB after stubbing crates
   that never compile on x86_64 Linux, and the build needs no system packages
   beyond those in `rust:1-slim`.
6. **Not already used.** No repository from the `l3` corpus.

Preferences:

- issues filed after mid-2026, to reduce training-data contamination;
- at least one task in tooling, security, or MCP (#296);
- a spread of repositories and problem types.

## Selected tasks

| Task | Upstream issue | Fix PR | Filed / closed | License | Area |
|---|---|---|---|---|---|
| `l4-which-relative-path-cwd` | [harryfei/which-rs#127](https://github.com/harryfei/which-rs/issues/127) | [#128](https://github.com/harryfei/which-rs/pull/128) | 2026-08-17 / 2026-08-26 | MIT | tooling: executable lookup |
| `l4-tempfile-relative-temppath` | [Stebalien/tempfile#395](https://github.com/Stebalien/tempfile/issues/395) | [#396](https://github.com/Stebalien/tempfile/pull/396) | 2026-03-10 / 2026-03-11 | MIT OR Apache-2.0 | security: cleanup deletes the wrong file |
| `l4-quick-xml-dtd-chunk-panic` | [tafia/quick-xml#960](https://github.com/tafia/quick-xml/issues/960) | [#961](https://github.com/tafia/quick-xml/pull/961) | 2026-05-06 / 2026-05-07 | MIT | untrusted input: parser panic |
| `l4-toml-writer-quote-overflow` | [toml-rs/toml#1188](https://github.com/toml-rs/toml/issues/1188) | [#1189](https://github.com/toml-rs/toml/pull/1189) | 2026-07-14 / 2026-07-14 | MIT OR Apache-2.0 | serializer panic in a multi-crate workspace |
| `l4-glob-hidden-dir-leading-dot` | [rust-lang/glob#67](https://github.com/rust-lang/glob/issues/67) | [#128](https://github.com/rust-lang/glob/pull/128) | 2018-03-21 / 2023-01-11 | MIT OR Apache-2.0 | tooling: file matching |

Commits (full SHAs are also in each `task.toml` `[provenance]`):

| Task | `base_commit` (snapshot) | `fix_commit` |
|---|---|---|
| `l4-which-relative-path-cwd` | `7fba0e04d0674bbe8ccf5bfb4f09b1d00925806f` | `48e49d554b7a7c2812644d8d4950f50e995a9ecc` |
| `l4-tempfile-relative-temppath` | `929a1127bde5046733d535281203bf8ec1bf2c9e` | `e34e5748d66a48073ec8e1e6ba37338eecca4548` |
| `l4-quick-xml-dtd-chunk-panic` | `42cbb86ccf15a49eebcc61a83fe1125c7a0ddc9a` | `8128038a58feb3d10a4280897777ceec88d47853` |
| `l4-toml-writer-quote-overflow` | `771a975840ff876825c2973a79634ffe79a3bf5a` | `3adbbb7860810b146dbdedba1f315c90813083f2` |
| `l4-glob-hidden-dir-leading-dot` | `55d550eab0f7a0ba537baba77a12c2d11e32fe4d` | `4ab7f0fb50d3b74e16ba92824779823b888ab018` |

The tempfile fix landed as three rebased commits. Its base is the parent of the
first one, and `fix_commit` is the last. For the merge-commit PRs (quick-xml,
toml, glob), the base is the merge commit's first parent.

### Verification per task

| Task | Public oracle | Hidden oracle | Hidden source |
|---|---|---|---|
| `l4-which-relative-path-cwd` | `cargo test --offline` | `--test basic` | upstream `tests/basic.rs` (3 new tests) |
| `l4-tempfile-relative-temppath` | `cargo test --offline` | `--test temp_path_relative` | owner-written (2 tests) |
| `l4-quick-xml-dtd-chunk-panic` | `cargo test --offline` | `--test issues` | upstream `tests/issues.rs` (`issue960`) |
| `l4-toml-writer-quote-overflow` | `cargo test --offline -p toml_writer` | `-p toml_writer --test string` | upstream `crates/toml_writer/tests/string.rs` (`many_quotes`) |
| `l4-glob-hidden-dir-leading-dot` | `cargo test --offline --skip test_iteration_errors` | `--test glob-std` | upstream `tests/glob-std.rs` |

All five pass `crom eval validate-suite l4 --isolation podman --container-image
docker.io/library/rust:1-slim`, with the fixtures fetched from GitHub into an
empty cache. That means 0 errors per task: the unmodified snapshot fails, and
the snapshot plus `oracle.patch` passes both public and hidden tests.

## Why each task

- **which-rs #127.** An agent's shell tool resolves executables the same way,
  so this is tooling in Crom's own domain. The issue proposes the behavior but
  not the code. The fix has to keep platform-specific cases (tilde, Windows
  drive-relative paths) unchanged.
- **tempfile #395.** A security-relevant bug: dropping a `TempPath` after a
  `chdir` deletes a different file than the one it was created for. The issue
  gives a reproduction and the expected outcome.
- **quick-xml #960.** A fuzzer-found panic on malformed input in a streaming
  parser. The fix is a small state-machine change. The bug only appears when
  input arrives in small chunks, so the agent has to reason about buffer
  boundaries.
- **toml #1188.** An overflow panic in one crate of a nine-crate workspace,
  which tests navigation in a larger repository. The fixed output must also
  match the upstream snapshot test (`snapbox`) exactly.
- **glob #67.** A short, informal bug report with no code pointer. The agent
  has to find the directory-walk path that ignores `require_literal_leading_dot`.

## Candidates considered and rejected

| Candidate | Reason |
|---|---|
| Keats/jsonwebtoken#538 (`alg: none` in `insecure_decode`) | Fix adds `insecure_decode_claims`; hidden tests call a name the issue never mentions |
| bmwill/diffy#35 (blank context lines) | Fix adds an opt-in `suppress_blank_empty` and keeps the old default; an agent following the issue would change the default and fail |
| alexcrichton/tar-rs#326 (absolute paths) | Fix adds `Builder::preserve_absolute` |
| comex/rust-shlex GHSA-r7qv-8r2h-pg27 (`quote` misses `{`, `\xa0`) | A security advisory, not an issue; its tests are in-source in `src/lib.rs` and `src/bytes.rs`, so they cannot be overlaid without replacing the code under test |
| zip-rs/zip2#1149 | Compression dependencies need C toolchains (bzip2, zstd) and push `vendor/` well past the size budget |
| toml-rs/toml#1106 | Same repository as the selected toml task |
| Crom's own backlog | Vendoring the full Crom workspace (tokio, reqwest, rustls, ...) exceeds the size budget, and Crom's own issues are the most contaminated by the agent's own context. Left for a later round |

## Prompt and grading notes

- **Prompt format.** Each prompt has a short preamble: the crate,
  "keep the public API and tests green", no edits to `Cargo.toml`,
  `Cargo.lock`, `.cargo/`, or `vendor/`, and build with `--offline`. The
  issue title and body follow, verbatim. Links in the issue text point to
  GitHub, which the agent cannot reach (`network = "none"`).
- **tempfile: owner-written hidden test.** The upstream tests call
  `TempPath::try_from_path`, which is only one of three fixes the issue
  proposes. The hidden test `tests/temp_path_relative.rs` reproduces the
  issue's example through `TempPath::from_path` (drop and `close()`), which
  exists at the base commit. Any fix that meets the issue's "Expected" section
  passes.
- **which-rs: empty PATH entries.** The upstream tests also expect an empty
  Unix PATH entry to resolve against the provided `cwd`. The issue says only
  that empty entries keep "representing the current directory". An agent that
  reads "current directory" as the process cwd fails one of the three hidden
  tests.
- **quick-xml: suggested fix in the issue.** The issue includes its author's
  suggested fix. It is kept because it is part of the issue as filed, which
  makes this the easiest of the five.
- **glob: root-only public test.** `test_iteration_errors` assumes it runs as
  a non-root user who cannot read `/root`. The public oracle skips it, so the
  task also validates in a root container.
- **Canary warning.** `l4-which-relative-path-cwd` raises one
  `canary_hidden_names` warning, a false positive: the issue's heading
  "Proposed tests" contains the word `tests`, which is a path component of
  the hidden file.

## Contamination

Four of the five issues were filed in 2026. glob#67 dates from 2018 and its
fix from 2023, so it is the most likely to appear in model training data.
Results should be reported per task, with that caveat for glob.

## Snapshot details

- `vendor/` sizes: which 45M, tempfile 28M, quick-xml 55M, toml 83M, glob
  6.5M.
- Lockfiles: tempfile, quick-xml, and glob have no upstream `Cargo.lock`, so
  their snapshots carry one generated on 2026-11-15.
- Stubs: vendored crates that never compile on x86_64 Linux (Windows, wasi,
  redox, hermit, `web-sys`/`js-sys`) are replaced by stubs, as in L3.
- Line endings: the toml and quick-xml snapshots add `vendor/.gitattributes`
  (`* -text`). Without it, the upstream `* text eol=lf` would normalize CRLF
  test data in vendored crates and invalidate `.cargo-checksum.json`.
- Harness fix: `copy_dir_recursive` used to drop every directory named
  `target`, including the `cc` crate's vendored `src/target/` module. Below
  the root it now skips only Cargo build dirs (`CACHEDIR.TAG` or
  `.rustc_info.json`). Without this, `l4-quick-xml-dtd-chunk-panic` could not
  build.

## Running

```console
crom eval validate-suite l4                      # fetches the fixtures commit once
CROM_EVAL_PODMAN=1 crom eval validate-suite l4 --isolation podman \
  --container-image docker.io/library/rust:1-slim
crom eval run --suite l4 --provider <profile>    # #297: 3 attempts per task
```

## Sign-off

The owner signed off on this selection on 2026-11-16, before any live L4 run.
Changing a task after sign-off means bumping its `task_version` and recording
the reason here.

| Date | Owner | Decision |
|---|---|---|
| 2026-11-16 | @mfaulk | approved as frozen (2026-11-15); no live L4 run before sign-off |
