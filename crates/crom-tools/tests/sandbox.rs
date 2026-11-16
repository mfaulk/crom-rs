//! Enforcement tests for the Bash OS sandbox (threat model §7, P1–P7).
//!
//! On Linux each test skips with an explicit `SKIP:` line when the kernel
//! cannot enforce the sandbox; set `CROM_REQUIRE_SANDBOX_TESTS=1` (as the
//! ubuntu CI lane does) to turn a skip into a failure.
//!
//! The P6 containment tests (fork loop, fork bomb, memory) take down the host
//! if the sandbox is broken: a fork bomb run without it exhausted 300 GB of RAM
//! on 2026-11-14 (#253). They are `#[ignore]`d and run only through
//! `scripts/sandbox-tests.sh`, which places the test binary in a cgroup scope
//! with `TasksMax`/`MemoryMax`. Each one also refuses to start unless it can
//! see that outer limit, and unless a probe through the same Bash path shows
//! the sandbox is actually engaged. Never weaken these guards for a mutation
//! check.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crom_core::{SandboxConfig, SandboxMode, SessionId, Tool, ToolContext, ToolError, ToolResult};
use crom_tools::{BashTool, ToolLimits};
use serde_json::json;
use tempfile::TempDir;

fn tool(cfg: SandboxConfig) -> BashTool {
    BashTool::new(Arc::new(ToolLimits {
        bash_timeout: Duration::from_secs(60),
        sandbox: cfg,
        ..ToolLimits::default()
    }))
}

async fn run(tool: &BashTool, ws: &Path, cmd: &str) -> Result<ToolResult, ToolError> {
    run_with_timeout(tool, ws, cmd, None).await
}

async fn run_with_timeout(
    tool: &BashTool,
    ws: &Path,
    cmd: &str,
    timeout_secs: Option<u64>,
) -> Result<ToolResult, ToolError> {
    let mut input = json!({ "command": cmd });
    if let Some(t) = timeout_secs {
        input["timeout_secs"] = json!(t);
    }
    tool.execute(
        input,
        ToolContext::new_minimal(ws.to_path_buf(), SessionId::new()),
    )
    .await
}

fn text(r: &ToolResult) -> String {
    r.content
        .iter()
        .map(|c| {
            let crom_core::ToolContent::Text(t) = c;
            t.as_str()
        })
        .collect()
}

// A `required` call on a host that cannot enforce the sandbox is refused and spawns nothing.
#[tokio::test]
async fn required_mode_refuses_when_backend_cannot_enforce() {
    let ws = TempDir::new().unwrap();
    let cfg = SandboxConfig {
        // No kernel offers this ABI, so the probe fails on every platform.
        min_landlock_abi: u32::MAX,
        ..SandboxConfig::required()
    };
    let err = run(&tool(cfg), ws.path(), "touch marker")
        .await
        .expect_err("required sandbox must refuse");
    assert!(
        matches!(err, ToolError::PermissionDenied(ref m) if m.contains("sandbox")),
        "{err:?}"
    );
    assert!(!ws.path().join("marker").exists(), "command must not run");
}

// On platforms without a backend, the default `required` configuration is refused with a clear message.
#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn required_mode_refuses_on_unsupported_platform() {
    let ws = TempDir::new().unwrap();
    let err = run(
        &tool(SandboxConfig::required()),
        ws.path(),
        "touch marker",
    )
    .await
    .expect_err("required sandbox must refuse");
    assert!(err.to_string().contains("sandbox"), "{err}");
    assert!(!ws.path().join("marker").exists());
}

