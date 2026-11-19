use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

use crate::action::ValidatedArgs;
use crate::context::ExecutionContext;
use crate::ids::ActionId;
use crate::time::Timestamp;
use crate::traits::{BoxFuture, ExecuteError, ExecutionOutput, GrantStore, ProtectedOperation};

/// Tool names Guard keeps for its own operations (e.g. `guard.delegate`,
/// #353). Application operations cannot register under it.
pub const RESERVED_ACTION_PREFIX: &str = "guard.";

/// Largest serialized argument object the pipeline validates. Larger input
/// is `invalid_arguments` before schema validation.
pub const MAX_ARGUMENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    #[error("action {0} is already registered")]
    Duplicate(ActionId),
    #[error("action {0} uses the prefix {RESERVED_ACTION_PREFIX:?}, which is reserved for Guard")]
    Reserved(ActionId),
    #[error("input schema of {action} rejected: {problem}")]
    InvalidSchema { action: ActionId, problem: String },
    #[error("logged field {field:?} of {action} is not a top-level schema property")]
    UnknownLoggedField { action: ActionId, field: String },
}

/// An operation Guard implements itself, with access to the caller's
/// execution context and the grant store (extension point for
/// `guard.delegate`, #353). Registered only from inside this crate under
/// [`RESERVED_ACTION_PREFIX`]. The pipeline runs authentication, schema
/// validation, the action-in-every-grant check, the decision record, and
/// the recheck for it, but no resolver or Cedar evaluation: the operation
/// must enforce its own rules (for delegation, narrowing in the store).
#[allow(dead_code)] // first implemented by #353
pub(crate) trait BuiltinOperation: Send + Sync {
    fn action(&self) -> &ActionId;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &Value;
    fn logged_fields(&self) -> &[&'static str] {
        &[]
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ExecutionContext,
        store: &'a dyn GrantStore,
        args: &'a ValidatedArgs,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>>;
}

pub(crate) enum OpKind {
    Protected(Arc<dyn ProtectedOperation>),
    #[allow(dead_code)] // first constructed by #353
    Builtin(Arc<dyn BuiltinOperation>),
}

pub(crate) struct Registered {
    pub(crate) action: ActionId,
    pub(crate) kind: OpKind,
    validator: jsonschema::Validator,
}

impl Registered {
    fn description(&self) -> &str {
        match &self.kind {
            OpKind::Protected(o) => o.description(),
            OpKind::Builtin(o) => o.description(),
        }
    }

    fn input_schema(&self) -> &Value {
        match &self.kind {
            OpKind::Protected(o) => o.input_schema(),
            OpKind::Builtin(o) => o.input_schema(),
        }
    }

    pub(crate) fn logged_fields(&self) -> &[&'static str] {
        match &self.kind {
            OpKind::Protected(o) => o.logged_fields(),
            OpKind::Builtin(o) => o.logged_fields(),
        }
    }

    /// Size bound, then the operation's JSON Schema. Only this builds a
    /// `ValidatedArgs` for the pipeline.
    pub(crate) fn validate(&self, args: Value) -> Option<ValidatedArgs> {
        let size = serde_json::to_vec(&args).map(|v| v.len()).ok()?;
        if size > MAX_ARGUMENT_BYTES || !args.is_object() || !self.validator.is_valid(&args) {
            return None;
        }
        Some(ValidatedArgs::new(args))
    }
}

/// One advertised tool, for MCP `tools/list` (#357).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ToolInfo<'a> {
    pub name: &'a ActionId,
    pub description: &'a str,
    pub input_schema: &'a Value,
}

/// The protected operations Guard exposes, one MCP tool name per action.
/// Lookup is an exact, case-sensitive match on the action id: there is no
/// normalization and there are no aliases.
///
/// Input schemas are checked at registration against a keyword allowlist
/// (see `docs/guard/pipeline.md`): every subschema declares one `type`,
/// objects are closed with `"additionalProperties": false`, arrays have
/// typed `items`, and nothing is referenced or composed. The schema is then
/// compiled as JSON Schema 2020-12 with `format` assertion on and the
/// linear-time `regex` engine for `pattern`.
#[derive(Default)]
pub struct OperationRegistry {
    ops: BTreeMap<String, Registered>,
}

impl std::fmt::Debug for OperationRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationRegistry")
            .field("actions", &self.ops.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl OperationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, op: Arc<dyn ProtectedOperation>) -> Result<(), RegistryError> {
        let action = op.action().clone();
        if action.as_str().starts_with(RESERVED_ACTION_PREFIX) {
            return Err(RegistryError::Reserved(action));
        }
        let validator = compile(&action, op.input_schema(), op.logged_fields())?;
        self.insert(action, OpKind::Protected(op), validator)
    }

    #[allow(dead_code)] // first used by #353
    pub(crate) fn register_builtin(
        &mut self,
        op: Arc<dyn BuiltinOperation>,
    ) -> Result<(), RegistryError> {
        let action = op.action().clone();
        if !action.as_str().starts_with(RESERVED_ACTION_PREFIX) {
            return Err(RegistryError::InvalidSchema {
                action,
                problem: format!("builtin operations must use {RESERVED_ACTION_PREFIX:?}"),
            });
        }
        let validator = compile(&action, op.input_schema(), op.logged_fields())?;
        self.insert(action, OpKind::Builtin(op), validator)
    }

    fn insert(
        &mut self,
        action: ActionId,
        kind: OpKind,
        validator: jsonschema::Validator,
    ) -> Result<(), RegistryError> {
        if self.ops.contains_key(action.as_str()) {
            return Err(RegistryError::Duplicate(action));
        }
        self.ops.insert(
            action.as_str().to_owned(),
            Registered {
                action,
                kind,
                validator,
            },
        );
        Ok(())
    }

    /// Exact match only.
    pub(crate) fn get(&self, tool: &str) -> Option<&Registered> {
        self.ops.get(tool)
    }

    /// For `GrantStoreConfig::registered_actions`.
    pub fn action_ids(&self) -> BTreeSet<ActionId> {
        self.ops.values().map(|r| r.action.clone()).collect()
    }

    pub fn tools(&self) -> impl Iterator<Item = ToolInfo<'_>> {
        self.ops.values().map(|r| ToolInfo {
            name: &r.action,
            description: r.description(),
            input_schema: r.input_schema(),
        })
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

