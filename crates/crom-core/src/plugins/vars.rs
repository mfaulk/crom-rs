//! Variable substitution for plugin configuration files.
//!
//! Supports four `${VAR}` forms:
//! - `${CROM_PLUGIN_ROOT}` — the cached plugin root directory.
//! - `${CROM_PLUGIN_DATA}` — the persistent plugin data directory.
//! - `${ENV_VAR}` — process-environment variables.
//! - `${user_config.key}` — values from `plugins.plugin_config.<plugin>`.
//!
//! All other `${...}` expressions are an error so typos are caught early.

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;

use super::registry::PluginRuntimePaths;

/// Errors produced by [`substitute_str`] / [`substitute_value`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PluginSubstError {
    #[error("missing environment variable {var:?} (referenced in {field})")]
    MissingEnv { var: String, field: String },
    #[error("missing user_config key {key:?} (referenced in {field})")]
    MissingUserConfig { key: String, field: String },
    #[error("invalid variable {var:?} in {field}: {message}")]
    InvalidVar {
        var: String,
        field: String,
        message: String,
    },
}

/// Context supplied to [`substitute_str`] / [`substitute_value`]. The
/// caller is responsible for assembling the user-config map (typically
/// `settings.plugin_config.<plugin>`).
#[derive(Debug, Clone, Default)]
pub struct PluginVarContext<'a> {
    pub paths: Option<&'a PluginRuntimePaths>,
    pub user_config: BTreeMap<String, Value>,
}

impl<'a> PluginVarContext<'a> {
    pub fn new(paths: &'a PluginRuntimePaths) -> Self {
        Self {
            paths: Some(paths),
            user_config: BTreeMap::new(),
        }
    }

    pub fn with_user_config(mut self, user_config: BTreeMap<String, Value>) -> Self {
        self.user_config = user_config;
        self
    }
}

/// Substitute every `${...}` reference in `input`. `field` is included in
/// error messages so operators can locate which manifest field had the
/// bad reference.
pub fn substitute_str(
    input: &str,
    field: &str,
    ctx: &PluginVarContext<'_>,
) -> Result<String, PluginSubstError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Trailing `${...` with no closing `}`. Preserve verbatim.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let var_full = &after[..end];
        let value = lookup_var(var_full, field, ctx)?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn lookup_var(
    var: &str,
    field: &str,
    ctx: &PluginVarContext<'_>,
) -> Result<String, PluginSubstError> {
    if var.is_empty() {
        return Err(PluginSubstError::InvalidVar {
            var: var.to_string(),
            field: field.to_string(),
            message: "empty variable name".to_string(),
        });
    }
    if var == "CROM_PLUGIN_ROOT" {
        return paths(ctx, var, field).map(|p| p.root.display().to_string());
    }
    if var == "CROM_PLUGIN_DATA" {
        return paths(ctx, var, field).map(|p| p.data.display().to_string());
    }
    if let Some(rest) = var.strip_prefix("user_config.") {
        if !rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || rest.is_empty()
        {
            return Err(PluginSubstError::InvalidVar {
                var: var.to_string(),
                field: field.to_string(),
                message: "invalid user_config key".to_string(),
            });
        }
        return ctx
            .user_config
            .get(rest)
            .ok_or_else(|| PluginSubstError::MissingUserConfig {
                key: rest.to_string(),
                field: field.to_string(),
            })
            .and_then(|v| match v {
                Value::String(s) => Ok(s.clone()),
                Value::Number(n) => Ok(n.to_string()),
                Value::Bool(b) => Ok(b.to_string()),
                Value::Null => Ok(String::new()),
                Value::Array(_) | Value::Object(_) => Err(PluginSubstError::InvalidVar {
                    var: var.to_string(),
                    field: field.to_string(),
                    message: "user_config values must be scalars for substitution".to_string(),
                }),
            });
    }
    if !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(PluginSubstError::InvalidVar {
            var: var.to_string(),
            field: field.to_string(),
            message: "must match [A-Za-z0-9_]+".to_string(),
        });
    }
    std::env::var(var).map_err(|_| PluginSubstError::MissingEnv {
        var: var.to_string(),
        field: field.to_string(),
    })
}

