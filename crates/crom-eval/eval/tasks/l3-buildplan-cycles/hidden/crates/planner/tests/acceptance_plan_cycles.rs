use depgraph::GraphError;
use planner::{plan_source, PlanError};

const SRC: &str = "\
app: lib cli
cli: util
lib: util
util: codec
codec: lib
tool:
";

#[test]
fn cyclic_goal_is_an_error_not_a_panic() {
    assert_eq!(
        plan_source(SRC, "app"),
        Err(PlanError::Graph(GraphError::Cycle(vec![
            "codec".into(),
            "lib".into(),
            "util".into(),
        ])))
    );
}

#[test]
fn unrelated_goal_still_plans() {
    let p = plan_source(SRC, "tool").unwrap();
    assert_eq!(p.order, ["tool"]);
}

#[test]
fn error_message_names_the_cycle() {
    let err = plan_source(SRC, "cli").unwrap_err();
    assert_eq!(
        err.to_string(),
        "graph error: dependency cycle: codec -> lib -> util -> codec"
    );
}
