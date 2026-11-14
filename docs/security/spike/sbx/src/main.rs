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
