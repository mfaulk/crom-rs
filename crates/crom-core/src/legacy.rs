//! Read-only fallback to pre-rename (`code-agent`) config dirs and
//! `CODE_AGENT_*` env vars. Callers must only use these helpers on read
//! paths: nothing here may cause a write to the legacy location.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const CONFIG_DIR_NAME: &str = "crom";
pub const LEGACY_CONFIG_DIR_NAME: &str = "code-agent";
pub const ENV_PREFIX: &str = "CROM_";
pub const LEGACY_ENV_PREFIX: &str = "CODE_AGENT_";

/// A resolved value plus the deprecation warning to show if it came from a
/// legacy source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    pub value: T,
    pub warning: Option<String>,
}

/// Map `CROM_FOO` to `CODE_AGENT_FOO`; `None` for names without the prefix.
pub fn legacy_env_name(name: &str) -> Option<String> {
    name.strip_prefix(ENV_PREFIX)
        .map(|rest| format!("{LEGACY_ENV_PREFIX}{rest}"))
}

/// Look up `name`, falling back to its legacy `CODE_AGENT_*` spelling only
/// when `name` is unset.
pub fn resolve_env_with(
    name: &str,
    get: impl Fn(&str) -> Option<String>,
) -> Option<Resolved<String>> {
    if let Some(value) = get(name) {
        return Some(Resolved {
            value,
            warning: None,
        });
    }
    let old = legacy_env_name(name)?;
    let value = get(&old)?;
    Some(Resolved {
        value,
        warning: Some(format!(
            "{old} is deprecated and will stop working in a future release; rename it to {name}"
        )),
    })
}

/// [`resolve_env_with`] against the process environment, printing the
/// deprecation warning once per variable.
pub fn env_var(name: &str) -> Option<String> {
    let r = resolve_env_with(name, |k| std::env::var(k).ok())?;
    if let Some(w) = &r.warning {
        warn_once(w);
    }
    Some(r.value)
}

/// Pick `<parent>/crom`, or `<parent>/code-agent` when only the legacy dir
/// exists.
pub fn resolve_config_dir(parent: &Path) -> Resolved<PathBuf> {
    let new = parent.join(CONFIG_DIR_NAME);
    let old = parent.join(LEGACY_CONFIG_DIR_NAME);
    if !new.exists() && old.is_dir() {
        let warning = format!(
            "reading config from legacy directory {} (read-only); move it to {} to silence this warning",
            old.display(),
            new.display()
        );
        return Resolved {
            value: old,
            warning: Some(warning),
        };
    }
    Resolved {
        value: new,
        warning: None,
    }
}

/// [`resolve_config_dir`], printing the deprecation warning once.
pub fn config_dir_for_read(parent: &Path) -> PathBuf {
    let r = resolve_config_dir(parent);
    if let Some(w) = &r.warning {
        warn_once(w);
    }
    r.value
}

/// Map a legacy `CODE_AGENT_*` plugin substitution variable to its `CROM_*`
/// name, warning once; other names pass through unchanged.
pub fn canonical_var_name(var: &str) -> String {
    match var.strip_prefix(LEGACY_ENV_PREFIX) {
        Some(rest @ ("PLUGIN_ROOT" | "PLUGIN_DATA")) => {
            let new = format!("{ENV_PREFIX}{rest}");
            warn_once(&format!(
                "${{{var}}} is deprecated in plugin manifests; use ${{{new}}}"
            ));
            new
        }
        _ => var.to_string(),
    }
}

fn warn_once(msg: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let first = seen
        .lock()
        .map(|mut s| s.insert(msg.to_string()))
        .unwrap_or(true);
    if first {
        eprintln!("warning: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    // Only the new dir present: it is used with no warning.
    fn config_dir_new_only() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("crom")).unwrap();
        let r = resolve_config_dir(t.path());
        assert_eq!(r.value, t.path().join("crom"));
        assert!(r.warning.is_none());
    }

    #[test]
    // Only the legacy dir present: it is read and a move-it warning is produced.
    fn config_dir_old_only_reads_legacy_with_warning() {
        let t = tempfile::tempdir().unwrap();
        let old = t.path().join("code-agent");
        std::fs::create_dir(&old).unwrap();
        let r = resolve_config_dir(t.path());
        assert_eq!(r.value, old);
        let w = r.warning.expect("warning");
        assert!(w.contains("code-agent") && w.contains("crom"), "{w}");
        assert!(!t.path().join("crom").exists(), "must not create new dir");
    }

    #[test]
    // Both dirs present: the new dir wins with no warning.
    fn config_dir_both_new_wins() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("crom")).unwrap();
        std::fs::create_dir(t.path().join("code-agent")).unwrap();
        let r = resolve_config_dir(t.path());
        assert_eq!(r.value, t.path().join("crom"));
        assert!(r.warning.is_none());
    }

    #[test]
    // Neither dir present: the new dir path is returned (for later writes).
    fn config_dir_neither_defaults_to_new() {
        let t = tempfile::tempdir().unwrap();
        let r = resolve_config_dir(t.path());
        assert_eq!(r.value, t.path().join("crom"));
        assert!(r.warning.is_none());
    }

    #[test]
    // CROM_* names map to CODE_AGENT_*; unrelated names have no legacy form.
    fn legacy_env_name_mapping() {
        assert_eq!(
            legacy_env_name("CROM_EVAL_LIVE").as_deref(),
            Some("CODE_AGENT_EVAL_LIVE")
        );
        assert_eq!(legacy_env_name("HOME"), None);
    }

    #[test]
    // New env var set: it is used with no warning.
    fn env_new_only() {
        let r = resolve_env_with("CROM_EVAL_LIVE", lookup(&[("CROM_EVAL_LIVE", "1")])).unwrap();
        assert_eq!(r.value, "1");
        assert!(r.warning.is_none());
    }

    #[test]
    // Only the legacy env var set: its value is honoured with a warning.
    fn env_old_only_honoured_with_warning() {
        let r = resolve_env_with(
            "CROM_CONFIG_HOME",
            lookup(&[("CODE_AGENT_CONFIG_HOME", "/old")]),
        )
        .unwrap();
        assert_eq!(r.value, "/old");
        let w = r.warning.expect("warning");
        assert!(
            w.contains("CODE_AGENT_CONFIG_HOME") && w.contains("CROM_CONFIG_HOME"),
            "{w}"
        );
    }

    #[test]
    // Both env vars set: the new name wins with no warning.
    fn env_both_new_wins() {
        let r = resolve_env_with(
            "CROM_CONFIG_HOME",
            lookup(&[
                ("CROM_CONFIG_HOME", "/new"),
                ("CODE_AGENT_CONFIG_HOME", "/old"),
            ]),
        )
        .unwrap();
        assert_eq!(r.value, "/new");
        assert!(r.warning.is_none());
    }

    #[test]
    // Neither env var set: resolution yields None.
    fn env_neither_is_none() {
        assert!(resolve_env_with("CROM_CONFIG_HOME", lookup(&[])).is_none());
    }

    #[test]
    // Legacy plugin variables map to the new names; others are unchanged.
    fn canonical_var_name_maps_legacy_plugin_vars() {
        assert_eq!(
            canonical_var_name("CODE_AGENT_PLUGIN_ROOT"),
            "CROM_PLUGIN_ROOT"
        );
        assert_eq!(canonical_var_name("CROM_PLUGIN_DATA"), "CROM_PLUGIN_DATA");
        assert_eq!(canonical_var_name("HOME"), "HOME");
    }
}