// `off` runs the command unsandboxed and records that in the result metadata.
#[tokio::test]
async fn off_mode_runs_and_records_unsandboxed() {
    let ws = TempDir::new().unwrap();
    let r = run(&tool(SandboxConfig::default()), ws.path(), "touch marker")
        .await
        .unwrap();
    assert!(ws.path().join("marker").exists());
    assert_eq!(r.metadata["sandbox"]["mode"], "off");
    assert_eq!(r.metadata["sandbox"]["enforced"], false);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// True when the sandbox is enforceable here; otherwise prints an explicit
    /// skip line (or panics when CI requires the sandbox tests to run).
    fn sandbox_available() -> bool {
        match crom_tools::sandbox::probe(&SandboxConfig::required()) {
            Ok(_) => true,
            Err(reason) => {
                if std::env::var_os("CROM_REQUIRE_SANDBOX_TESTS").is_some() {
                    panic!("sandbox required by CROM_REQUIRE_SANDBOX_TESTS but unavailable: {reason}");
                }
                eprintln!("SKIP: OS sandbox unavailable on this host: {reason}");
                false
            }
        }
    }

    fn has(cmd: &str) -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {cmd}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn unique_marker() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("9{:04}{:05}", std::process::id() % 10_000, nanos % 100_000)
    }

    /// Pids of live (non-zombie) processes whose command line contains `needle`.
    fn processes_matching(needle: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if pid == std::process::id() {
                continue;
            }
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let stat = std::fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            let zombie = stat
                .rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
                .unwrap_or(false);
            if !zombie && String::from_utf8_lossy(&cmdline).contains(needle) {
                out.push(pid);
            }
        }
        out
    }

    /// Wait up to `limit` for every process matching `needle` to disappear.
    async fn wait_gone(needle: &str, limit: Duration) -> Vec<u32> {
        let start = std::time::Instant::now();
        loop {
            let left = processes_matching(needle);
            if left.is_empty() || start.elapsed() > limit {
                return left;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn required() -> BashTool {
        tool(SandboxConfig::required())
    }

    /// Ceiling on the outer cgroup `pids.max` a containment test accepts.
    const OUTER_TASKS_CEILING: u64 = 4096;
    /// Ceiling on the outer cgroup `memory.max` a containment test accepts.
    const OUTER_MEMORY_CEILING: u64 = 16 << 30;

    /// Tightest `(pids.max, memory.max)` over this process's cgroup v2 path and
    /// its ancestors; `None` for a limit that is `max` all the way up.
    fn outer_cgroup_limits() -> (Option<u64>, Option<u64>) {
        let rel = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("0::").map(|p| p.trim().to_string()))
            })
            .unwrap_or_default();
        let read = |dir: &Path, file: &str| -> Option<u64> {
            std::fs::read_to_string(dir.join(file))
                .ok()
                .and_then(|v| v.trim().parse().ok())
        };
        let (mut pids, mut mem) = (None::<u64>, None::<u64>);
        let mut dir = Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
        loop {
            if let Some(v) = read(&dir, "pids.max") {
                pids = Some(pids.map_or(v, |p| p.min(v)));
            }
            if let Some(v) = read(&dir, "memory.max") {
                mem = Some(mem.map_or(v, |m| m.min(v)));
            }
            if dir == Path::new("/sys/fs/cgroup") || !dir.pop() {
                return (pids, mem);
            }
        }
    }

    /// Guard for host-destructive tests: refuse (panic, spawning nothing)
    /// unless an outer cgroup bounds this process independently of the code
    /// under test, then require the sandbox itself.
    fn containment_preconditions() -> bool {
        let (pids, mem) = outer_cgroup_limits();
        let bounded = pids.is_some_and(|p| p <= OUTER_TASKS_CEILING)
            && mem.is_some_and(|m| m <= OUTER_MEMORY_CEILING);
        assert!(
            bounded,
            "refusing to run a containment test without an outer cgroup limit \
             (pids.max={pids:?}, memory.max={mem:?}); run scripts/sandbox-tests.sh"
        );
        sandbox_available()
    }

    /// Fail before any bomb is started unless a probe through the same Bash
    /// path shows seccomp, no_new_privs, and finite NPROC/DATA rlimits.
    async fn assert_sandbox_engaged(tool: &BashTool, ws: &Path) {
        let probe = "grep -E '^(NoNewPrivs|Seccomp):' /proc/self/status; \
                     grep -E '^Max (processes|data size)' /proc/self/limits";
        let r = run(tool, ws, probe).await.expect("engagement probe");
        let out = text(&r);
        let field = |name: &str| {
            out.lines()
                .find(|l| l.starts_with(name))
                .map(|l| l.split_whitespace().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(field("NoNewPrivs:").get(1), Some(&"1"), "no_new_privs off: {out}");
        assert_eq!(field("Seccomp:").get(1), Some(&"2"), "seccomp filter off: {out}");
        let host_limits = std::fs::read_to_string("/proc/self/limits").unwrap();
        for limit in ["Max processes", "Max data size"] {
            let soft_of = |text: &str| -> Option<u64> {
                let words: Vec<&str> = text
                    .lines()
                    .find(|l| l.starts_with(limit))?
                    .split_whitespace()
                    .collect();
                words.get(limit.split(' ').count())?.parse().ok()
            };
            // An inherited host limit can be finite but huge (NPROC ~2M), so
            // the child's limit must be strictly tighter than this process's.
            let child = soft_of(&out);
            let host = soft_of(&host_limits);
            assert!(
                child.is_some_and(|c| host.is_none_or(|h| c < h)),
                "{limit} rlimit not tightened in child (child={child:?}, host={host:?}): {out}"
            );
        }
    }

    // P1: writes, truncation, renames, and deletes outside the workspace fail and leave targets unchanged.
    #[tokio::test]
    async fn write_outside_workspace_fails() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        let o = outside.path().display();
        let cmd = format!(
            "echo pwned > {o}/new.txt; echo pwned > {o}/victim.txt; : > {o}/victim.txt; \
             truncate -s 0 {o}/victim.txt; mv {o}/victim.txt {o}/moved.txt; rm -f {o}/victim.txt; \
             mkdir {o}/dir; echo done"
        );
        let r = run(&required(), ws.path(), &cmd).await.unwrap();
        assert!(text(&r).contains("done"), "{}", text(&r));
        assert!(!outside.path().join("new.txt").exists());
        assert!(!outside.path().join("moved.txt").exists());
        assert!(!outside.path().join("dir").exists());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
    }

    // P2: a synthetic ~/.ssh secret and a sibling directory listing outside the workspace are unreadable.
    #[tokio::test]
    async fn read_outside_workspace_fails() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        std::fs::create_dir(home.path().join(".ssh")).unwrap();
        std::fs::write(home.path().join(".ssh/id_fake"), "SYNTHETIC-SECRET").unwrap();
        let h = home.path().display();
        let cmd = format!("cat {h}/.ssh/id_fake; ls {h}/.ssh; echo done");
        let r = run(&required(), ws.path(), &cmd).await.unwrap();
        let out = text(&r);
        assert!(out.contains("done"), "{out}");
        assert!(!out.contains("SYNTHETIC-SECRET"), "{out}");
        assert!(!out.contains("id_fake\n"), "directory listing leaked: {out}");
    }

    // P2/P4: the host process's /proc environ (which holds provider keys) is unreadable.
    #[tokio::test]
    async fn host_process_environ_is_unreadable() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let cmd = format!(
            "cat /proc/{}/environ >/dev/null && echo LEAKED || echo blocked",
            std::process::id()
        );
        let r = run(&required(), ws.path(), &cmd).await.unwrap();
        assert!(text(&r).contains("blocked"), "{}", text(&r));
    }

    // The workspace and a private TMPDIR are writable, system tools and the toolchain run.
    #[tokio::test]
    async fn workspace_tmp_and_toolchain_are_usable() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let cmd = "set -e; mkdir -p sub; echo hi > sub/f.txt; mv sub/f.txt g.txt; rm g.txt; \
                   echo t > \"$TMPDIR/x\"; cat \"$TMPDIR/x\"; ls /usr/bin >/dev/null; echo ok";
        let r = run(&required(), ws.path(), cmd).await.unwrap();
        let out = text(&r);
        assert!(out.contains("ok") && out.contains("[exit: 0]"), "{out}");
        assert_eq!(r.metadata["sandbox"]["mode"], "required");
        assert_eq!(r.metadata["sandbox"]["enforced"], true);
        assert_eq!(r.metadata["sandbox"]["backend"], "landlock");
        assert!(r.metadata["sandbox"]["landlock_abi"].as_u64().unwrap() >= 3);
        assert!(r.metadata["sandbox"]["setup_ms"].is_number());
    }

    // A zero-dependency crate builds offline inside the sandbox with the host toolchain.
    #[tokio::test]
    async fn cargo_builds_offline_inside_sandbox() {
        if !sandbox_available() {
            return;
        }
        if !has("cargo") {
            eprintln!("SKIP: cargo not on PATH");
            return;
        }
        let ws = TempDir::new().unwrap();
        std::fs::write(
            ws.path().join("Cargo.toml"),
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::create_dir(ws.path().join("src")).unwrap();
        std::fs::write(
            ws.path().join("src/lib.rs"),
            "#[test]\nfn t() { assert_eq!(1 + 1, 2); }\n",
        )
        .unwrap();
        let r = run(&required(), ws.path(), "cargo test --offline -q 2>&1")
            .await
            .unwrap();
        let out = text(&r);
        assert!(out.contains("[exit: 0]") && out.contains("test result: ok"), "{out}");
    }

    // P3: TCP to a host loopback server, UDP connect, and unix-socket connect all fail by default.
    #[tokio::test]
    async fn network_is_denied_by_default() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let sock_path = ws.path().join("host.sock");
        let _unix = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        let mut cmd = format!(
            "(exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo TCP-OPEN || echo tcp-blocked; \
             (exec 3<>/dev/udp/192.0.2.1/53) 2>/dev/null && echo UDP-OPEN || echo udp-blocked; "
        );
        if has("curl") {
            cmd.push_str(&format!(
                "curl -s -m 3 http://127.0.0.1:{port}/ >/dev/null 2>&1 && echo CURL-OPEN || echo curl-blocked; "
            ));
        }
        if has("python3") {
            cmd.push_str(
                "python3 -c 'import socket; s=socket.socket(socket.AF_UNIX); s.connect(\"host.sock\")' \
                 2>/dev/null && echo UNIX-OPEN || echo unix-blocked; ",
            );
        }
        let r = run(&required(), ws.path(), &cmd).await.unwrap();
        let out = text(&r);
        assert!(out.contains("tcp-blocked"), "{out}");
        assert!(out.contains("udp-blocked"), "{out}");
        assert!(!out.contains("-OPEN"), "{out}");
        assert!(listener.accept().is_err(), "host server saw a connection");
    }

    // P3: when policy grants network, TCP to a host loopback server succeeds (all-or-nothing).
    #[tokio::test]
    async fn network_grant_allows_tcp() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = SandboxConfig {
            network: true,
            ..SandboxConfig::required()
        };
        let cmd = format!(
            "(exec 3<>/dev/tcp/127.0.0.1/{port}) && echo tcp-open || echo TCP-BLOCKED"
        );
        let r = run(&tool(cfg), ws.path(), &cmd).await.unwrap();
        assert!(text(&r).contains("tcp-open"), "{}", text(&r));
    }

    // P5: a setsid'd background process is dead once the call returns normally.
    #[tokio::test]
    async fn setsid_descendant_killed_after_return() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let m = unique_marker();
        let cmd = format!("setsid sleep {m} </dev/null >/dev/null 2>&1 & echo started");
        let r = run(&required(), ws.path(), &cmd).await.unwrap();
        assert!(text(&r).contains("started"));
        let left = wait_gone(&format!("sleep\0{m}"), Duration::from_secs(2)).await;
        assert!(left.is_empty(), "setsid child survived: {left:?}");
    }

    // P5: a setsid'd background process is dead after the call times out.
    #[tokio::test]
    async fn setsid_descendant_killed_after_timeout() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let m = unique_marker();
        let cmd = format!("setsid sleep {m} </dev/null >/dev/null 2>&1 & sleep 30");
        let r = run_with_timeout(&required(), ws.path(), &cmd, Some(1))
            .await
            .unwrap();
        assert_eq!(r.metadata["timed_out"], true);
        let left = wait_gone(&format!("sleep\0{m}"), Duration::from_secs(2)).await;
        assert!(left.is_empty(), "setsid child survived: {left:?}");
    }

    // P5: a setsid'd background process is dead after the tool future is dropped (cancel).
    #[tokio::test]
    async fn setsid_descendant_killed_when_call_is_cancelled() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let m = unique_marker();
        let cmd = format!("setsid sleep {m} </dev/null >/dev/null 2>&1 & sleep 30");
        let t = required();
        let fut = run(&t, ws.path(), &cmd);
        let res = tokio::time::timeout(Duration::from_millis(700), fut).await;
        assert!(res.is_err(), "call should still be running when cancelled");
        let left = wait_gone(&format!("sleep\0{m}"), Duration::from_secs(3)).await;
        assert!(left.is_empty(), "setsid child survived cancel: {left:?}");
    }

    // P6: the number of processes a call can create is bounded by max_processes.
    #[tokio::test]
    #[ignore = "host-destructive if the sandbox is broken; run via scripts/sandbox-tests.sh"]
    async fn process_count_is_bounded() {
        if !containment_preconditions() {
            return;
        }
        if !has("python3") {
            eprintln!("SKIP: python3 not on PATH");
            return;
        }
        let ws = TempDir::new().unwrap();
        let cfg = SandboxConfig {
            max_processes: 40,
            ..SandboxConfig::required()
        };
        let script = "import os,time\nn=0\nfor i in range(400):\n    try:\n        pid=os.fork()\n    except OSError:\n        break\n    if pid==0:\n        time.sleep(3); os._exit(0)\n    n+=1\nprint('forks=%d' % n)\n";
        std::fs::write(ws.path().join("fork.py"), script).unwrap();
        let t = tool(cfg);
        assert_sandbox_engaged(&t, ws.path()).await;
        let r = run(&t, ws.path(), "python3 fork.py").await.unwrap();
        let out = text(&r);
        let n: u64 = out
            .split("forks=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no fork count: {out}"));
        assert!(n < 400, "fork loop was not bounded: {out}");
        assert!(n <= 45, "bound exceeded max_processes: {out}");
    }

    // P6: a shell fork bomb is contained, the call ends, and no bomb process survives.
    #[tokio::test]
    #[ignore = "host-destructive if the sandbox is broken; run via scripts/sandbox-tests.sh"]
    async fn fork_bomb_is_contained() {
        if !containment_preconditions() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let m = unique_marker();
        let cfg = SandboxConfig {
            max_processes: 64,
            ..SandboxConfig::required()
        };
        let cmd = format!("# {m}\nbomb() {{ bomb | bomb & }}; bomb; sleep 2; echo survived");
        let t = tool(cfg);
        assert_sandbox_engaged(&t, ws.path()).await;
        let started = std::time::Instant::now();
        let _ = run_with_timeout(&t, ws.path(), &cmd, Some(4)).await;
        assert!(started.elapsed() < Duration::from_secs(20));
        let left = wait_gone(&m, Duration::from_secs(5)).await;
        assert!(left.is_empty(), "fork bomb processes survived: {}", left.len());
    }

    // P6: an allocation past max_memory_bytes fails.
    #[tokio::test]
    #[ignore = "host-destructive if the sandbox is broken; run via scripts/sandbox-tests.sh"]
    async fn memory_limit_is_enforced() {
        if !containment_preconditions() {
            return;
        }
        if !has("python3") {
            eprintln!("SKIP: python3 not on PATH");
            return;
        }
        let ws = TempDir::new().unwrap();
        let cfg = SandboxConfig {
            max_memory_bytes: 256 << 20,
            ..SandboxConfig::required()
        };
        let cmd = "python3 -c 'b = bytearray(1024 * 1024 * 1024); print(\"ALLOC\" + \"ATED\")'";
        let t = tool(cfg);
        assert_sandbox_engaged(&t, ws.path()).await;
        let r = run(&t, ws.path(), cmd).await.unwrap();
        let out = text(&r);
        assert!(!out.contains("ALLOCATED"), "{out}");
        assert!(!out.contains("[exit: 0]"), "{out}");
    }

    // P6: writing a file past max_file_size_bytes fails and the file stays within the limit.
    #[tokio::test]
    async fn file_size_limit_is_enforced() {
        if !sandbox_available() {
            return;
        }
        let ws = TempDir::new().unwrap();
        let cfg = SandboxConfig {
            max_file_size_bytes: 1 << 20,
            ..SandboxConfig::required()
        };
        let r = run(
            &tool(cfg),
            ws.path(),
            "head -c 4194304 /dev/zero > big.bin",
        )
        .await
        .unwrap();
        assert!(!text(&r).contains("[exit: 0]"), "{}", text(&r));
        let len = std::fs::metadata(ws.path().join("big.bin")).unwrap().len();
        assert!(len <= 1 << 20, "file grew to {len}");
    }

    // With landlock ABI >= 6, a tool process cannot signal the host process.
    #[tokio::test]
    async fn cannot_signal_host_process_with_scoped_abi() {
        if !sandbox_available() {
            return;
        }
        let info = crom_tools::sandbox::probe(&SandboxConfig::required()).unwrap();
        if info.landlock_abi < 6 {
            eprintln!("SKIP: landlock ABI {} has no signal scoping", info.landlock_abi);
            return;
        }
        let ws = TempDir::new().unwrap();
        let cmd = format!(
            "kill -0 {} 2>/dev/null && echo SIGNALLED || echo blocked",
            std::process::id()
        );
        let r = run(&required(), ws.path(), &cmd).await.unwrap();
        assert!(text(&r).contains("blocked"), "{}", text(&r));
    }

    // The sandbox refuses a workspace of `/`, which would confine nothing.
    #[tokio::test]
    async fn refuses_root_workspace() {
        if !sandbox_available() {
            return;
        }
        let err = run(&required(), Path::new("/"), "true").await.unwrap_err();
        assert!(err.to_string().contains("workspace"), "{err}");
    }

    // The mode is reported as required in metadata only when SandboxMode::Required is configured.
    #[test]
    fn required_constructor_sets_mode() {
        assert_eq!(SandboxConfig::required().mode, SandboxMode::Required);
    }
}
