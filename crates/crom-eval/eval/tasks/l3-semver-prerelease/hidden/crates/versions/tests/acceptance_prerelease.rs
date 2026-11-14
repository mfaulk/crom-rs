use versions::{Version, VersionReq};

fn v(s: &str) -> Version {
    s.parse().unwrap()
}

fn m(req: &str, ver: &str) -> bool {
    VersionReq::parse(req).unwrap().matches(&v(ver))
}

#[test]
fn release_outranks_its_prereleases() {
    assert!(v("1.0.0-rc.1") < v("1.0.0"));
    assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
    assert!(v("1.0.0-beta.11") < v("1.0.0-rc.1"));
    assert!(v("0.9.9") < v("1.0.0-alpha"));
    assert!(v("1.0.0") < v("1.0.1-alpha"));
    let mut all = vec![v("1.0.0"), v("1.0.0-rc.1"), v("0.9.0"), v("1.0.0-beta")];
    all.sort();
    let s: Vec<String> = all.iter().map(Version::to_string).collect();
    assert_eq!(s, ["0.9.0", "1.0.0-beta", "1.0.0-rc.1", "1.0.0"]);
}

#[test]
fn plain_requirements_exclude_prereleases() {
    assert!(!m(">=1.0.0", "2.0.0-rc.1"));
    assert!(!m("*", "0.1.0-alpha"));
    assert!(!m("^1.2.0", "1.3.0-beta.1"));
    assert!(m("^1.2.0", "1.3.0"));
}

#[test]
fn prerelease_requirements_match_same_triple_only() {
    assert!(m("^1.3.0-beta.1", "1.3.0-beta.2"));
    assert!(m("^1.3.0-beta.1", "1.3.0"));
    assert!(m("^1.3.0-beta.1", "1.4.0"));
    assert!(!m("^1.3.0-beta.1", "1.4.0-alpha"));
    assert!(!m("^1.3.0-beta.2", "1.3.0-beta.1"));
    assert!(m(">=1.0.0-rc.1, <2.0.0", "1.0.0-rc.2"));
    assert!(m("=2.0.0-rc.1", "2.0.0-rc.1"));
}
