//! Plugin manifest parsing and validation (M8).
//!
//! Plugins are described by a `.agent-plugin/plugin.json` file at the
//! plugin root. A `.claude-plugin/plugin.json` may be loaded when the
//! caller opts into compatibility mode. The manifest is intentionally
//! conservative: every component path is a relative path resolved
//! against the plugin root, names must be identifier-safe, and unknown
//! fields are preserved in `metadata` so future loaders can warn.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum length of a plugin name. Mirrors the agent-name cap so plugin
/// component names stay well under filesystem limits even after the
/// `<plugin>:<component>` namespacing prefix is applied.
pub const MAX_PLUGIN_NAME_LEN: usize = 64;

/// Default manifest path relative to the plugin root.
pub const PLUGIN_MANIFEST_PATH: &str = ".agent-plugin/plugin.json";
/// Compatibility manifest path. Only consulted when the caller opts in.
pub const COMPAT_PLUGIN_MANIFEST_PATH: &str = ".claude-plugin/plugin.json";

/// Maximum manifest file size. Generous, but bounded so a pathological
/// file cannot exhaust memory.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Author metadata block on a plugin manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginAuthor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Parsed plugin manifest. Path fields are normalized to be relative to the
/// plugin root with a leading `./` stripped; absolute paths and any path
/// containing `..` are rejected at parse time.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub author: Option<PluginAuthor>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    pub keywords: Vec<String>,

    pub skills: Vec<PathBuf>,
    pub commands: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
    pub hooks: Option<PathBuf>,
    pub mcp_servers: Option<PathBuf>,
    pub settings: Option<PathBuf>,
    pub bin: Option<PathBuf>,

    pub metadata: serde_json::Value,
}

#[derive(Debug, Error)]
pub enum PluginManifestError {
    #[error("read manifest at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("manifest at {path} exceeds {max} bytes")]
    TooLarge { path: PathBuf, max: usize },
    #[error("manifest at {path}: invalid JSON: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("manifest at {path}: missing required field {field}")]
    MissingField { path: PathBuf, field: &'static str },
    #[error("manifest at {path}: invalid field {field}: {message}")]
    InvalidField {
        path: PathBuf,
        field: &'static str,
        message: String,
    },
    #[error("manifest at {path}: plugin name {name:?} is not identifier-safe")]
    UnsafeName { path: PathBuf, name: String },
    #[error(
        "manifest at {path}: plugin path {component_path:?} is absolute or escapes plugin root"
    )]
    UnsafePath {
        path: PathBuf,
        component_path: String,
    },
}

