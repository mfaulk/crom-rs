//! Phase G: live wiring between the SDK's [`ApprovalCallback`] and the
//! TUI's reducer-driven approval flow.
//!
//! The SDK calls the host's approval callback whenever the policy
//! engine needs a human decision. Before Phase G the TUI only logged
//! the resolution to the activity panel — a real approval would hang
//! the SDK forever because nothing was unblocking the callback.
//!
//! `ApprovalBridge` solves that. It implements the SDK trait, forwards
//! the request to the [`TuiApp`](crate::app::TuiApp) event loop over an
//! unbounded channel, and awaits a `oneshot` reply that the app sends
//! once the user resolves the modal. The reducer remains pure — only
//! the host loop deals with the channels.
//!
//! TUI flow:
//! 1. SDK runtime calls `bridge.approve(request)`.
//! 2. Bridge creates a `oneshot` channel, forwards
//!    `(request, decision_tx)` over the mpsc to the app loop, and
//!    awaits `decision_rx`.
//! 3. App loop receives the [`BridgeRequest`] in its `select!`, stashes
//!    the resolver, and the reducer renders the approval modal
//!    (driven by the SDK's matching `ApprovalRequired` event).
//! 4. User presses `y`/`s`/`n`/`r`; reducer emits
//!    [`TuiAction::ResolveApproval`](crate::state::TuiAction::ResolveApproval).
//! 5. App handler maps that to an SDK [`ApprovalDecision`] and sends
//!    it through the stashed `oneshot`. Bridge returns to the SDK.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crom_sdk::callback::{ApprovalCallback, ApprovalDecision, ApprovalRequest};
use crom_sdk::error::{AgentSdkError, AgentSdkErrorKind};

/// Forwarded approval payload from the bridge to the app loop. The
/// `decision_tx` resolves the still-awaiting SDK callback when the
/// user makes a choice; dropping it without sending denies the
/// approval (the bridge maps a `RecvError` to a `Deny`).
#[derive(Debug)]
pub struct BridgeRequest {
    pub request: ApprovalRequest,
    pub decision_tx: oneshot::Sender<ApprovalDecision>,
}

/// SDK-side [`ApprovalCallback`]. Construct one with
/// [`new_bridge_pair`] and pass the `Arc<ApprovalBridge>` to
/// `AgentRuntimeBuilder::approval_callback`; pass the
/// `Receiver<BridgeRequest>` to
/// [`TuiApp::with_approval_bridge`](crate::app::TuiApp::with_approval_bridge).
///
/// The bridge uses a bounded channel with capacity 1 to enforce
/// single-flight semantics: if a second approval is requested while
/// one is already pending, the SDK side back-pressures (its
/// `approve()` await blocks on the channel send) instead of stacking
/// up approvals that the single TUI modal cannot service.
#[derive(Debug, Clone)]
pub struct ApprovalBridge {
    tx: mpsc::Sender<BridgeRequest>,
}

#[async_trait]
impl ApprovalCallback for ApprovalBridge {
    async fn approve(&self, request: ApprovalRequest) -> Result<ApprovalDecision, AgentSdkError> {
        let (decision_tx, decision_rx) = oneshot::channel();
        let req = BridgeRequest {
            request,
            decision_tx,
        };
        if self.tx.send(req).await.is_err() {
            return Err(AgentSdkError::new(
                AgentSdkErrorKind::Internal,
                "approval bridge: receiver dropped before approval could be requested",
            ));
        }
        match decision_rx.await {
            Ok(d) => Ok(d),
            Err(_) => Ok(ApprovalDecision::Deny {
                reason: "approval canceled (TUI exited)".to_string(),
            }),
        }
    }
}

/// Construct a paired bridge. Move the `Arc<ApprovalBridge>` into the
/// SDK runtime builder and the receiver into the TUI app. The
/// channel capacity is 1 so the SDK back-pressures on the rare case
/// of a second concurrent approval request rather than stacking.
pub fn new_bridge_pair() -> (Arc<ApprovalBridge>, mpsc::Receiver<BridgeRequest>) {
    let (tx, rx) = mpsc::channel(1);
    (Arc::new(ApprovalBridge { tx }), rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_core::permission::PermissionKind;
    use crom_sdk::policy::SideEffectLevel;
    use crom_sdk::RunId;

    fn fake_request() -> ApprovalRequest {
        ApprovalRequest {
            tool_name: "Bash".to_string(),
            permission_kind: PermissionKind::Execution,
            side_effect_level: SideEffectLevel::CommandExecution,
            summary: "rm -rf target".to_string(),
            input: serde_json::json!({"cmd": "rm -rf target"}),
            run_id: RunId::new(),
            session_id: None,
            request_id: Some("req-1".to_string()),
            tenant_id: None,
            user_id: None,
        }
    }

    // Calling `approve` forwards the request through the channel and
    // returns the decision the app loop sends back.
    #[tokio::test]
    async fn approve_round_trip() {
        let (bridge, mut rx) = new_bridge_pair();
        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move { bridge_clone.approve(fake_request()).await });
        let req = rx.recv().await.expect("bridge request");
        req.decision_tx
            .send(ApprovalDecision::Allow)
            .expect("send decision");
        let decision = handle.await.expect("join").expect("decision");
        assert_eq!(decision, ApprovalDecision::Allow);
    }

    // If the app drops the decision sender (TUI exited mid-approval),
    // the SDK sees a Deny rather than hanging or panicking.
    #[tokio::test]
    async fn dropped_decision_denies() {
        let (bridge, mut rx) = new_bridge_pair();
        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move { bridge_clone.approve(fake_request()).await });
        let req = rx.recv().await.expect("bridge request");
        drop(req.decision_tx);
        let decision = handle.await.expect("join").expect("decision");
        match decision {
            ApprovalDecision::Deny { reason } => {
                assert!(reason.contains("canceled"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // If the receiver is dropped before the bridge forwards (host
    // never wired in), `approve` returns an SDK error instead of
    // panicking.
    #[tokio::test]
    async fn dropped_receiver_errors() {
        let (bridge, rx) = new_bridge_pair();
        drop(rx);
        let res = bridge.approve(fake_request()).await;
        assert!(res.is_err());
    }
}
