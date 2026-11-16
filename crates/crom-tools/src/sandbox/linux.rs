//! Linux backend: landlock + seccomp + rlimits + subreaper supervisor
//! (ADR 0001 §5).
//!
//! Everything that allocates (ruleset construction, BPF program, env, rlimit
//! values) happens in the parent. The `pre_exec` closure runs in the forked
//! child of a multi-threaded process, so it only issues raw syscalls:
//!
//! - the forked child becomes the supervisor `S`: it marks itself a child
//!   subreaper and clones the tool process `B` with a raw `clone` (no atfork
//!   handlers), then never returns from the closure. `S` closes every fd so
//!   `spawn()` sees the exec-error pipe close once `B` execs, waits for `B`,
//!   and on `B`'s exit, on SIGTERM (timeout/cancel/drop), or when the host
//!   process dies, SIGKILLs every descendant (they reparent to `S`) until it
//!   has no children left, then exits with `B`'s status.
//! - `B` calls `setsid`, lowers rlimits, sets `NO_NEW_PRIVS`, enters the
//!   landlock domain, installs the seccomp filter, and returns to std, which
//!   execs the command.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crom_core::SandboxConfig;

use super::SandboxInfo;

const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_uint = 1;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
const FS_READ: u64 = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
const FS_FILE_ONLY: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: libc::c_ushort,
    filter: *const SockFilter,
}

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
#[cfg(target_arch = "x86_64")]
const BPF_JMP_JGE_K: u16 = 0x35;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_DATA_NR: u32 = 0;
const SECCOMP_DATA_ARCH: u32 = 4;
const SECCOMP_DATA_ARG0_LO: u32 = 16;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000_00B7;

const PROC_CHILDREN: &[u8] = b"/proc/thread-self/children\0";

/// Kernel landlock ABI version, or 0 when landlock is unavailable.
pub(super) fn landlock_abi() -> u32 {
    // SAFETY: landlock_create_ruleset(NULL, 0, VERSION) only returns the ABI.
    let r = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if r < 0 {
        0
    } else {
        r as u32
    }
}

pub(super) fn probe(cfg: &SandboxConfig) -> Result<SandboxInfo, String> {
    if !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        return Err(format!(
            "no seccomp filter for architecture {}",
            std::env::consts::ARCH
        ));
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return Err(
            "running as root: capabilities and RLIMIT_NPROC cannot be bounded; run as an unprivileged user"
                .into(),
        );
    }
    let abi = landlock_abi();
    if abi == 0 {
        return Err(
            "landlock is unavailable (kernel without CONFIG_SECURITY_LANDLOCK or landlock not in the active LSM list)"
                .into(),
        );
    }
    if abi < cfg.min_landlock_abi {
        return Err(format!(
            "landlock ABI {abi} is below the required minimum {}",
            cfg.min_landlock_abi
        ));
    }
    // SAFETY: PR_GET_SECCOMP takes no pointer arguments.
    if unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) } < 0 {
        return Err("seccomp is unavailable".into());
    }
    if std::fs::metadata("/proc/thread-self/children").is_err() {
        return Err(
            "/proc/thread-self/children is unavailable (CONFIG_PROC_CHILDREN); process-tree cleanup cannot be enforced"
                .into(),
        );
    }
    Ok(SandboxInfo {
        backend: "landlock",
        landlock_abi: abi,
    })
}

fn fs_all(abi: u32) -> u64 {
    let mut m = (1u64 << 13) - 1;
    if abi >= 2 {
        m |= FS_REFER;
    }
    if abi >= 3 {
        m |= FS_TRUNCATE;
    }
    if abi >= 5 {
        m |= FS_IOCTL_DEV;
    }
    m
}

fn last_err(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

/// Build a landlock ruleset fd in the parent. Missing paths are skipped.
fn build_ruleset(
    abi: u32,
    network: bool,
    rw: &[PathBuf],
    ro: &[PathBuf],
) -> Result<OwnedFd, String> {
    let handled_fs = fs_all(abi);
    let attr = RulesetAttr {
        handled_access_fs: handled_fs,
        handled_access_net: if abi >= 4 && !network {
            NET_BIND_TCP | NET_CONNECT_TCP
        } else {
            0
        },
        scoped: if abi >= 6 {
            SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
        } else {
            0
        },
    };
    // Older kernels accept the larger struct as long as the unknown tail is zero.
    // SAFETY: attr is a valid, initialized landlock_ruleset_attr.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(last_err("landlock_create_ruleset"));
    }
    // SAFETY: fd is a fresh, owned file descriptor.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    for (paths, access) in [(rw, handled_fs), (ro, FS_READ)] {
        for p in paths {
            add_path_rule(&ruleset, p, access & handled_fs)?;
        }
    }
    Ok(ruleset)
}