#[derive(Debug, Default, Deserialize)]
struct RawManifest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    author: Option<RawAuthor>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    keywords: Option<serde_json::Value>,
    #[serde(default)]
    skills: Option<serde_json::Value>,
    #[serde(default)]
    commands: Option<serde_json::Value>,
    #[serde(default)]
    agents: Option<serde_json::Value>,
    #[serde(default)]
    hooks: Option<serde_json::Value>,
    #[serde(rename = "mcpServers", alias = "mcp_servers", default)]
    mcp_servers: Option<serde_json::Value>,
    #[serde(default)]
    settings: Option<serde_json::Value>,
    #[serde(default)]
    bin: Option<serde_json::Value>,
    #[serde(flatten)]
    extras: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawAuthor {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl PluginManifest {
    /// Read and parse a manifest from `path`. The plugin root is the
    /// parent of the `.agent-plugin/` directory containing the manifest.
    pub fn load_from_path(path: &Path) -> Result<Self, PluginManifestError> {
        let bytes = std::fs::read(path).map_err(|e| PluginManifestError::Read {
            path: path.to_path_buf(),
            source: e,
        })?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PluginManifestError::TooLarge {
                path: path.to_path_buf(),
                max: MAX_MANIFEST_BYTES,
            });
        }
        Self::parse(&bytes, path)
    }

    /// Parse manifest bytes. Public so SDK callers and tests can avoid
    /// going through disk.
    pub fn parse(bytes: &[u8], source_path: &Path) -> Result<Self, PluginManifestError> {
        let raw: RawManifest =
            serde_json::from_slice(bytes).map_err(|e| PluginManifestError::InvalidJson {
                path: source_path.to_path_buf(),
                source: e,
            })?;

        let name = raw.name.ok_or_else(|| PluginManifestError::MissingField {
            path: source_path.to_path_buf(),
            field: "name",
        })?;
        if !is_plugin_name_safe(&name) {
            return Err(PluginManifestError::UnsafeName {
                path: source_path.to_path_buf(),
                name,
            });
        }
        // Validate `version` strictly. Without this an attacker could
        // ship a manifest whose version is `../../../etc` and have the
        // plugin install/uninstall paths join that into the cache root,
        // which would resolve to (and `remove_dir_all`!) directories
        // outside the plugin cache.
        if let Some(v) = raw.version.as_deref() {
            if !is_plugin_version_safe(v) {
                return Err(PluginManifestError::InvalidField {
                    path: source_path.to_path_buf(),
                    field: "version",
                    message: format!(
                        "version {v:?} contains characters that are not safe \
                         in a filesystem path"
                    ),
                });
            }
        }

        let mut keywords = Vec::new();
        if let Some(v) = raw.keywords {
            keywords = parse_string_list(&v, "keywords").map_err(|message| {
                PluginManifestError::InvalidField {
                    path: source_path.to_path_buf(),
                    field: "keywords",
                    message,
                }
            })?;
        }

        let skills = parse_path_list_field(raw.skills, "skills", source_path)?;
        let commands = parse_path_list_field(raw.commands, "commands", source_path)?;
        let agents = parse_path_list_field(raw.agents, "agents", source_path)?;

        let hooks = parse_optional_path_field(raw.hooks, "hooks", source_path)?;
        let mcp_servers = parse_optional_path_field(raw.mcp_servers, "mcpServers", source_path)?;
        let settings = parse_optional_path_field(raw.settings, "settings", source_path)?;
        let bin = parse_optional_path_field(raw.bin, "bin", source_path)?;

        let metadata = if raw.extras.is_empty() {
            serde_json::Value::Null
        } else {
            for k in raw.extras.keys() {
                tracing::warn!(
                    unknown = k.as_str(),
                    "preserving unknown plugin manifest key"
                );
            }
            serde_json::Value::Object(raw.extras)
        };

        let author = raw.author.map(|a| PluginAuthor {
            name: a.name,
            email: a.email,
            url: a.url,
        });

        Ok(PluginManifest {
            name,
            version: raw.version,
            description: raw.description,
            author,
            homepage: raw.homepage,
            repository: raw.repository,
            license: raw.license,
            keywords,
            skills,
            commands,
            agents,
            hooks,
            mcp_servers,
            settings,
            bin,
            metadata,
        })
    }

    /// Iterator of every component path declared in the manifest, paired
    /// with the field it came from. Used by validators to confirm each
    /// resolved path stays inside the plugin root.
    pub fn declared_paths(&self) -> Vec<(&'static str, PathBuf)> {
        let mut out: Vec<(&'static str, PathBuf)> = Vec::new();
        for p in &self.skills {
            out.push(("skills", p.clone()));
        }
        for p in &self.commands {
            out.push(("commands", p.clone()));
        }
        for p in &self.agents {
            out.push(("agents", p.clone()));
        }
        if let Some(p) = &self.hooks {
            out.push(("hooks", p.clone()));
        }
        if let Some(p) = &self.mcp_servers {
            out.push(("mcpServers", p.clone()));
        }
        if let Some(p) = &self.settings {
            out.push(("settings", p.clone()));
        }
        if let Some(p) = &self.bin {
            out.push(("bin", p.clone()));
        }
        out
    }
}

/// Identifier-safe plugin name: lowercase ASCII letters, digits, `-`, `_`.
/// Empty, control, path-separator, and shell-metacharacter names are
/// rejected. Length capped to keep namespaced child names compact.
pub fn is_plugin_name_safe(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_PLUGIN_NAME_LEN {
        return false;
    }
    if name == "." || name == ".." {
        return false;
    }
    name.chars()
        .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_'))
}

/// Identifier-safe plugin version: ASCII alphanumerics, `.`, `-`, `_`,
/// and `+`. The version is used as a directory component in the cache
/// path; this validation prevents path-traversal sequences like
/// `../../etc` from being accepted as a "version".
pub fn is_plugin_version_safe(version: &str) -> bool {
    if version.is_empty() || version.len() > MAX_PLUGIN_NAME_LEN {
        return false;
    }
    if version == "." || version == ".." {
        return false;
    }
    if version.starts_with('.') {
        // Reject leading-dot versions so `..` etc. cannot sneak through.
        return false;
    }
    version
        .chars()
        .all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' | '+'))
}