fn paths<'a>(
    ctx: &'a PluginVarContext<'_>,
    var: &str,
    field: &str,
) -> Result<&'a PluginRuntimePaths, PluginSubstError> {
    ctx.paths.ok_or_else(|| PluginSubstError::InvalidVar {
        var: var.to_string(),
        field: field.to_string(),
        message: "no plugin runtime paths available".to_string(),
    })
}

/// Substitute variables inside a `serde_json::Value` recursively. Strings
/// are interpolated; numbers/booleans/nulls pass through.
pub fn substitute_value(
    value: &Value,
    field: &str,
    ctx: &PluginVarContext<'_>,
) -> Result<Value, PluginSubstError> {
    match value {
        Value::String(s) => substitute_str(s, field, ctx).map(Value::String),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (idx, item) in items.iter().enumerate() {
                let nested_field = format!("{field}[{idx}]");
                out.push(substitute_value(item, &nested_field, ctx)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let nested_field = format!("{field}.{k}");
                out.insert(k.clone(), substitute_value(v, &nested_field, ctx)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(root: &str, data: &str) -> PluginRuntimePaths {
        PluginRuntimePaths {
            root: PathBuf::from(root),
            data: PathBuf::from(data),
        }
    }

    #[test]
    // Plugin-root and plugin-data variables resolve to their paths.
    fn substitutes_plugin_path_vars() {
        let p = ctx("/r", "/d");
        let c = PluginVarContext::new(&p);
        assert_eq!(
            substitute_str("${CROM_PLUGIN_ROOT}/x", "f", &c).unwrap(),
            "/r/x"
        );
        assert_eq!(
            substitute_str("${CROM_PLUGIN_DATA}/y", "f", &c).unwrap(),
            "/d/y"
        );
    }

    #[test]
    // Process env vars resolve via std::env.
    fn substitutes_env_var() {
        temp_env::with_var("PLUGIN_VARS_TEST_VAL", Some("abc"), || {
            let p = ctx("/r", "/d");
            let c = PluginVarContext::new(&p);
            assert_eq!(
                substitute_str("${PLUGIN_VARS_TEST_VAL}", "f", &c).unwrap(),
                "abc"
            );
        });
    }

    #[test]
    // Missing env var produces a structured MissingEnv error.
    fn missing_env_errors() {
        let p = ctx("/r", "/d");
        let c = PluginVarContext::new(&p);
        let err = substitute_str("${PLUGIN_VARS_NOPE}", "f", &c).unwrap_err();
        assert!(matches!(err, PluginSubstError::MissingEnv { .. }));
    }

    #[test]
    // user_config keys substitute scalar values.
    fn substitutes_user_config_scalar() {
        let p = ctx("/r", "/d");
        let mut user = BTreeMap::new();
        user.insert("review_depth".to_string(), Value::String("normal".into()));
        let c = PluginVarContext::new(&p).with_user_config(user);
        assert_eq!(
            substitute_str("depth=${user_config.review_depth}", "f", &c).unwrap(),
            "depth=normal"
        );
    }

    #[test]
    // Missing user_config key produces a structured MissingUserConfig error.
    fn missing_user_config_errors() {
        let p = ctx("/r", "/d");
        let c = PluginVarContext::new(&p);
        let err = substitute_str("${user_config.gone}", "f", &c).unwrap_err();
        assert!(matches!(err, PluginSubstError::MissingUserConfig { .. }));
    }

    #[test]
    // Trailing `${...` without closing brace is preserved verbatim.
    fn unclosed_brace_preserved() {
        let p = ctx("/r", "/d");
        let c = PluginVarContext::new(&p);
        assert_eq!(substitute_str("a${X", "f", &c).unwrap(), "a${X");
    }
}