fn add_path_rule(ruleset: &OwnedFd, path: &Path, access: u64) -> Result<(), String> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path contains NUL: {}", path.display()))?;
    // SAFETY: c is a valid NUL-terminated path.
    let pfd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if pfd < 0 {
        let e = std::io::Error::last_os_error();
        if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) {
            return Ok(());
        }
        return Err(format!("open {}: {e}", path.display()));
    }
    // SAFETY: pfd is a fresh, owned file descriptor.
    let pfd = unsafe { OwnedFd::from_raw_fd(pfd) };
    // SAFETY: zeroed stat is a valid out-parameter for fstat.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: pfd is open and st is a valid out-pointer.
    if unsafe { libc::fstat(pfd.as_raw_fd(), &mut st) } != 0 {
        return Err(last_err(&format!("fstat {}", path.display())));
    }
    let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
    let allowed = if is_dir { access } else { access & FS_FILE_ONLY };
    let rule = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd: pfd.as_raw_fd(),
    };
    // SAFETY: rule is a valid landlock_path_beneath_attr for an open ruleset.
    let r = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &rule as *const PathBeneathAttr,
            0u32,
        )
    };
    if r < 0 {
        return Err(last_err(&format!("landlock_add_rule {}", path.display())));
    }
    Ok(())
}

fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Seccomp program: kill foreign-arch syscalls, refuse x32 and io_uring
/// (which could create sockets without `socket(2)`), and refuse `socket(2)`
/// for every family (network off) or for `AF_UNIX` (network on), so neither
/// the host's loopback services nor its unix sockets (D-Bus, docker, agents)
/// are reachable. `socketpair(2)` stays allowed.
fn seccomp_program(network: bool) -> Vec<SockFilter> {
    let eacces = SECCOMP_RET_ERRNO | libc::EACCES as u32;
    let enosys = SECCOMP_RET_ERRNO | libc::ENOSYS as u32;
    let mut p = vec![
        stmt(BPF_LD_W_ABS, SECCOMP_DATA_ARCH),
        jump(BPF_JMP_JEQ_K, AUDIT_ARCH_NATIVE, 1, 0),
        stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
        stmt(BPF_LD_W_ABS, SECCOMP_DATA_NR),
    ];
    #[cfg(target_arch = "x86_64")]
    {
        p.push(jump(BPF_JMP_JGE_K, 0x4000_0000, 0, 1));
        p.push(stmt(BPF_RET_K, enosys));
    }
    p.push(jump(
        BPF_JMP_JEQ_K,
        libc::SYS_io_uring_setup as u32,
        0,
        1,
    ));
    p.push(stmt(BPF_RET_K, enosys));
    if network {
        p.push(jump(BPF_JMP_JEQ_K, libc::SYS_socket as u32, 0, 3));
        p.push(stmt(BPF_LD_W_ABS, SECCOMP_DATA_ARG0_LO));
        p.push(jump(BPF_JMP_JEQ_K, libc::AF_UNIX as u32, 0, 1));
        p.push(stmt(BPF_RET_K, eacces));
    } else {
        p.push(jump(BPF_JMP_JEQ_K, libc::SYS_socket as u32, 0, 1));
        p.push(stmt(BPF_RET_K, eacces));
    }
    p.push(stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
    p
}

/// Tasks (threads count) currently owned by the real UID, for the
/// `RLIMIT_NPROC` budget.
fn uid_task_count(uid: libc::uid_t) -> u64 {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut total = 0;
    for e in dir.flatten() {
        let name = e.file_name();
        if !name.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(e.path().join("status")) else {
            continue;
        };
        let mut real_uid = None;
        let mut threads = 1;
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("Uid:") {
                real_uid = v.split_whitespace().next().and_then(|s| s.parse().ok());
            } else if let Some(v) = line.strip_prefix("Threads:") {
                threads = v.trim().parse().unwrap_or(1);
            }
        }
        if real_uid == Some(uid) {
            total += threads;
        }
    }
    total
}

#[cfg(target_env = "gnu")]
type Resource = libc::__rlimit_resource_t;
#[cfg(not(target_env = "gnu"))]
type Resource = libc::c_int;

#[derive(Clone, Copy)]
struct Rlimit {
    resource: Resource,
    value: libc::rlim_t,
}

