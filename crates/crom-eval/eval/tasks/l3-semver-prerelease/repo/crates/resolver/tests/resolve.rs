use registry::Registry;
use resolver::{resolve, select, ResolveError};
use versions::VersionReq;

fn index() -> Registry {
    let mut r = Registry::new();
    r.publish("log", "0.4.0", &[]);
    r.publish("log", "0.4.17", &[]);
    r.publish("log", "0.5.0", &[]);
    r.publish("http", "1.0.0", &[("log", "^0.4.0")]);
    r.publish("http", "1.1.0", &[("log", "^0.4.10")]);
    r.publish("web", "2.0.0", &[("http", "^1.0.0"), ("log", "~0.4.0")]);
    r
}

#[test]
fn picks_newest_compatible() {
    let r = index();
    let v = select(&r, "log", &[VersionReq::parse("^0.4.0").unwrap()]).unwrap();
    assert_eq!(v.to_string(), "0.4.17");
}

#[test]
fn resolves_transitively() {
    let r = index();
    let out = resolve(&r, &[("web", "2.0.0")]).unwrap();
    assert_eq!(out["web"].to_string(), "2.0.0");
    assert_eq!(out["http"].to_string(), "1.1.0");
    assert_eq!(out["log"].to_string(), "0.4.17");
}

#[test]
fn skips_yanked() {
    let mut r = index();
    r.yank("log", "0.4.17");
    let v = select(&r, "log", &[VersionReq::parse("^0.4.0").unwrap()]).unwrap();
    assert_eq!(v.to_string(), "0.4.0");
}

#[test]
fn reports_unknown_package() {
    let r = index();
    assert_eq!(
        resolve(&r, &[("nope", "*")]).unwrap_err(),
        ResolveError::UnknownPackage("nope".into())
    );
}