/// Resolve a manifest-declared relative path against `plugin_root`. Returns
/// the canonicalized absolute path on success, or
/// [`PluginManifestError::UnsafePath`] if the path escapes the root.
///
/// The relative path must already have been normalized by the manifest
/// loader (no leading `./`, no `..`, not absolute). This helper also
/// canonicalizes both the root and the resolved path so symlink escapes
/// are caught.
pub fn resolve_safe_plugin_path(
    plugin_root: &Path,
    relative: &Path,
) -> Result<PathBuf, PluginManifestError> {
    if relative.is_absolute() {
        return Err(PluginManifestError::UnsafePath {
            path: plugin_root.to_path_buf(),
            component_path: relative.display().to_string(),
        });
    }
    for c in relative.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PluginManifestError::UnsafePath {
                    path: plugin_root.to_path_buf(),
                    component_path: relative.display().to_string(),
                });
            }
            _ => {}
        }
    }
    let canonical_root =
        plugin_root
            .canonicalize()
            .map_err(|_| PluginManifestError::UnsafePath {
                path: plugin_root.to_path_buf(),
                component_path: relative.display().to_string(),
            })?;
    let joined = canonical_root.join(relative);
    let canonical_joined = match joined.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // Path may not yet exist (e.g. optional resource); fall back
            // to a lexical check against the canonical root. The lexical
            // form has already been validated for `..` above.
            joined
        }
    };
    if !canonical_joined.starts_with(&canonical_root) {
        return Err(PluginManifestError::UnsafePath {
            path: plugin_root.to_path_buf(),
            component_path: relative.display().to_string(),
        });
    }
    Ok(canonical_joined)
}

fn parse_path_list_field(
    value: Option<serde_json::Value>,
    field: &'static str,
    source_path: &Path,
) -> Result<Vec<PathBuf>, PluginManifestError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let normalized =
        parse_path_list(&value).map_err(|message| PluginManifestError::InvalidField {
            path: source_path.to_path_buf(),
            field,
            message,
        })?;
    for p in &normalized {
        validate_relative_no_escape(p).map_err(|component_path| {
            PluginManifestError::UnsafePath {
                path: source_path.to_path_buf(),
                component_path,
            }
        })?;
    }
    Ok(normalized)
}

fn parse_optional_path_field(
    value: Option<serde_json::Value>,
    field: &'static str,
    source_path: &Path,
) -> Result<Option<PathBuf>, PluginManifestError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let s = value
        .as_str()
        .ok_or_else(|| PluginManifestError::InvalidField {
            path: source_path.to_path_buf(),
            field,
            message: format!("expected a string, got {value}"),
        })?;
    if s.is_empty() {
        return Ok(None);
    }
    let path = normalize_relative(s);
    validate_relative_no_escape(&path).map_err(|component_path| {
        PluginManifestError::UnsafePath {
            path: source_path.to_path_buf(),
            component_path,
        }
    })?;
    Ok(Some(path))
}

fn parse_string_list(
    value: &serde_json::Value,
    field: &'static str,
) -> Result<Vec<String>, String> {
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let s = item
                    .as_str()
                    .ok_or_else(|| format!("`{field}` items must be strings"))?;
                out.push(s.to_string());
            }
            Ok(out)
        }
        other => Err(format!(
            "expected `{field}` to be a list of strings, got {other}"
        )),
    }
}

fn parse_path_list(value: &serde_json::Value) -> Result<Vec<PathBuf>, String> {
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::String(s) => {
            if s.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![normalize_relative(s)])
            }
        }
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let s = item
                    .as_str()
                    .ok_or_else(|| "list items must be strings".to_string())?;
                out.push(normalize_relative(s));
            }
            Ok(out)
        }
        other => Err(format!("expected a string or list of strings, got {other}")),
    }
}

fn normalize_relative(input: &str) -> PathBuf {
    let trimmed = input.trim();
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let trimmed = trimmed.strip_prefix(".\\").unwrap_or(trimmed);
    PathBuf::from(trimmed)
}

