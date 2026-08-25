//! Interactive-verification flow state: SAS frames, stage transitions, and
//! binding a server frame to a pending outgoing request.

use super::support::*;
use crate::app::*;

fn outgoing_flow() -> VerificationFlow {
    VerificationFlow {
        account_id: Uuid::nil(),
        user_id: "@self:example.com".to_owned(),
        device_id: "DEV".to_owned(),
        flow_id: Some("txn1".to_owned()),
        direction: VerificationDirection::Outgoing,
        stage: VerificationStage::Waiting,
        emoji: None,
        decimals: None,
    }
}

fn sas_frame() -> VerificationFrameDto {
    VerificationFrameDto {
        flow_id: "txn1".to_owned(),
        user_id: "@self:example.com".to_owned(),
        device_id: Some("DEV".to_owned()),
        emoji: Some(vec![EmojiDto {
            symbol: "🐶".to_owned(),
            description: "Dog".to_owned(),
        }]),
        decimals: Some([1, 2, 3]),
        reason: None,
    }
}

fn flow_dto(flow_id: &str, user_id: &str, device_id: Option<&str>) -> FlowDto {
    FlowDto {
        flow_id: flow_id.to_owned(),
        user_id: user_id.to_owned(),
        device_id: device_id.map(str::to_owned),
        stage: FlowStage::Requested,
        emoji: None,
        decimals: None,
        cancel_reason: None,
    }
}

#[test]
fn verification_sas_frame_moves_to_compare() {
    let mut flow = outgoing_flow();
    flow.apply_frame(VerificationFrameKind::Sas, &sas_frame());
    assert_eq!(flow.stage, VerificationStage::Compare);
    assert_eq!(flow.decimals, Some([1, 2, 3]));
    assert_eq!(flow.emoji.as_ref().unwrap()[0].symbol, "🐶");
}

#[test]
fn verification_done_and_cancel_are_terminal() {
    let mut flow = outgoing_flow();
    flow.apply_frame(VerificationFrameKind::Done, &sas_frame());
    assert_eq!(flow.stage, VerificationStage::Done);
    assert!(flow.stage.is_terminal());

    let mut flow = outgoing_flow();
    let cancel = VerificationFrameDto {
        reason: Some("user".to_owned()),
        ..sas_frame()
    };
    flow.apply_frame(VerificationFrameKind::Cancelled, &cancel);
    assert!(matches!(flow.stage, VerificationStage::Ended(_)));
}

#[test]
fn verification_confirming_not_regressed_by_late_sas() {
    // After the user confirms, a trailing SAS frame must not pull the modal
    // back to the compare prompt.
    let mut flow = outgoing_flow();
    flow.stage = VerificationStage::Confirming;
    flow.apply_frame(VerificationFrameKind::Sas, &sas_frame());
    assert_eq!(flow.stage, VerificationStage::Confirming);
}

#[test]
fn pending_outgoing_requested_frame_is_not_treated_as_unsolicited() {
    let mut app = app_with_rooms(Vec::new());
    app.display.accept_incoming_verification = false;
    app.accounts.accounts = vec![account_with_id(
        Uuid::nil(),
        "@alice:example.com",
        AccountState::Active,
    )];
    app.verification = Some(VerificationFlow {
        account_id: Uuid::nil(),
        user_id: "@bob:example.com".to_owned(),
        device_id: String::new(),
        flow_id: None,
        direction: VerificationDirection::Outgoing,
        stage: VerificationStage::Starting,
        emoji: None,
        decimals: None,
    });

    let action = app.handle_live_frame(LiveFrame::Verification(VerificationFrame {
        account_id: Uuid::nil(),
        kind: VerificationFrameKind::Requested,
        payload: VerificationFrameDto {
            flow_id: "server-flow".to_owned(),
            user_id: "@bob:example.com".to_owned(),
            device_id: None,
            emoji: None,
            decimals: None,
            reason: None,
        },
    }));

    assert_eq!(action, LiveFrameAction::None);
    let flow = app.verification.as_ref().unwrap();
    assert_eq!(flow.direction, VerificationDirection::Outgoing);
    assert_eq!(flow.flow_id, None);
    assert_eq!(flow.stage, VerificationStage::Starting);
}

#[test]
fn same_user_frame_does_not_bind_pending_outgoing_without_device_match() {
    let mut app = app_with_rooms(Vec::new());
    app.verification = Some(VerificationFlow {
        account_id: Uuid::nil(),
        user_id: "@bob:example.com".to_owned(),
        device_id: String::new(),
        flow_id: None,
        direction: VerificationDirection::Outgoing,
        stage: VerificationStage::Starting,
        emoji: None,
        decimals: None,
    });

    let action = app.handle_live_frame(LiveFrame::Verification(VerificationFrame {
        account_id: Uuid::nil(),
        kind: VerificationFrameKind::Sas,
        payload: VerificationFrameDto {
            flow_id: "other-flow".to_owned(),
            user_id: "@bob:example.com".to_owned(),
            device_id: None,
            emoji: Some(vec![EmojiDto {
                symbol: "🐶".to_owned(),
                description: "Dog".to_owned(),
            }]),
            decimals: Some([1, 2, 3]),
            reason: None,
        },
    }));

    assert_eq!(action, LiveFrameAction::None);
    let flow = app.verification.as_ref().unwrap();
    assert_eq!(flow.flow_id, None);
    assert_eq!(flow.emoji, None);
    assert_eq!(flow.decimals, None);
}

#[tokio::test]
async fn discovered_cross_user_request_honors_incoming_suppression() {
    let mut app = app_with_rooms(Vec::new());
    app.display.accept_incoming_verification = false;
    app.accounts.accounts = vec![account_with_id(
        Uuid::nil(),
        "@alice:example.com",
        AccountState::Active,
    )];

    app.handle_lifecycle_outcome(LifecycleOutcome::VerifyDiscovered {
        account_id: Uuid::nil(),
        result: Ok(vec![flow_dto("flow1", "@bob:example.com", None)]),
    })
    .await;

    assert!(app.verification.is_none());
    assert_ne!(app.mode, Mode::Verification);
}

#[test]
fn verification_apply_flow_maps_server_stage() {
    let mut flow = outgoing_flow();
    flow.apply_flow(&FlowDto {
        flow_id: "txn1".to_owned(),
        user_id: "@self:example.com".to_owned(),
        device_id: Some("DEV".to_owned()),
        stage: FlowStage::KeysExchanged,
        emoji: Some(vec![EmojiDto {
            symbol: "🐱".to_owned(),
            description: "Cat".to_owned(),
        }]),
        decimals: Some([4, 5, 6]),
        cancel_reason: None,
    });
    assert_eq!(flow.stage, VerificationStage::Compare);
    assert!(flow.matches(Uuid::nil(), "txn1"));
    assert!(!flow.matches(Uuid::nil(), "other"));
}
