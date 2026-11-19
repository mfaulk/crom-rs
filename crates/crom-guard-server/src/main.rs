//! `crom-guard` service binary. The agent endpoint and admin API arrive in
//! #357; until then it refuses to start rather than run without enforcement.

fn main() {
    eprintln!("crom-guard: no agent endpoint is implemented yet (see #357)");
    std::process::exit(2);
}