/// Clamp a desired limit to the current hard limit so `setrlimit` never
/// needs to raise it.
fn clamp_rlimit(resource: Resource, want: u64) -> Rlimit {
    // SAFETY: zeroed rlimit is a valid out-parameter.
    let mut cur: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: cur is a valid out-pointer.
    let ok = unsafe { libc::getrlimit(resource, &mut cur) } == 0;
    let value = if ok && cur.rlim_max != libc::RLIM_INFINITY {
        want.min(cur.rlim_max)
    } else {
        want
    };
    Rlimit { resource, value }
}

/// Directories holding the `cargo`/`rustc`/`rustup` the host PATH resolves
/// to (rustup proxies may live outside `$CARGO_HOME/bin`).
fn toolchain_bin_dirs() -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in ["cargo", "rustc", "rustup"] {
        if let Some(bin) = std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
        {
            out.extend(bin.parent().map(Path::to_path_buf));
            if let Some(dir) = bin.canonicalize().ok().and_then(|c| c.parent().map(Path::to_path_buf)) {
                out.push(dir);
            }
        }
    }
    out
}

fn env_path(var: &str, home: Option<&Path>, default_rel: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(default_rel)))
        .filter(|p| p.exists())
}

/// Environment names passed through from the host when set.
const ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "TERM",
    "TZ",
    "USER",
    "LOGNAME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_TERM_COLOR",
    "NO_COLOR",
    "CLICOLOR",
    "RUST_BACKTRACE",
    "SOURCE_DATE_EPOCH",
];

/// Parent-side state that must outlive `spawn()`.
pub(crate) struct Prepared {
    pub(crate) info: SandboxInfo,
    _ruleset: OwnedFd,
    _tmp: tempfile::TempDir,
}

pub(crate) fn prepare(
    cfg: &SandboxConfig,
    workspace: &Path,
    cmd: &mut tokio::process::Command,
) -> Result<Prepared, String> {
    let info = probe(cfg)?;
    let ws = workspace
        .canonicalize()
        .map_err(|e| format!("workspace {}: {e}", workspace.display()))?;
    if ws.parent().is_none() {
        return Err("workspace `/` cannot be sandboxed".into());
    }
    let host_home = std::env::var_os("HOME").map(PathBuf::from);
    if host_home.as_deref().and_then(|h| h.canonicalize().ok()).as_deref() == Some(ws.as_path()) {
        return Err(format!(
            "workspace {} is the home directory; refusing to expose it read-write",
            ws.display()
        ));
    }
    let tmp = tempfile::Builder::new()
        .prefix("crom-sbx-")
        .tempdir()
        .map_err(|e| format!("private tmp dir: {e}"))?;
    let tmp_path = tmp
        .path()
        .canonicalize()
        .map_err(|e| format!("private tmp dir: {e}"))?;

    let cargo_home = env_path("CARGO_HOME", host_home.as_deref(), ".cargo");
    let rustup_home = env_path("RUSTUP_HOME", host_home.as_deref(), ".rustup");
    let mut ro: Vec<PathBuf> = [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/libx32",
        "/etc",
        "/proc",
        "/sys/devices/system/cpu",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/nix/store",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(ch) = &cargo_home {
        for sub in ["bin", "registry", "git", "config.toml", "config"] {
            ro.push(ch.join(sub));
        }
    }
    if let Some(rh) = &rustup_home {
        ro.push(rh.clone());
    }
    ro.extend(toolchain_bin_dirs());
    ro.extend(cfg.read_paths.iter().cloned());
    let rw = vec![ws.clone(), tmp_path.clone(), PathBuf::from("/dev/null")];
    let ruleset = build_ruleset(info.landlock_abi, cfg.network, &rw, &ro)?;
    let ruleset_fd = ruleset.as_raw_fd();
    let filter = seccomp_program(cfg.network);

    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let nproc = uid_task_count(uid).saturating_add(cfg.max_processes);
    let rlimits = [
        clamp_rlimit(libc::RLIMIT_NPROC, nproc),
        clamp_rlimit(libc::RLIMIT_DATA, cfg.max_memory_bytes),
        clamp_rlimit(libc::RLIMIT_FSIZE, cfg.max_file_size_bytes),
        clamp_rlimit(libc::RLIMIT_CPU, cfg.max_cpu_seconds),
        clamp_rlimit(libc::RLIMIT_CORE, 0),
    ];

    cmd.env_clear();
    for name in ENV_PASSTHROUGH
        .iter()
        .copied()
        .chain(cfg.env_allow.iter().map(String::as_str))
    {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    for name in ["HOME", "TMPDIR", "TMP", "TEMP"] {
        cmd.env(name, &tmp_path);
    }
    if let Some(ch) = &cargo_home {
        cmd.env("CARGO_HOME", ch);
    }
    if let Some(rh) = &rustup_home {
        cmd.env("RUSTUP_HOME", rh);
    }

    // SAFETY: the closure only performs async-signal-safe raw syscalls on
    // data prepared above (no allocation, no locks); see the module docs.
    unsafe {
        cmd.pre_exec(move || child_entry(ruleset_fd, &filter, &rlimits));
    }
    Ok(Prepared {
        info,
        _ruleset: ruleset,
        _tmp: tmp,
    })
}

static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_terminate(_: libc::c_int) {
    TERMINATE.store(true, Ordering::SeqCst);
}

extern "C" fn on_wakeup(_: libc::c_int) {}

unsafe fn set_handler(sig: libc::c_int, handler: libc::sighandler_t) {
    let mut sa: libc::sigaction = std::mem::zeroed();
    sa.sa_sigaction = handler;
    libc::sigemptyset(&mut sa.sa_mask);
    // No SA_RESTART: waitpid must return EINTR so the loop re-checks state.
    sa.sa_flags = 0;
    libc::sigaction(sig, &sa, std::ptr::null_mut());
}

/// Runs in the forked child. Returns only in the tool process `B`; an `Err`
/// there travels through std's exec-error pipe so `spawn()` fails closed.
fn child_entry(
    ruleset_fd: RawFd,
    filter: &[SockFilter],
    rlimits: &[Rlimit],
) -> std::io::Result<()> {
    // SAFETY (whole function): post-fork child; raw syscalls only.
    unsafe {
        let host = libc::getppid();
        let mut block: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut block);
        for s in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            libc::sigaddset(&mut block, s);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &block, std::ptr::null_mut());
        if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let supervisor = libc::getpid();
        let pid = libc::syscall(libc::SYS_clone, libc::SIGCHLD as libc::c_long, 0, 0, 0, 0);
        if pid < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid == 0 {
            return enter_sandbox(supervisor, ruleset_fd, filter, rlimits)
                .map_err(std::io::Error::from_raw_os_error);
        }
        supervise(pid as libc::pid_t, host, &block)
    }
}

