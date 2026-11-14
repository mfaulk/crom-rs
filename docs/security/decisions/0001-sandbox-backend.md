# ADR 0001: Sandbox backend for agent tool execution (Linux)

- **Status:** Proposed — requires owner sign-off (`owner-action`)
- **Date:** 2026-11-14
- **Issue:** #276 · consumed by #253 (implementation), #292 (adversarial suite)
- **Properties:** P1–P9 in [`../threat-model.md` §7](../threat-model.md#7-required-enforcement-properties)

## 1. Context

Agent Bash, eval agent tool calls, validation commands and MCP stdio servers
run as plain children of the host process with the user's full privileges
(threat model §5, gaps G1–G3, G10). M17 S2 requires OS enforcement of P1–P7 on
Linux and fail-closed behaviour elsewhere. The backend must work for a
`cargo test` inside the workspace (toolchain + registry read-only, no network)
and on the GitHub `ubuntu-24.04` runners used by CI and the eval lanes (#295).

## 2. Options

**(a) landlock + seccomp + rlimits, applied in the child before `exec`.**
A per-call supervisor (the `crom` binary re-executed in a hidden
`__sandbox-exec` mode, or `pre_exec` in a single-threaded shim) does:
`PR_SET_CHILD_SUBREAPER`; spawn the child in its own process group; in the
child: `env_clear` + allowlist, `setrlimit`, `PR_SET_NO_NEW_PRIVS`,
`landlock_restrict_self` (FS rw: workspace + private tmp; ro: allowlist; TCP
bind/connect handled with no rules when ABI ≥ 4; signal + abstract-unix scope
when ABI ≥ 6), seccomp filter denying `socket(AF_INET|AF_INET6|AF_PACKET)`;
`execve`. After the child exits (or on timeout/cancel), the supervisor
SIGKILLs every remaining descendant (they reparent to it) until none remain.
Per-call PID/memory bounds: a cgroup v2 scope (`systemd-run --user --scope
-p TasksMax= -p MemoryMax=`, or direct writes to a delegated cgroup).
Crates: `landlock` 0.4.7, `seccompiler` 0.5.0, `libc` (or `rlimit`).

**(b) bubblewrap.** External `bwrap` binary: user + mount + PID + net
namespaces, bind-mount allowlist, `--clearenv`, `--die-with-parent`,
`--new-session`. PID namespace kills the whole tree when the init exits.

**(c) rootless podman / docker per task or per tool call.** `run --rm
--network none --pids-limit --memory -v ws:ws`, toolchain bind-mounted
read-only or baked into the image. Already used for eval *verification*
(`crates/eval/src/isolation.rs:244-291`).

## 3. Measured evidence

All probes: [Appendix C](#appendix-c-spikesh). Same probe script under each
backend: write a file outside the workspace, read a synthetic `~/.ssh` secret
outside the workspace, TCP connect to a host `127.0.0.1` HTTP server, UDP
`connect` to `192.0.2.1:53`, TCP to `1.1.1.1:443`, check a fake provider key
in the child env, `setsid sleep 4242 &` then exit and check it is gone,
`RLIMIT_NPROC=256` fork count, mean wall time of `true` over 20 warm calls,
and `cargo test --offline` on a zero-dep crate and a crate depending on `libc`
from a read-only `CARGO_HOME` (target dir in the workspace, cold).

### 3.1 Platforms (2026-11-14)

| Host | Kernel | Landlock ABI | `apparmor_restrict_unprivileged_userns` | `unshare -Ur` | bwrap | podman / docker | `systemd-run --user --scope` |
|---|---|---|---|---|---|---|---|
| Local: Fedora 44 Workstation | 7.3.3-200.fc44 | 11 | n/a (SELinux) | ok | 0.12.0 | podman 5.8.9 | ok |
| GH `ubuntu-24.04`, image `ubuntu24 20261111.127.1` | 7.0.0-1014-aws | 8 | **1** | **fails** (`write /proc/self/uid_map: Operation not permitted`) | not preinstalled; apt `0.9.0-1ubuntu0.3` **fails** (`setting up uid map: Permission denied`; with `--unshare-net`: `loopback: Failed RTM_NEWADDR`) | podman 4.9.3 rootless (has AppArmor profile) · docker 28.0.4 (rootful daemon) | ok |
| GH `ubuntu-22.04`, image `ubuntu22 20261111.360.2` | 6.8.0-1070-azure | 4 | 0 | ok | apt `0.6.1-1ubuntu0.3` ok | podman 3.4.4 rootless · docker 28.0.4 | ok |

Runner logs: run [43566979251](https://github.com/mfaulk/crom-rs/actions/runs/43566979251)
(primary) and [43566732153](https://github.com/mfaulk/crom-rs/actions/runs/43566732153)
(first attempt; same platform facts).

### 3.2 Enforcement results

`blocked` = the probe failed inside the sandbox (desired). Identical on all
three hosts for every backend that started (docker was not run on the
Fedora host).

| Probe | bare | (a) landlock | (a) + systemd scope | (b) bwrap | (c) podman | (c) docker |
|---|---|---|---|---|---|---|
| P1 write outside | ALLOWED | blocked | blocked | blocked | blocked | blocked |
| P2 read synthetic secret | ALLOWED | blocked | blocked | blocked | blocked | blocked |
| P3 TCP to host loopback | ALLOWED | blocked | blocked | blocked | blocked | blocked |
| P3 UDP connect | ALLOWED | blocked | blocked | blocked | blocked | blocked |
| P3 TCP public | ALLOWED | blocked | blocked | blocked | blocked | blocked |
| P4 provider key in env | leaked | absent | absent | absent | absent | absent |
| P5 `setsid sleep` after return | survived | reaped | reaped | reaped | reaped | reaped |
| `cargo test --offline` (zero-dep, registry dep) | ok | ok | ok | ok | ok | ok |
| Starts on GH `ubuntu-24.04` (default) | — | **yes** | **yes** | **no** | yes | yes |

Not measured: memory limit under rustc, file-size limit, timeout/cancel paths
(only normal exit was tested for P5), macOS.

### 3.3 Process-count bound (P6)

| Host | User's processes | `RLIMIT_NPROC=256` forks, no userns (bare, landlock) | Same, inside userns (bwrap) | cgroup `TasksMax=512` |
|---|---|---|---|---|
| Fedora 44 | 328 | **0** | 253 | 511 forks then `EAGAIN` |
| GH 24.04 | 13 | 194 | 253 (restriction lifted) | 511 |
| GH 22.04 | 13 | 199 | 253 | 511 |

`RLIMIT_NPROC` counts all processes of the real UID on the host, so it is not a
per-call bound: on a workstation a usable value either breaks every fork (0 on
Fedora) or does not bound the call. A cgroup `pids.max` is per call.

### 3.4 Overhead (ms)

Mean of 20 warm `true` invocations, including spawn of the backend itself.
`cargo test` columns are single cold runs (wall time is dominated by
compilation; differences there are within noise).

| Host | bare | (a) landlock | (a) + systemd scope | (b) bwrap | (c) podman | (c) docker |
|---|---|---|---|---|---|---|
| Fedora 44 (`true`) | 1 | 3 | 11 | 11 | 139 | — |
| GH 24.04 (`true`) | 1 | 2 | 6 | 3 ¹ | 82 | 79 |
| GH 22.04 (`true`) | 1 | 2 | 8 | 4 | 280 | 129 |
| GH 24.04 `cargo test` zero-dep / libc-dep | 951 ² / 668 | 118 / 682 | 120 / 697 | 108 / 667 ¹ | 191 / 760 | 189 / 760 |
| GH 22.04 `cargo test` zero-dep / libc-dep | 379 ² / 830 | 147 / 835 | 159 / 853 | 146 / 828 | 331 / 1031 | 327 / 1082 |

¹ After `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`.
² First cargo invocation on the runner (cold disk cache).

### 3.5 Other observations

- Cargo 1.99.1 builds and tests with `CARGO_HOME` read-only (registry already
  populated, `--offline`) under every backend; no write to `CARGO_HOME` was
  needed. Network-dependent fetches must happen outside the sandbox
  (MILESTONE_17 risk 2).
- Rootful docker without `--user` left root-owned files in the workspace that
  the runner user could not delete (first attempt, run 43566732153).
- Podman on Fedora required `--security-opt label=disable` (or `:z` relabel)
  for bind mounts under `/tmp`.
- Landlock ABI 4 (22.04) has no signal scoping: a sandboxed process can still
  `kill(2)` other processes of the same UID, including the host process. ABI ≥ 6
  closes this (`Scope::Signal`).
- Landlock ABI < 3 does not restrict `truncate(2)` on files that are only
  readable; the spike helper refuses to run below ABI 3 (exit 97).

## 4. Comparison

| Criterion | (a) landlock + seccomp | (b) bubblewrap | (c) podman / docker |
|---|---|---|---|
| P1/P2 FS | Kernel-enforced path-beneath rules; ABI ≥ 3 needed for truncate | Mount namespace, only bound paths exist | Mount namespace |
| P3 network | All-or-nothing via seccomp (TCP+UDP+raw); TCP **port** allowlist possible with ABI ≥ 4; no host allowlist | All-or-nothing (netns) | All-or-nothing (`--network none`); custom networks for more |
| P4 env | `env_clear` + allowlist in the shim | `--clearenv` | Only `-e` vars |
| P5 tree | Subreaper supervisor + group kill; cgroup `cgroup.kill` if a scope is used | PID namespace (kernel-guaranteed) | PID namespace |
| P6 limits | `setrlimit` for CPU/FSIZE/NOFILE; PIDs/memory need a cgroup scope | Same rlimits; NPROC per userns works; memory needs cgroup | `--pids-limit`, `--memory`, `--cpus` |
| P7 fail closed | In-process probe of ABI and `RulesetStatus::FullyEnforced`; refuse otherwise | Probe by running `bwrap true`; refuse on failure | Probe engine; refuse on failure |
| Kernel/distro | Landlock in the active LSM list (verified: Ubuntu 22.04/24.04 runners, Fedora 44); ABI 3 needs kernel ≥ 6.2 | Unprivileged userns; **blocked by default on Ubuntu ≥ 23.10** (AppArmor) unless an AppArmor profile or sysctl is changed by root | Engine installed; rootless needs userns (podman ships an AppArmor profile on Ubuntu) |
| GH `ubuntu-24.04` | Works unmodified (ABI 8) | Fails unless `sudo sysctl` | Works |
| Toolchain / `~/.cargo` | Read-only rules on host paths; host toolchain used as-is | Read-only binds; host toolchain | Bind-mount host toolchain (needs compatible image with `cc`) or bake into image |
| Per-call overhead | 2–3 ms (6–11 ms with systemd scope) | 3–11 ms | 79–280 ms |
| macOS / unsupported | Compile-time `cfg(target_os = "linux")`; runtime refuse | Binary absent ⇒ refuse | Possible on macOS via VM, but not equivalent; refuse |
| Rust complexity | Moderate: shim mode, ruleset construction, seccomp BPF, supervisor kill loop; no external binary | Low: build an argv; external dependency + version skew (0.6 vs 0.9 vs 0.12) | Low code, high ops: images, UID mapping, SELinux labels, daemon |
| Residual risk | Kernel LSM bugs; no mount isolation (paths visible but denied); `/proc` of same-UID processes readable unless excluded | Userns attack surface | Daemon is root-equivalent (docker); image supply chain |

## 5. Decision (proposed)

1. **Primary: (a) landlock + seccomp + rlimits**, applied by a per-call
   supervisor shim (the `crom` binary in a hidden exec mode, so no extra
   artifact ships), with:
   - Minimum landlock ABI 3 for `required`; ABI ≥ 4 adds TCP rules; ABI ≥ 6
     adds signal/abstract-socket scoping. The effective ABI is recorded in the
     audit event (#291).
   - Network: deny `AF_INET`/`AF_INET6`/`AF_PACKET` sockets via seccomp by
     default. When policy grants network, lift the filter entirely
     (all-or-nothing). Per-host allowlists are out of scope for M17.
   - Process tree: subreaper supervisor + process-group kill on exit, timeout,
     cancel and drop; supervisor sets `PR_SET_PDEATHSIG` so the tree dies with
     the host. `kill_on_drop` targets the supervisor.
   - PIDs/memory: per-call cgroup v2 scope when available
     (`systemd-run --user --scope` measured at +4–8 ms; available on all three
     hosts). `RLIMIT_NPROC` is not used as the per-call bound.
   - Writable set excludes `<workspace>/.agent/` (threat model G9);
     grader material is outside every allowlisted path (P8, #286).
2. **Fallback: (b) bubblewrap**, used only when landlock is missing or
   ABI < 3 *and* `bwrap` can create a user namespace (probed at startup).
   Covers older kernels (e.g. Debian 12, 6.1, ABI 2) where userns is allowed.
3. **(c) containers stay the eval verification backend** and an optional
   per-*task* backend for eval (`--isolation podman`). They are not used per
   tool call: 79–280 ms per call and image/UID/label management.
4. **Fail closed:** with `sandbox = required` (default for live eval runs,
   #253), if neither backend can fully enforce, the call is refused before
   spawn. macOS/Windows always refuse under `required`; `off` runs unsandboxed
   and emits an explicit `Unsandboxed` audit event.

Rationale: (a) is the only option that works unmodified on the GitHub
`ubuntu-24.04` runner *and* on stock Ubuntu ≥ 23.10 desktops (where
unprivileged userns is AppArmor-restricted), has the lowest overhead, uses the
host toolchain directly, and needs no external binary. Its weaknesses —
per-UID `RLIMIT_NPROC` and no PID namespace — are covered by the subreaper
supervisor (measured: `setsid` child reaped) and a cgroup scope.

## 6. MCP stdio servers (MILESTONE_17 risk 11)

**Opt-in in M17** (`mcp.servers.<name>.sandbox = "required"`; default off).
MCP servers commonly need network and `$HOME` caches (`npx`, `uvx`), so a
default-on sandbox with `network=none` would break most configured servers
without a per-server allowlist design. Repo-sourced servers are instead gated
by the trust decision in #290 (P9), and host `${VAR}` interpolation is not
applied for untrusted workspaces.

## 7. Requirements handed to #253

- Implement P1–P7 with the tests in threat model §7 on the Linux CI lane
  (`ubuntu-24.04`); add a macOS job asserting refusal under `required`.
- Cover timeout, cancel, and future-drop for P5 (the spike measured normal
  exit only).
- Measure and record per-call overhead in eval results.
- Validate `MemoryMax`/`RLIMIT_AS` against a real `cargo build` of this
  repository before choosing defaults.
- Read-only allowlist: `/usr`, `/etc`, `/bin`, `/lib`, `/lib64`, `/proc`
  (consider excluding other PIDs), `/dev/null` (rw), `/dev/urandom`, the
  active rustup toolchain, `$CARGO_HOME/registry` and `$CARGO_HOME/git`.

## 8. Open questions for the owner

1. Minimum landlock ABI for `required`: 3 (FS-only guarantees; ubuntu-22.04 and
   Ubuntu 24.04 GA kernels qualify) or 6 (adds signal scoping; excludes 22.04
   runners and 6.8 kernels)? Proposed: 3, with ABI recorded per run.
2. If no cgroup scope is available (no systemd user session, e.g. inside a
   container), should `required` refuse (strict P6) or run with rlimits only
   and flag the result? Proposed: refuse.
3. Is bubblewrap worth implementing as a fallback in M17, or should
   ABI < 3 hosts simply fail closed? Proposed: defer to after #253 lands if
   time is short.
4. Confirm MCP stdio sandbox stays opt-in for M17.

## Appendix A: spike workflow

Temporary workflow, removed before merge. Spike sources were committed under
`docs/security/spike/` for the run and removed in the same commit.

```yaml
# Temporary feasibility spike for #276 (sandbox backend ADR). Removed before merge;
# the content is preserved in docs/security/decisions/0001-sandbox-backend.md.
name: sandbox-spike

on:
  pull_request:
    paths:
      - .github/workflows/sandbox-spike.yml
      - docs/security/**

permissions:
  contents: read

jobs:
  spike:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-24.04, ubuntu-22.04]
    runs-on: ${{ matrix.os }}
    timeout-minutes: 25
    steps:
      - uses: actions/checkout@v6

      - name: Platform facts
        run: |
          set +e
          echo "image: ${ImageOS:-?} ${ImageVersion:-?}"
          grep PRETTY_NAME /etc/os-release
          echo "kernel: $(uname -r)"
          echo "lsm: $(cat /sys/kernel/security/lsm)"
          python3 -c 'import ctypes; l=ctypes.CDLL(None, use_errno=True); print("landlock_abi:", l.syscall(444, None, 0, 1), "errno:", ctypes.get_errno())'
          for k in kernel.apparmor_restrict_unprivileged_userns kernel.unprivileged_userns_clone user.max_user_namespaces kernel.yama.ptrace_scope; do
            echo "$k = $(sysctl -n $k 2>&1)"
          done
          unshare -Ur true && echo "unshare -Ur: ok" || echo "unshare -Ur: FAILED"
          unshare -Urn true && echo "unshare -Urn: ok" || echo "unshare -Urn: FAILED"
          echo "cgroup fs: $(stat -fc %T /sys/fs/cgroup)"
          echo "cgroup delegated (user@): $(cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.subtree_control 2>&1)"
          systemd-run --user --scope -q true && echo "systemd-run --user --scope: ok" || echo "systemd-run --user --scope: FAILED"
          echo "user procs: $(ps -u "$(id -u)" --no-headers | wc -l); ulimit -u: $(ulimit -u)"
          for t in bwrap podman docker; do echo "$t: $(command -v $t) $($t --version 2>&1 | head -1)"; done
          docker info --format 'docker rootless={{.SecurityOptions}} cgroup={{.CgroupVersion}}' 2>&1
          podman info --format 'podman rootless={{.Host.Security.Rootless}} cgroup={{.Host.CgroupsVersion}}' 2>&1
          ls /etc/apparmor.d | grep -iE 'bwrap|podman|unpriv' || true
          rustc --version; cargo --version

      - name: Install bubblewrap (if missing)
        run: command -v bwrap || (sudo apt-get update -qq && sudo apt-get install -y -qq bubblewrap)

      - name: bwrap smoke
        run: |
          set +e
          bwrap --unshare-all --ro-bind / / true; echo "bwrap --unshare-all rc=$?"
          bwrap --unshare-user --ro-bind / / true; echo "bwrap --unshare-user rc=$?"

      - name: Build landlock helper
        run: |
          cp -r docs/security/spike "$RUNNER_TEMP/spike"
          cd "$RUNNER_TEMP/spike/sbx" && cargo build --release -q
          ls -la target/release/sbx

      - name: Run spike (all backends)
        env:
          SPIKE_DIR: ${{ runner.temp }}/spike
          BACKENDS: bare landlock landlock-scope bwrap podman docker
          IMG_CARGO: docker.io/library/rust:1-slim
          N: "20"
        run: bash "$RUNNER_TEMP/spike/spike.sh"

      - name: systemd scope pids bound
        run: |
          set +e
          systemd-run --user --scope -q -p TasksMax=512 python3 "$RUNNER_TEMP/spike/forkcount.py"

      - name: bwrap with AppArmor userns restriction lifted (admin opt-in)
        if: matrix.os == 'ubuntu-24.04'
        env:
          SPIKE_DIR: ${{ runner.temp }}/spike
          BACKENDS: bwrap
          N: "20"
        run: |
          sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
          bash "$RUNNER_TEMP/spike/spike.sh"
```

## Appendix B: landlock helper (`sbx`)

`Cargo.toml`: `landlock = "=0.4.7"`, `seccompiler = "=0.5.0"`, `libc = "0.2"`.

<details><summary><code>src/main.rs</code></summary>

```rust
// Spike: landlock + seccomp + rlimits + env allowlist + subreaper tree kill.
// usage: sbx [--rw DIR]... [--ro DIR]... [--env NAME]... [--nproc N] -- cmd args...
use landlock::{
    path_beneath_rules, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, ABI,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::process::{exit, Command};

fn kernel_abi() -> i64 {
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)
    unsafe { libc::syscall(444, std::ptr::null::<u8>(), 0usize, 1u32) }
}

fn abi_from(n: i64) -> ABI {
    match n {
        n if n >= 6 => ABI::V6,
        5 => ABI::V5,
        4 => ABI::V4,
        3 => ABI::V3,
        2 => ABI::V2,
        1 => ABI::V1,
        _ => ABI::Unsupported,
    }
}

struct Opts {
    rw: Vec<String>,
    ro: Vec<String>,
    env: Vec<String>,
    nproc: Option<u64>,
    cmd: Vec<String>,
}

fn parse(args: &[String]) -> Opts {
    let mut o = Opts { rw: vec![], ro: vec![], env: vec![], nproc: None, cmd: vec![] };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rw" => { o.rw.push(args[i + 1].clone()); i += 2; }
            "--ro" => { o.ro.push(args[i + 1].clone()); i += 2; }
            "--env" => { o.env.push(args[i + 1].clone()); i += 2; }
            "--nproc" => { o.nproc = Some(args[i + 1].parse().unwrap()); i += 2; }
            "--" => { o.cmd = args[i + 1..].to_vec(); break; }
            other => { eprintln!("sbx: bad arg {other}"); exit(2); }
        }
    }
    o
}

fn setrl(res: libc::__rlimit_resource_t, v: u64) {
    let r = libc::rlimit { rlim_cur: v, rlim_max: v };
    if unsafe { libc::setrlimit(res, &r) } != 0 {
        eprintln!("sbx: setrlimit failed");
        exit(98);
    }
}

fn seccomp_no_inet() -> BpfProgram {
    // Deny socket(AF_INET|AF_INET6|AF_PACKET, ...) with EACCES; AF_UNIX stays allowed.
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    let mut v = vec![];
    for fam in [libc::AF_INET, libc::AF_INET6, libc::AF_PACKET] {
        v.push(
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                fam as u64,
            )
            .unwrap()])
            .unwrap(),
        );
    }
    rules.insert(libc::SYS_socket, v);
    SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EACCES as u32),
        std::env::consts::ARCH.try_into().unwrap(),
    )
    .unwrap()
    .try_into()
    .unwrap()
}

fn inner(o: Opts) -> ! {
    let k = kernel_abi();
    let abi = abi_from(k);
    // Fail closed: ABI < 3 cannot restrict truncate(2) on read-only paths.
    if k < 3 {
        eprintln!("sbx: landlock ABI {k} < 3; refusing (fail closed)");
        exit(97);
    }
    setrl(libc::RLIMIT_CPU, 600);
    setrl(libc::RLIMIT_FSIZE, 1 << 30);
    setrl(libc::RLIMIT_NOFILE, 4096);
    if let Some(n) = o.nproc {
        setrl(libc::RLIMIT_NPROC, n);
    }
    let mut rs = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .unwrap();
    if k >= 4 {
        rs = rs.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp).unwrap();
    }
    if k >= 6 {
        rs = rs.scope(Scope::AbstractUnixSocket | Scope::Signal).unwrap();
    }
    let created = rs
        .create()
        .unwrap()
        .add_rules(path_beneath_rules(&o.ro, AccessFs::from_read(abi)))
        .unwrap()
        .add_rules(path_beneath_rules(&o.rw, AccessFs::from_all(abi)))
        .unwrap();
    let st = created.restrict_self().unwrap();
    if st.ruleset != RulesetStatus::FullyEnforced {
        eprintln!("sbx: landlock not fully enforced; refusing");
        exit(97);
    }
    seccompiler::apply_filter(&seccomp_no_inet()).unwrap();
    if std::env::var_os("SBX_VERBOSE").is_some() {
        eprintln!("sbx: landlock abi={k} enforced; seccomp no-inet applied");
    }
    let keep: Vec<(String, String)> = o
        .env
        .iter()
        .filter_map(|n| std::env::var(n).ok().map(|v| (n.clone(), v)))
        .collect();
    let err = Command::new(&o.cmd[0]).args(&o.cmd[1..]).env_clear().envs(keep).exec();
    eprintln!("sbx: exec: {err}");
    exit(127);
}

fn descendants_of(me: i32) -> Vec<i32> {
    // Direct children only; orphans reparent to us (subreaper) on the next pass.
    std::fs::read_to_string(format!("/proc/{me}/task/{me}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--inner") {
        inner(parse(&args[2..]));
    }
    // Supervisor: become subreaper so setsid/double-forked descendants reparent here.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    let me = std::process::id() as i32;
    let mut child = Command::new("/proc/self/exe")
        .arg("--inner")
        .args(&args[1..])
        .process_group(0)
        .spawn()
        .expect("spawn");
    let status = child.wait().expect("wait");
    // Kill and reap everything still parented to us, until none remain.
    loop {
        let d = descendants_of(me);
        if d.is_empty() {
            break;
        }
        for p in &d {
            unsafe { libc::kill(*p, libc::SIGKILL) };
        }
        for _ in &d {
            unsafe { libc::waitpid(-1, std::ptr::null_mut(), 0) };
        }
    }
    exit(status.code().unwrap_or(128));
}
```

</details>

## Appendix C: spike.sh

`tinycrate`: one `#[test]`, no dependencies. `depcrate`: same, plus
`libc = "=0.2.191"`. `forkcount.py`: fork up to 800 children that sleep 2 s,
print the count reached.

<details><summary><code>spike.sh</code></summary>

```bash
#!/usr/bin/env bash
# Sandbox feasibility spike. Runs the same probes under each available backend.
# env: SPIKE_DIR (holds tinycrate/, depcrate/, sbx binary at $SBX), BACKENDS,
#      IMG_CARGO (container image with cc), IMG_TRUE (small image), N (timing reps)
set -u
SPIKE_DIR=${SPIKE_DIR:?}
SBX=${SBX:-$SPIKE_DIR/sbx/target/release/sbx}
BACKENDS=${BACKENDS:-"bare landlock bwrap podman"}
IMG_CARGO=${IMG_CARGO:-docker.io/library/rust:1-slim}
IMG_TRUE=${IMG_TRUE:-docker.io/library/alpine:3}
N=${N:-20}
TC=$(dirname "$(dirname "$(rustup which cargo)")")
CH=${CARGO_HOME:-$HOME/.cargo}

ROOT=$(mktemp -d "${TMPDIR:-/tmp}/spike.XXXXXX")
WS=$ROOT/ws; OUT=$ROOT/outside; FAKEHOME=$ROOT/home
mkdir -p "$WS" "$OUT" "$FAKEHOME/.ssh"
echo "SYNTHETIC-SECRET" > "$FAKEHOME/.ssh/id_fake"
cp -r "$SPIKE_DIR/tinycrate" "$SPIKE_DIR/depcrate" "$WS/"
( cd "$SPIKE_DIR/depcrate" && cargo fetch -q )
export FAKE_PROVIDER_KEY=sk-fake-should-not-leak

python3 -u -m http.server 0 --bind 127.0.0.1 >"$ROOT/http.log" 2>&1 &
HTTP_PID=$!
sleep 1
PORT=$(grep -o 'port [0-9]*' "$ROOT/http.log" | awk '{print $2}')

cat > "$WS/probe.sh" <<EOF
r() { n=\$1; shift; if "\$@" >/dev/null 2>&1; then echo "\$n=ALLOWED"; else echo "\$n=blocked"; fi; }
r write_outside sh -c 'echo x > $OUT/pwned'
r read_secret cat $FAKEHOME/.ssh/id_fake
r net_tcp_local bash -c 'exec 3<>/dev/tcp/127.0.0.1/$PORT'
r net_udp_connect bash -c 'exec 3<>/dev/udp/192.0.2.1/53'
r net_tcp_public timeout 5 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443'
if command -v python3 >/dev/null; then python3 -c '
import os,resource,time
resource.setrlimit(resource.RLIMIT_NPROC,(256,256)); n=0
for i in range(300):
    try:
        pid=os.fork()
    except OSError:
        break
    if pid==0:
        time.sleep(3); os._exit(0)
    n+=1
print("nproc256_forks=%d" % n)'; else echo nproc256_forks=n/a; fi
if [ -n "\${FAKE_PROVIDER_KEY:-}" ]; then echo env_key=LEAKED; else echo env_key=absent; fi
setsid sleep 4242 </dev/null >/dev/null 2>&1 &
echo spawned_setsid=yes
EOF

ENVS="PATH=$TC/bin:/usr/local/bin:/usr/bin:/bin HOME=$WS CARGO_HOME=$CH TMPDIR=$WS/.tmp CARGO_TARGET_DIR=$WS/target"
mkdir -p "$WS/.tmp"

run() { # backend, cmd...
  local b=$1; shift
  case $b in
    bare) env $ENVS FAKE_PROVIDER_KEY=$FAKE_PROVIDER_KEY "$@" ;;
    landlock)
      env $ENVS FAKE_PROVIDER_KEY=$FAKE_PROVIDER_KEY "$SBX" \
        --rw "$WS" --rw /dev/null --ro /usr --ro /etc --ro /bin --ro /lib --ro /lib64 \
        --ro /proc --ro /dev/urandom --ro "$TC" --ro "$CH" \
        --env PATH --env HOME --env CARGO_HOME --env TMPDIR --env CARGO_TARGET_DIR -- "$@" ;;
    landlock-scope)
      systemd-run --user --scope -q -p TasksMax=512 -p MemoryMax=2G \
        env $ENVS FAKE_PROVIDER_KEY=$FAKE_PROVIDER_KEY "$SBX" \
        --rw "$WS" --rw /dev/null --ro /usr --ro /etc --ro /bin --ro /lib --ro /lib64 \
        --ro /proc --ro /dev/urandom --ro "$TC" --ro "$CH" \
        --env PATH --env HOME --env CARGO_HOME --env TMPDIR --env CARGO_TARGET_DIR -- "$@" ;;
    bwrap)
      bwrap --unshare-all --die-with-parent --new-session --tmpfs /tmp \
        --ro-bind /usr /usr --ro-bind /etc /etc \
        --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
        --ro-bind "$TC" "$TC" --ro-bind "$CH" "$CH" --bind "$WS" "$WS" \
        --proc /proc --dev /dev --clearenv \
        $(for e in $ENVS; do printf -- '--setenv %s %s ' "${e%%=*}" "${e#*=}"; done) \
        --chdir "$WS" "$@" ;;
    podman|docker)
      local img=$IMG_CARGO; [ "$1" = true ] && img=$IMG_TRUE
      local extra=(--user "$(id -u):$(id -g)"); [ $b = podman ] && extra=(--userns=keep-id --security-opt label=disable)
      $b run --rm --network none --pids-limit 512 --memory 2g "${extra[@]}" \
        -v "$WS:$WS" -v "$TC:$TC:ro" -v "$CH:$CH:ro" -w "$WS" \
        $(for e in $ENVS; do printf -- '-e %s ' "$e"; done) \
        "$img" "$@" ;;
  esac
}

ms() { echo $(( ($2 - $1) / 1000000 )); }

echo "host: kernel=$(uname -r) user_procs=$(ps -u "$(id -u)" --no-headers | wc -l) ulimit_u=$(ulimit -u)"
for b in $BACKENDS; do
  echo "=== backend: $b"
  case $b in
    bare) ;;
    landlock) [ -x "$SBX" ] || { echo "  unavailable"; continue; } ;;
    landlock-scope) systemd-run --user --scope -q true >/dev/null 2>&1 || { echo "  unavailable (systemd-run --user --scope failed)"; continue; } ;;
    *) command -v $b >/dev/null 2>&1 || { echo "  unavailable"; continue; } ;;
  esac
  if [ $b = podman ] || [ $b = docker ]; then
    $b pull -q "$IMG_TRUE" >/dev/null 2>&1; $b pull -q "$IMG_CARGO" >/dev/null 2>&1
  fi
  rm -f "$OUT/pwned"
  ( cd "$WS" && run $b bash "$WS/probe.sh"; echo "probe_rc=$?" ) 2>&1 | sed 's/^/  /'
  sleep 0.5
  if pgrep -f '^sleep 4242$' >/dev/null; then echo "  setsid_child=SURVIVED"; pkill -f '^sleep 4242$'; else echo "  setsid_child=reaped"; fi
  [ -e "$OUT/pwned" ] && echo "  (outside file exists)"
  # first true includes cold-start; then N warm reps
  run $b true >/dev/null 2>&1
  t0=$(date +%s%N); for _ in $(seq "$N"); do run $b true >/dev/null 2>&1; done; t1=$(date +%s%N)
  echo "  true_x${N}_mean_ms=$(( $(ms $t0 $t1) / N ))"
  for c in tinycrate depcrate; do
    cmd=(sh -c "cd $WS/$c && cargo test --offline -q 2>&1 | grep -E 'test result|^warning|^error' | head -2")
    rm -rf "$WS/target"
    t0=$(date +%s%N); outp=$(run $b "${cmd[@]}" 2>&1); rc=$?; t1=$(date +%s%N)
    echo "  cargo_test_${c}: rc=$rc ms=$(ms $t0 $t1) last=[$(echo "$outp" | tr '\n' ' ' | cut -c1-160)]"
  done
done
kill $HTTP_PID 2>/dev/null
rm -rf "$ROOT" 2>/dev/null
exit 0
```

</details>
