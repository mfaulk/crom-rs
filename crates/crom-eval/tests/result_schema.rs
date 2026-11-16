use std::time::Duration;

use crom_eval::artifacts::RunDir;
use crom_eval::fixture::default_tasks_dir;
use crom_eval::providers::{ProviderFault, ProviderMode};
use crom_eval::result_schema::{self, Outcome, RESULT_SCHEMA_JSON, RESULT_SCHEMA_VERSION};
use crom_eval::runner::{run_task, RunConfig};
use crom_eval::task::{discover_tasks, EvalTask};
use crom_eval::FailureClass;
use serde_json::Value;

fn find(id: &str) -> EvalTask {
    discover_tasks(&default_tasks_dir())
        .expect("discover tasks")
        .into_iter()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("task {id} not found"))
}

fn read_result_json(run: &RunDir, task_id: &str) -> Value {
    let path = run.task(task_id).unwrap().dir.join("result.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Minimal JSON Schema (draft 2020-12 subset) validator covering the keywords
/// the checked-in result schema uses: `type`, `required`, `properties`,
/// `additionalProperties: false`, `enum`, `const`, `items`, `minimum`, `$ref`
/// into `$defs`, and `anyOf`.
fn validate(schema: &Value, root: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        let name = r.strip_prefix("#/$defs/").expect("only local $defs refs");
        let target = &root["$defs"][name];
        assert!(!target.is_null(), "unresolved $ref {r}");
        return validate(target, root, value, path, errors);
    }
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
        let ok = any.iter().any(|s| {
            let mut e = Vec::new();
            validate(s, root, value, path, &mut e);
            e.is_empty()
        });
        if !ok {
            errors.push(format!("{path}: matched no anyOf branch: {value}"));
        }
        return;
    }
    if let Some(t) = schema.get("type") {
        let types: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let matches = types.iter().any(|t| match *t {
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "integer" => value.is_u64() || value.is_i64(),
            "number" => value.is_number(),
            "string" => value.is_string(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            other => panic!("unsupported type {other}"),
        });
        if !matches {
            errors.push(format!("{path}: expected {types:?}, got {value}"));
            return;
        }
    }
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        if !e.contains(value) {
            errors.push(format!("{path}: {value} not in enum {e:?}"));
        }
    }
    if let Some(c) = schema.get("const") {
        if c != value {
            errors.push(format!("{path}: expected const {c}, got {value}"));
        }
    }
    if let (Some(min), Some(n)) = (
        schema.get("minimum").and_then(Value::as_f64),
        value.as_f64(),
    ) {
        if n < min {
            errors.push(format!("{path}: {n} < minimum {min}"));
        }
    }
    if let Some(obj) = value.as_object() {
        for req in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let key = req.as_str().unwrap();
            if !obj.contains_key(key) {
                errors.push(format!("{path}: missing required '{key}'"));
            }
        }
        let props = schema.get("properties").and_then(Value::as_object);
        for (k, v) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(s) => validate(s, root, v, &format!("{path}.{k}"), errors),
                None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    errors.push(format!("{path}: unexpected property '{k}'"))
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(arr)) = (schema.get("items"), value.as_array()) {
        for (i, v) in arr.iter().enumerate() {
            validate(items, root, v, &format!("{path}[{i}]"), errors);
        }
    }
}

fn schema_errors(value: &Value) -> Vec<String> {
    let schema: Value = serde_json::from_str(RESULT_SCHEMA_JSON).expect("schema parses");
    let mut errors = Vec::new();
    validate(&schema, &schema, value, "$", &mut errors);
    errors
}

async fn run(task_id: &str, config: RunConfig, run_name: &str) -> (tempfile::TempDir, Value) {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), run_name).unwrap();
    let task = find(task_id);
    run_task(&task, &run, &config).await;
    let json = read_result_json(&run, task_id);
    (tmp, json)
}

fn golden() -> RunConfig {
    RunConfig {
        provider: ProviderMode::Golden,
        timeout_grace: Duration::from_secs(20),
        ..RunConfig::default()
    }
}