fn errno() -> i32 {
    // SAFETY: __errno_location always returns a valid thread-local pointer.
    unsafe { *libc::__errno_location() }
}

unsafe fn enter_sandbox(
    supervisor: libc::pid_t,
    ruleset_fd: RawFd,
    filter: &[SockFilter],
    rlimits: &[Rlimit],
) -> Result<(), i32> {
    let mut empty: libc::sigset_t = std::mem::zeroed();
    libc::sigemptyset(&mut empty);
    libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
        return Err(errno());
    }
    if libc::getppid() != supervisor {
        return Err(libc::ESRCH);
    }
    if libc::setsid() < 0 {
        return Err(errno());
    }
    for r in rlimits {
        let lim = libc::rlimit {
            rlim_cur: r.value,
            rlim_max: r.value,
        };
        if libc::setrlimit(r.resource, &lim) != 0 {
            return Err(errno());
        }
    }
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        return Err(errno());
    }
    if libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0u32) != 0 {
        return Err(errno());
    }
    let prog = SockFprog {
        len: filter.len() as libc::c_ushort,
        filter: filter.as_ptr(),
    };
    if libc::prctl(
        libc::PR_SET_SECCOMP,
        libc::SECCOMP_MODE_FILTER as libc::c_ulong,
        &prog as *const SockFprog,
        0,
        0,
    ) != 0
    {
        return Err(errno());
    }
    Ok(())
}

