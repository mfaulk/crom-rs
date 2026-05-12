# Approvals

The SDK never prompts a terminal. Any operation that would require user
approval routes through the host-supplied `ApprovalCallback`.

## Approval policies

| Policy                     | Behavior                                                            |
| -------------------------- | ------------------------------------------------------------------- |
| `CallbackRequired`         | Default. Consult the callback. Deny if no callback configured.      |
| `NeverAskDeny`             | Always deny without calling the callback.                           |
| `NeverAskAllowSafe`        | Allow read-only ops, deny everything else.                          |
| `AutoApproveReadOnly`      | Auto-approve `PermissionKind::ReadOnly`, callback otherwise.        |
| `AutoApprovePlanScope`     | Auto-approve ops within an approved plan's scope.                   |

## Interactivity modes

| Mode              | Behavior                                                       |
| ----------------- | -------------------------------------------------------------- |
| `Interactive`     | Use the callback. Still no terminal.                           |
| `Headless`        | Use the callback if provided, deny otherwise.                  |
| `HeadlessDeny`    | Default. Deny without consulting the callback.                 |
| `HeadlessPlanOnly`| No side effects; only plan/review.                             |

## Writing a callback

```rust,no_run
use async_trait::async_trait;
use code_agent_sdk::{
    AgentSdkError, ApprovalCallback, ApprovalDecision, ApprovalRequest,
};

struct ReadOnlyApprover;

#[async_trait]
impl ApprovalCallback for ReadOnlyApprover {
    async fn approve(&self, request: ApprovalRequest) -> Result<ApprovalDecision, AgentSdkError> {
        if request.tool_name == "Read" {
            Ok(ApprovalDecision::Allow)
        } else {
            Ok(ApprovalDecision::deny("only reads allowed"))
        }
    }
}
```

The runtime emits `SdkEvent::ApprovalRequired` before the callback fires
and `SdkEvent::ApprovalResolved` afterwards so applications can audit every
decision.