/// A golden run's result.json declares schema v2 and validates against the checked-in JSON Schema.
#[tokio::test]
async fn emitted_golden_result_validates_against_v2_schema() {
    let (_tmp, json) = run("golden-edit-recover", golden(), "schema-golden").await;
    assert_eq!(json["schema_version"], RESULT_SCHEMA_VERSION);
    assert_eq!(json["outcome"], "resolved");
    let errors = schema_errors(&json);
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

/// A fake-provider run's result.json (unresolved, usage null) also validates against the schema.
#[tokio::test]
async fn emitted_fake_result_validates_against_v2_schema() {
    let config = RunConfig {
        provider: ProviderMode::Fake,
        timeout_grace: Duration::from_secs(10),
        ..RunConfig::default()
    };
    let (_tmp, json) = run("smoke-noop", config, "schema-fake").await;
    let errors = schema_errors(&json);
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

/// The repro block records task content hash, prompt hash, tool policy, runtime budgets, seed, and host info.
#[tokio::test]
async fn result_records_reproducibility_metadata() {
    let mut config = golden();
    config.seed = 7;
    let (_tmp, json) = run("golden-edit-recover", config, "repro").await;
    let repro = &json["repro"];
    assert!(repro["task"]["content_hash"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(repro["task"]["content_version"].is_string());
    assert!(repro["prompt"]["hash"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(repro["seed"], 7);
    assert_eq!(repro["provider"]["model"], "golden");
    assert!(repro["tool_policy"]["builtin_tools"].is_object());
    assert_eq!(repro["isolation"]["backend"], "local");
    assert!(repro["runtime"]["max_turns"].as_u64().unwrap() > 0);
    assert!(repro["host"]["cpus"].as_u64().unwrap() > 0);
    assert!(repro["crom_version"].is_string());
    assert_eq!(
        repro["pricing_source_version"],
        crom_core::PricingTable::builtin().version.as_str()
    );
}

/// A provider 5xx during the run is classified as provider_error (outcome and failure class), not did_not_attempt.
#[tokio::test]
async fn provider_5xx_is_recorded_as_provider_error() {
    let config = RunConfig {
        provider: ProviderMode::Fault(ProviderFault::ServerError),
        timeout_grace: Duration::from_secs(10),
        ..RunConfig::default()
    };
    let (_tmp, json) = run("golden-edit-recover", config, "provider-5xx").await;
    assert_eq!(json["failure_class"], "provider_error", "{json:#}");
    assert_eq!(json["outcome"], "provider_error");
    assert_eq!(json["resolved"], false);
    let errors = schema_errors(&json);
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

/// Two runs that differ only in tool policy are flagged as non-comparable by the metadata diff.
#[tokio::test]
async fn different_tool_policies_are_not_comparable() {
    let (_a, a) = run("golden-edit-recover", golden(), "cmp-a").await;
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = RunDir::create(tmp.path(), "cmp-b").unwrap();
    let mut task = find("golden-edit-recover");
    task.allowed_tools = vec!["Read".into(), "Edit".into()];
    run_task(&task, &run_dir, &golden()).await;
    let b = read_result_json(&run_dir, "golden-edit-recover");

    let a = result_schema::parse_result(a).unwrap();
    let b = result_schema::parse_result(b).unwrap();
    let diff =
        result_schema::comparability_diff(a.repro.as_ref().unwrap(), b.repro.as_ref().unwrap());
    assert!(
        diff.iter().any(|d| d.starts_with("tool_policy")),
        "{diff:?}"
    );

    let (_c, c) = run("golden-edit-recover", golden(), "cmp-c").await;
    let c = result_schema::parse_result(c).unwrap();
    let same =
        result_schema::comparability_diff(a.repro.as_ref().unwrap(), c.repro.as_ref().unwrap());
    assert!(same.is_empty(), "identical configs must compare: {same:?}");
}

/// A v1 result.json (no schema_version/outcome/repro) still loads, with its outcome derived from status and failure class.
#[test]
fn v1_result_still_loads() {
    let text = include_str!("fixtures/result-v1.json");
    let v1: Value = serde_json::from_str(text).unwrap();
    assert!(v1.get("schema_version").is_none());

    let r = result_schema::parse_result(v1.clone()).unwrap();
    assert_eq!(r.schema_version, 1);
    assert!(r.repro.is_none());
    assert_eq!(r.outcome(), Outcome::Resolved);

    let mut errored = v1;
    errored["status"] = "errored".into();
    errored["resolved"] = false.into();
    errored["failure_class"] = "harness_error".into();
    let r = result_schema::parse_result(errored).unwrap();
    assert_eq!(r.failure_class, Some(FailureClass::HarnessError));
    assert_eq!(r.outcome(), Outcome::InfraError);
}

/// A result.json declaring an unknown future schema version is rejected rather than misread.
#[test]
fn unknown_result_schema_version_is_rejected() {
    let mut v: Value = serde_json::from_str(include_str!("fixtures/result-v1.json")).unwrap();
    v["schema_version"] = 99.into();
    assert!(result_schema::parse_result(v).is_err());
}