unsafe fn supervise(child: libc::pid_t, host: libc::pid_t, block: &libc::sigset_t) -> ! {
    TERMINATE.store(false, Ordering::SeqCst);
    set_handler(libc::SIGTERM, on_terminate as *const () as libc::sighandler_t);
    set_handler(libc::SIGHUP, on_terminate as *const () as libc::sighandler_t);
    set_handler(libc::SIGINT, on_terminate as *const () as libc::sighandler_t);
    set_handler(libc::SIGALRM, on_wakeup as *const () as libc::sighandler_t);
    libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    libc::sigprocmask(libc::SIG_UNBLOCK, block, std::ptr::null_mut());
    // Drop every inherited fd, including std's exec-error pipe, so the
    // host's spawn() returns as soon as `B` has exec'd.
    if libc::syscall(libc::SYS_close_range, 0u32, u32::MAX, 0u32) != 0 {
        for fd in 0..4096 {
            libc::close(fd);
        }
    }
    // Periodic wakeups so a dead host is noticed even if nothing exits.
    let tick = libc::timeval {
        tv_sec: 0,
        tv_usec: 200_000,
    };
    let timer = libc::itimerval {
        it_interval: tick,
        it_value: tick,
    };
    libc::setitimer(libc::ITIMER_REAL, &timer, std::ptr::null_mut());

    let mut child_status: Option<libc::c_int> = None;
    loop {
        if TERMINATE.load(Ordering::SeqCst) || libc::getppid() != host {
            break;
        }
        let mut st = 0;
        let r = libc::waitpid(-1, &mut st, 0);
        if r == child {
            child_status = Some(st);
            break;
        }
        if r < 0 && errno() == libc::ECHILD {
            break;
        }
    }
    let off: libc::itimerval = std::mem::zeroed();
    libc::setitimer(libc::ITIMER_REAL, &off, std::ptr::null_mut());
    kill_all_children(child, &mut child_status);

    match child_status {
        Some(st) if libc::WIFEXITED(st) => libc::_exit(libc::WEXITSTATUS(st)),
        Some(st) if libc::WIFSIGNALED(st) => {
            let sig = libc::WTERMSIG(st);
            libc::signal(sig, libc::SIG_DFL);
            let mut only: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut only);
            libc::sigaddset(&mut only, sig);
            libc::sigprocmask(libc::SIG_UNBLOCK, &only, std::ptr::null_mut());
            libc::kill(libc::getpid(), sig);
            libc::_exit(128 + sig)
        }
        _ => libc::_exit(128 + libc::SIGKILL),
    }
}

/// SIGKILL every child (orphaned descendants reparent to us as subreaper)
/// and reap until none remain.
unsafe fn kill_all_children(child: libc::pid_t, child_status: &mut Option<libc::c_int>) {
    loop {
        kill_listed_children();
        let mut st = 0;
        let r = libc::waitpid(-1, &mut st, libc::WNOHANG);
        if r > 0 {
            if r == child && child_status.is_none() {
                *child_status = Some(st);
            }
            continue;
        }
        if r < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return;
        }
        let pause = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        libc::nanosleep(&pause, std::ptr::null_mut());
    }
}

unsafe fn kill_listed_children() {
    let fd = libc::open(
        PROC_CHILDREN.as_ptr() as *const libc::c_char,
        libc::O_RDONLY | libc::O_CLOEXEC,
    );
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 4096];
    let mut cur: libc::pid_t = 0;
    let mut digits = false;
    loop {
        let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
        if n <= 0 {
            break;
        }
        for &b in &buf[..n as usize] {
            if b.is_ascii_digit() {
                cur = cur.wrapping_mul(10).wrapping_add((b - b'0') as libc::pid_t);
                digits = true;
            } else {
                if digits && cur > 0 {
                    libc::kill(cur, libc::SIGKILL);
                }
                cur = 0;
                digits = false;
            }
        }
    }
    if digits && cur > 0 {
        libc::kill(cur, libc::SIGKILL);
    }
    libc::close(fd);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The network-off filter refuses socket(2) and the network-on filter only refuses AF_UNIX sockets.
    #[test]
    fn seccomp_program_shapes() {
        let off = seccomp_program(false);
        let on = seccomp_program(true);
        assert_eq!(off.last().unwrap().k, SECCOMP_RET_ALLOW);
        assert_eq!(on.last().unwrap().k, SECCOMP_RET_ALLOW);
        assert!(on
            .iter()
            .any(|i| i.code == BPF_JMP_JEQ_K && i.k == libc::AF_UNIX as u32));
        assert!(!off.iter().any(|i| i.k == SECCOMP_DATA_ARG0_LO));
        assert!(off.len() < 256 && on.len() < 256);
    }

    // Filesystem access masks grow with the landlock ABI.
    #[test]
    fn fs_masks_follow_abi() {
        assert_eq!(fs_all(1) & FS_REFER, 0);
        assert_ne!(fs_all(3) & FS_TRUNCATE, 0);
        assert_eq!(fs_all(4) & FS_IOCTL_DEV, 0);
        assert_ne!(fs_all(5) & FS_IOCTL_DEV, 0);
    }
}
