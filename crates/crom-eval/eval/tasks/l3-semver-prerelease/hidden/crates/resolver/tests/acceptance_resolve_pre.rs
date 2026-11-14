use registry::Registry;
use resolver::{resolve, select};
use versions::VersionReq;

fn index() -> Registry {
    let mut r = Registry::new();
    r.publish("tokio", "1.0.0", &[]);
    r.publish("tokio", "2.0.0-rc.1", &[]);
    r.publish("tokio", "2.0.0", &[]);
    r.publish("tokio", "2.1.0-alpha.1", &[]);
    r.publish("app", "0.1.0", &[("tokio", ">=1.0.0")]);
    r
}

#[test]
fn stable_preferred_over_candidates() {
    let r = index();
    let got = select(&r, "tokio", &[VersionReq::parse("*").unwrap()]).unwrap();
    assert_eq!(got.to_string(), "2.0.0");
    let out = resolve(&r, &[("app", "*")]).unwrap();
    assert_eq!(out["tokio"].to_string(), "2.0.0");
}

#[test]
fn explicit_prerelease_opt_in() {
    let r = index();
    let got = select(&r, "tokio", &[VersionReq::parse("^2.1.0-alpha.1").unwrap()]).unwrap();
    assert_eq!(got.to_string(), "2.1.0-alpha.1");
    let got = select(&r, "tokio", &[VersionReq::parse("=2.0.0-rc.1").unwrap()]).unwrap();
    assert_eq!(got.to_string(), "2.0.0-rc.1");
}