fn compile(
    action: &ActionId,
    schema: &Value,
    logged: &[&'static str],
) -> Result<jsonschema::Validator, RegistryError> {
    let invalid = |problem: String| RegistryError::InvalidSchema {
        action: action.clone(),
        problem,
    };
    check_closed(schema, "#").map_err(invalid)?;
    let props = schema.get("properties").and_then(Value::as_object);
    if let Some(field) = logged
        .iter()
        .find(|f| !props.is_some_and(|p| p.contains_key(**f)))
    {
        return Err(RegistryError::UnknownLoggedField {
            action: action.clone(),
            field: (*field).to_owned(),
        });
    }
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .with_pattern_options(jsonschema::PatternOptions::regex())
        .should_validate_formats(true)
        .build(schema)
        .map_err(|e| invalid(e.to_string()))
}

/// The only `$schema` accepted (at the root only).
const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Annotation and value keywords allowed on any typed schema.
const COMMON_KEYWORDS: &[&str] = &[
    "type",
    "title",
    "description",
    "$comment",
    "examples",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "enum",
    "const",
];

/// `format` values Guard asserts (format assertion is enabled).
const FORMATS: &[&str] = &[
    "date-time",
    "date",
    "time",
    "duration",
    "email",
    "hostname",
    "ipv4",
    "ipv6",
    "uri",
    "uri-reference",
    "uuid",
];

fn type_keywords(ty: &str) -> Option<&'static [&'static str]> {
    Some(match ty {
        "string" => &["minLength", "maxLength", "pattern", "format"],
        "integer" | "number" => &[
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ],
        "boolean" | "null" => &[],
        "array" => &["items", "minItems", "maxItems", "uniqueItems"],
        "object" => &[
            "properties",
            "required",
            "additionalProperties",
            "minProperties",
            "maxProperties",
        ],
        _ => return None,
    })
}

/// Allowlist check. Every reachable subschema is a JSON object with a single
/// `type` and only that type's keywords: arrays must have an `items`
/// subschema, and objects `additionalProperties: false`. Boolean and empty
/// schemas, type unions, composition (`allOf`, `oneOf`, `not`, `if`, ...),
/// `$ref`, `$defs`, and every other keyword are refused, so nothing in the
/// arguments escapes validation. The only applicators are `properties` and
/// `items`, and both are walked.
fn check_closed(schema: &Value, path: &str) -> Result<(), String> {
    let obj = schema
        .as_object()
        .ok_or_else(|| format!("{path}: subschemas must be JSON objects, not booleans"))?;
    let root = path == "#";
    let ty = match obj.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(_) => return Err(format!("{path}: \"type\" must be a single type name")),
        None => return Err(format!("{path}: every subschema must declare \"type\"")),
    };
    if root && ty != "object" {
        return Err("#: root must have \"type\": \"object\"".into());
    }
    let allowed = type_keywords(ty).ok_or_else(|| format!("{path}: unknown type {ty:?}"))?;
    for (k, v) in obj {
        if root && k == "$schema" {
            if v != DRAFT_2020_12 {
                return Err(format!("#: $schema must be {DRAFT_2020_12:?}"));
            }
            continue;
        }
        if !COMMON_KEYWORDS.contains(&k.as_str()) && !allowed.contains(&k.as_str()) {
            return Err(format!(
                "{path}: keyword {k:?} is not allowed for type {ty:?}"
            ));
        }
    }
    if let Some(f) = obj.get("format") {
        if !f.as_str().is_some_and(|f| FORMATS.contains(&f)) {
            return Err(format!("{path}: unsupported format {f}"));
        }
    }
    match ty {
        "array" => {
            let items = obj
                .get("items")
                .ok_or_else(|| format!("{path}: arrays need an \"items\" subschema"))?;
            check_closed(items, &format!("{path}/items"))?;
        }
        "object" => {
            if obj.get("additionalProperties") != Some(&Value::Bool(false)) {
                return Err(format!(
                    "{path}: object schemas need \"additionalProperties\": false"
                ));
            }
            if let Some(props) = obj.get("properties") {
                let props = props
                    .as_object()
                    .ok_or_else(|| format!("{path}: \"properties\" must be an object"))?;
                for (name, sub) in props {
                    check_closed(sub, &format!("{path}/properties/{name}"))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn compiled(props: Value) -> jsonschema::Validator {
        let schema = json!({"type": "object", "properties": props, "additionalProperties": false});
        compile(&ActionId::new("t.op").unwrap(), &schema, &[]).unwrap()
    }

    /// `format` is asserted, not just annotated: a malformed email or uuid fails validation.
    #[test]
    fn formats_are_asserted() {
        let v = compiled(json!({"e": {"type": "string", "format": "email"},
                                "u": {"type": "string", "format": "uuid"}}));
        assert!(
            v.is_valid(&json!({"e": "a@example.com", "u": "67e55044-10b1-426f-9247-bb680e5fe0c8"}))
        );
        assert!(!v.is_valid(&json!({"e": "not-an-email"})));
        assert!(!v.is_valid(&json!({"u": "not-a-uuid"})));
    }
}
