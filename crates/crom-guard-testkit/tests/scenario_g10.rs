//! Scenario G10 fixture (#353, for #361): replays
//! `fixtures/scenarios/g10-subagent-escalation.json` through Guard and the
//! business API, with the ledger as the oracle.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;

use common::{env_with, POLICIES};
use crom_guard::{CallResult, GrantSpec};
use serde_json::Value;

const FIXTURE: &str = include_str!("../fixtures/scenarios/g10-subagent-escalation.json");

/// Every G10 step gives its expected result, and the ledger holds only the legitimate subtask's effects.
#[tokio::test]
async fn g10_fixture_replays_with_only_legit_effects() {
    let fx: Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(fx["scenario"], "G10");
    let spec: GrantSpec = serde_json::from_value(fx["grant"].clone()).unwrap();
    let e = env_with(POLICIES, spec).await;
    let mut handles = BTreeMap::from([("parent".to_owned(), e.handle.clone())]);

    for step in fx["steps"].as_array().unwrap() {
        let name = step["name"].as_str().unwrap();
        let handle = &handles[step["as"].as_str().unwrap()];
        let out = e
            .call_as(
                handle,
                step["tool"].as_str().unwrap(),
                step["arguments"].clone(),
            )
            .await;
        assert_eq!(
            out.code().unwrap_or("executed"),
            step["expect"].as_str().unwrap(),
            "{name}: {:?}",
            out.result
        );
        if let Some(bind) = step.get("bind_handle").and_then(Value::as_str) {
            let CallResult::Executed { content } = &out.result else {
                unreachable!()
            };
            handles.insert(bind.to_owned(), content["handle"].as_str().unwrap().into());
        }
    }

    let effects: Vec<Value> = e
        .api
        .ledger()
        .entries()
        .iter()
        .map(|l| serde_json::to_value(&l.effect).unwrap())
        .collect();
    let expected = fx["expected_ledger_effects"].as_array().unwrap();
    assert_eq!(effects.len(), expected.len(), "{effects:?}");
    for (got, want) in effects.iter().zip(expected) {
        for (k, v) in want.as_object().unwrap() {
            assert_eq!(&got[k], v, "{got} vs {want}");
        }
    }
}
