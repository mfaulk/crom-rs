# Plan / act API

The SDK exposes the M11 planning subsystem as two methods:

- `AgentRuntime::plan(SdkPlanRequest)` — produces a `PlanDocument`.
- `AgentRuntime::act(ActRequest)` — executes an approved plan.

For one-shot use, set `AgentRequest::mode(AgentMode::PlanThenAct)` and call
`run_once`. The SDK plans first, auto-approves the plan internally (in
test/integration flows), then acts.

## Plan-only

```rust,no_run
use code_agent_sdk::{AgentRuntime, ProviderConfig, SdkPlanRequest};
use code_agent_sdk::fake_provider::EchoFakeProvider;
use std::sync::Arc;

# async fn run() -> anyhow::Result<()> {
let runtime = AgentRuntime::builder()
    .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
    .build()
    .await?;
let plan = runtime
    .plan(SdkPlanRequest::new("Add a CHANGELOG"))
    .await?;
println!("plan {} has {} steps", plan.plan.id, plan.plan.steps.len());
# Ok(())
# }
```

Plan-only never mutates the workspace.

## Act

`ActRequest::from_plan(plan)` accepts a `PlanDocument`. `test_force_approval()`
auto-approves the plan internally and is intended for tests and library
integrations that own the entire flow.

In production, run plans through your application's approval flow before
calling `act`. `ApprovalPolicy::CallbackRequired` plus a host approval
callback closes the loop.

## Deviations

Plan deviations during act show up as `Diagnostic`s in the result and as
`SdkEvent::Diagnostic` events on the stream. Default policy denies all
deviations; supply a `PlanDeviationCallback` through the underlying
core API if you need fine-grained control.