fn validate_relative_no_escape(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("path must not be empty".to_string());
    }
    if path.is_absolute() {
        return Err(path.display().to_string());
    }
    for c in path.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(path.display().to_string());
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_path() -> PathBuf {
        PathBuf::from("/tmp/.agent-plugin/plugin.json")
    }

    #[test]
    // A complete manifest parses with all fields populated.
    fn parses_complete_manifest() {
        let json = serde_json::json!({
            "name": "rust-quality",
            "version": "1.0.0",
            "description": "Rust review",
            "author": {"name": "DevTools", "email": "dev@example.com"},
            "homepage": "https://example.com",
            "repository": "https://example.com/repo.git",
            "license": "MIT",
            "keywords": ["rust", "quality"],
            "skills": "./skills/",
            "commands": "./commands/",
            "agents": "./agents/",
            "hooks": "./hooks/hooks.json",
            "mcpServers": "./.mcp.json",
            "settings": "./settings.json",
            "bin": "./bin/",
            "extra_field": {"foo": 42}
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let m = PluginManifest::parse(&bytes, &fake_path()).unwrap();
        assert_eq!(m.name, "rust-quality");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.skills.len(), 1);
        assert_eq!(m.skills[0], PathBuf::from("skills/"));
        assert_eq!(m.hooks.as_deref().unwrap(), Path::new("hooks/hooks.json"));
        assert_eq!(m.bin.as_deref().unwrap(), Path::new("bin/"));
        assert!(m.metadata.is_object(), "unknown fields preserved");
    }

    #[test]
    // A missing `name` field is rejected with a structured error.
    fn missing_name_rejected() {
        let bytes = b"{}";
        let err = PluginManifest::parse(bytes, &fake_path()).unwrap_err();
        assert!(matches!(
            err,
            PluginManifestError::MissingField { field: "name", .. }
        ));
    }

    #[test]
    // An unsafe plugin name (uppercase) is rejected.
    fn unsafe_name_rejected() {
        let bytes = br#"{"name": "BadName"}"#;
        let err = PluginManifest::parse(bytes, &fake_path()).unwrap_err();
        assert!(matches!(err, PluginManifestError::UnsafeName { .. }));
    }

    #[test]
    // Absolute component paths are rejected.
    fn absolute_path_rejected() {
        let bytes = br#"{"name": "ok", "skills": "/etc/passwd"}"#;
        let err = PluginManifest::parse(bytes, &fake_path()).unwrap_err();
        assert!(matches!(err, PluginManifestError::UnsafePath { .. }));
    }

    #[test]
    // Path traversal entries are rejected.
    fn traversal_path_rejected() {
        let bytes = br#"{"name": "ok", "skills": "../etc"}"#;
        let err = PluginManifest::parse(bytes, &fake_path()).unwrap_err();
        assert!(matches!(err, PluginManifestError::UnsafePath { .. }));
    }

    #[test]
    // Invalid JSON returns InvalidJson with file context.
    fn invalid_json_rejected() {
        let bytes = b"{not json}";
        let err = PluginManifest::parse(bytes, &fake_path()).unwrap_err();
        assert!(matches!(err, PluginManifestError::InvalidJson { .. }));
    }

    #[test]
    // The leading `./` is normalized away from path entries.
    fn leading_dot_slash_normalized() {
        let bytes = br#"{"name": "ok", "skills": "./skills/"}"#;
        let m = PluginManifest::parse(bytes, &fake_path()).unwrap();
        assert_eq!(m.skills[0], PathBuf::from("skills/"));
    }

    #[test]
    // Skills as an array of paths is supported.
    fn skills_array_supported() {
        let bytes = br#"{"name": "ok", "skills": ["./a", "./b"]}"#;
        let m = PluginManifest::parse(bytes, &fake_path()).unwrap();
        assert_eq!(m.skills.len(), 2);
    }

    #[test]
    // is_plugin_name_safe accepts identifier-safe names and rejects unsafe ones.
    fn plugin_name_safety_rules() {
        assert!(is_plugin_name_safe("rust-quality"));
        assert!(is_plugin_name_safe("docs_tools"));
        assert!(is_plugin_name_safe("a"));
        assert!(!is_plugin_name_safe("Rust"));
        assert!(!is_plugin_name_safe("with space"));
        assert!(!is_plugin_name_safe("a/b"));
        assert!(!is_plugin_name_safe(".."));
        assert!(!is_plugin_name_safe(""));
    }

    #[test]
    // is_plugin_version_safe accepts well-formed semver-ish strings and
    // rejects path-traversal sequences. Without this check, an
    // attacker-controlled version field could redirect the cache path.
    fn version_safety_rejects_traversal() {
        assert!(is_plugin_version_safe("1.0.0"));
        assert!(is_plugin_version_safe("1.0.0-rc.1"));
        assert!(is_plugin_version_safe("v1"));
        assert!(!is_plugin_version_safe("../etc"));
        assert!(!is_plugin_version_safe(".."));
        assert!(!is_plugin_version_safe("1.0/0"));
        assert!(!is_plugin_version_safe("1.0\\0"));
        assert!(!is_plugin_version_safe(""));
        assert!(!is_plugin_version_safe(".hidden"));
    }

    #[test]
    // A manifest whose `version` field contains a traversal sequence is
    // rejected at parse time.
    fn manifest_rejects_unsafe_version() {
        let json = serde_json::json!({
            "name": "ok",
            "version": "../../../etc"
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let err = PluginManifest::parse(&bytes, &fake_path()).unwrap_err();
        assert!(matches!(
            err,
            PluginManifestError::InvalidField {
                field: "version",
                ..
            }
        ));
    }
}
