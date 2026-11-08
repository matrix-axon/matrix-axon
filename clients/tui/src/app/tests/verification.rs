//! Interactive-verification flow state: SAS frames, stage transitions, and
//! binding a server frame to a pending outgoing request.

use std::time::{Duration, Instant};

use super::support::*;
use crate::api::{AxonClient, DeviceListDto, DeviceListEntryDto, HTTP_REQUEST_TIMEOUT};
use crate::app::*;
use crate::ui::device_picker_lines;
use crossterm::event::{KeyEventKind, KeyEventState};
use unicode_width::UnicodeWidthChar;

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

const PICKER_ACCOUNT: u128 = 7;

fn enable_verification_send(app: &mut App) {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_lifecycle_sender(tx);
}

/// The picker refuses to open with no lifecycle sender, because that GET
/// cannot report back. Point the client at a closed port so the spawned list
/// fails immediately and its outcome is dropped with the receiver.
fn allow_picker_fetch(app: &mut App) {
    app.client = AxonClient::new("http://127.0.0.1:1".to_owned(), None);
    enable_verification_send(app);
}

fn signed_in_app(device_id: Option<&str>) -> App {
    let mut app = app_with_rooms(Vec::new());
    let mut account = account_with_id(
        Uuid::from_u128(PICKER_ACCOUNT),
        "@me:example.com",
        AccountState::Active,
    );
    account.device_id = device_id.map(str::to_owned);
    app.set_accounts(vec![account]);
    app
}

fn device(id: &str, name: Option<&str>, verified: bool) -> DeviceListEntryDto {
    DeviceListEntryDto {
        device_id: id.to_owned(),
        display_name: name.map(str::to_owned),
        is_verified: verified,
    }
}

fn device_list(devices: Vec<DeviceListEntryDto>) -> DeviceListDto {
    DeviceListDto {
        user_id: "@me:example.com".to_owned(),
        devices,
    }
}

fn sample_devices() -> Vec<DeviceListEntryDto> {
    vec![
        device("SELF", Some("this session"), true),
        device("BBB", Some("phone"), false),
        device("AAA", Some("Laptop"), true),
        device("DDD", None, false),
        device("CCC", Some("  "), false),
        device("EEE", Some("Phone"), false),
    ]
}

async fn apply_devices(app: &mut App, generation: u64, devices: Vec<DeviceListEntryDto>) {
    app.handle_lifecycle_outcome(LifecycleOutcome::DevicesListed {
        account_id: Uuid::from_u128(PICKER_ACCOUNT),
        generation,
        result: Ok(device_list(devices)),
    })
    .await;
}

fn picker_generation(app: &App) -> u64 {
    app.device_picker
        .as_ref()
        .expect("device picker open")
        .generation
}

fn row_ids(app: &App) -> Vec<&str> {
    app.device_picker
        .as_ref()
        .expect("device picker open")
        .rows
        .iter()
        .map(|row| row.device_id.as_str())
        .collect()
}

fn line_columns(text: &str) -> usize {
    text.chars().map(|ch| ch.width_cjk().unwrap_or(1)).sum()
}

fn line_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn device_picker_rows_drop_own_device_and_sort() {
    let rows = device_picker_rows(&sample_devices(), "SELF");
    assert_eq!(
        rows.iter()
            .map(|row| row.device_id.as_str())
            .collect::<Vec<_>>(),
        vec!["AAA", "BBB", "EEE", "CCC", "DDD"]
    );
    assert_eq!(rows[0].label, "Laptop");
    assert!(rows[0].is_verified);
    assert_eq!(rows[1].label, "phone");
    assert_eq!(rows[2].label, "Phone");
    assert_eq!(rows[3].label, "Unnamed device");
    assert_eq!(rows[4].label, "Unnamed device");

    let dirty = device_picker_rows(
        &[
            device("ZZZ", Some("\n\u{1b}[31m\u{202e}phone\t"), false),
            device("YYY", Some(" \n\u{202e}\t"), false),
        ],
        "SELF",
    );
    assert_eq!(dirty[0].label, "[31mphone");
    assert_eq!(dirty[1].label, "Unnamed device");
    assert!(!dirty[0].label.chars().any(char::is_control));
    assert!(!dirty[0].label.contains('\u{202e}'));
}

#[tokio::test]
async fn bare_verify_opens_the_device_picker() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;

    assert_eq!(app.mode, Mode::Popup(PopupKind::DevicePicker));
    assert!(app.prompt_owns_entry_line());
    assert_eq!(app.status.text(false), DEVICE_PICKER_LOADING);
    let picker = app.device_picker.as_ref().expect("picker");
    assert_eq!(picker.account_id, Uuid::from_u128(PICKER_ACCOUNT));
    assert_eq!(picker.generation, 1);
    assert_eq!(picker.phase, DevicePickerPhase::Loading);
    assert!(picker.rows.is_empty());
}

#[tokio::test]
async fn bare_verify_refuses_without_one_account() {
    let mut app = app_with_rooms(Vec::new());
    app.handle_command(Command::Verify(None)).await;
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.device_picker.is_none());
    assert!(app.status.text(false).contains("no active account"));

    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        account_with_id(Uuid::from_u128(1), "@a:example.com", AccountState::Active),
        account_with_id(Uuid::from_u128(2), "@b:example.com", AccountState::Active),
    ]);
    app.handle_command(Command::Verify(None)).await;
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.device_picker.is_none());
    assert!(app.status.text(false).contains("select an account"));
}

#[tokio::test]
async fn explicit_verify_targets_skip_the_picker() {
    let mut app = signed_in_app(Some("SELF"));
    enable_verification_send(&mut app);
    app.handle_command(Command::Verify(Some("ABCDEF".to_owned())))
        .await;
    assert_eq!(app.mode, Mode::Verification);
    assert!(app.device_picker.is_none());
    let flow = app.verification.as_ref().expect("flow");
    assert_eq!(flow.device_id, "ABCDEF");
    assert_eq!(flow.direction, VerificationDirection::Outgoing);
    assert_eq!(flow.stage, VerificationStage::Starting);

    let mut app = signed_in_app(Some("SELF"));
    enable_verification_send(&mut app);
    app.handle_command(Command::Verify(Some("@bob:example.org".to_owned())))
        .await;
    assert_eq!(app.mode, Mode::Verification);
    assert!(app.device_picker.is_none());
    let flow = app.verification.as_ref().expect("flow");
    assert_eq!(flow.user_id, "@bob:example.org");
    assert!(flow.device_id.is_empty());
}

#[tokio::test]
async fn device_list_fills_the_open_picker_and_ignores_stale_results() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);

    app.handle_lifecycle_outcome(LifecycleOutcome::DevicesListed {
        account_id: Uuid::from_u128(PICKER_ACCOUNT),
        generation: generation.wrapping_sub(1),
        result: Ok(device_list(sample_devices())),
    })
    .await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );

    app.handle_lifecycle_outcome(LifecycleOutcome::DevicesListed {
        account_id: Uuid::nil(),
        generation,
        result: Ok(device_list(sample_devices())),
    })
    .await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );

    apply_devices(&mut app, generation, sample_devices()).await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Ready
    );
    assert_eq!(row_ids(&app), vec!["AAA", "BBB", "EEE", "CCC", "DDD"]);
    assert_eq!(app.status.text(false), "Select a device to verify");

    app.mode = Mode::Compose;
    apply_devices(
        &mut app,
        generation,
        vec![device("ZZZ", Some("late"), false)],
    )
    .await;
    assert!(app.device_picker.is_none());

    app.close_device_picker();
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.device_picker.is_none());
    apply_devices(&mut app, generation, sample_devices()).await;
    assert!(app.device_picker.is_none());
    assert_eq!(app.mode, Mode::Compose);

    app.handle_command(Command::Verify(None)).await;
    assert_eq!(picker_generation(&app), generation + 1);
    apply_devices(&mut app, generation, sample_devices()).await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );
}

#[tokio::test]
async fn failed_device_list_stays_open_until_retry() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    app.handle_lifecycle_outcome(LifecycleOutcome::DevicesListed {
        account_id: Uuid::from_u128(PICKER_ACCOUNT),
        generation,
        result: Err("HTTP 500: devices unavailable".to_owned()),
    })
    .await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::DevicePicker));
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Failed("HTTP 500: devices unavailable".to_owned())
    );
    assert_eq!(app.status.text(false), DEVICE_PICKER_FAILED);

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );
    assert_ne!(picker_generation(&app), generation);
    apply_devices(&mut app, generation, sample_devices()).await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );
    let current = picker_generation(&app);
    apply_devices(&mut app, current, sample_devices()).await;
    assert_eq!(row_ids(&app), vec!["AAA", "BBB", "EEE", "CCC", "DDD"]);
}

#[tokio::test]
async fn enter_verifies_the_selected_device_and_p_returns_to_paste() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
        .await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::DevicePicker));

    let generation = picker_generation(&app);
    apply_devices(&mut app, generation, sample_devices()).await;
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()))
        .await;
    assert_eq!(app.device_picker.as_ref().unwrap().selection, 1);
    enable_verification_send(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
        .await;
    assert_eq!(app.mode, Mode::Verification);
    assert!(app.device_picker.is_none());
    assert_eq!(app.verification.as_ref().unwrap().device_id, "BBB");

    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::empty()))
        .await;
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.device_picker.is_none());
    assert_eq!(app.input.buffer, "/verify ");
    assert_eq!(app.input.cursor, "/verify ".len());
    assert!(!app.prompt_owns_entry_line());
}

#[tokio::test]
async fn esc_closes_the_picker_and_an_incoming_request_replaces_it() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
        .await;
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.status.text(false).is_empty());
    apply_devices(&mut app, generation, sample_devices()).await;
    assert_ne!(app.mode, Mode::Popup(PopupKind::DevicePicker));

    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    app.open_incoming_verification(
        Uuid::from_u128(PICKER_ACCOUNT),
        "flow-in".to_owned(),
        "@me:example.com".to_owned(),
        "OTHER".to_owned(),
    );
    assert_eq!(app.mode, Mode::Verification);
    assert!(app.device_picker.is_none());
    apply_devices(&mut app, generation, sample_devices()).await;
    assert_eq!(app.mode, Mode::Verification);
    assert_eq!(
        app.verification.as_ref().unwrap().flow_id.as_deref(),
        Some("flow-in")
    );
    assert!(app.device_picker.is_none());
}

#[test]
fn device_picker_lines_mark_the_selection_and_fit_the_width() {
    let mut app = signed_in_app(Some("SELF"));
    app.device_picker = Some(DevicePicker {
        account_id: Uuid::from_u128(PICKER_ACCOUNT),
        generation: 1,
        loading_since: Instant::now(),
        refreshing: false,
        phase: DevicePickerPhase::Ready,
        selection: 1,
        rows: vec![
            DevicePickerRow {
                device_id: "ABCDEFGHIJKLMNOP".to_owned(),
                label: "電話電話電話".to_owned(),
                is_verified: true,
            },
            DevicePickerRow {
                device_id: "SHORT".to_owned(),
                label: "phone".to_owned(),
                is_verified: false,
            },
        ],
    });
    // 16 columns cannot hold a device id plus the trust word.
    // The trust word stays; the id is what gets cut.
    let narrow = 16;
    let narrow_lines = device_picker_lines(&app, narrow);
    assert!(narrow_lines.len() >= 2);
    let narrow_texts: Vec<String> = narrow_lines.iter().map(line_text).collect();
    for text in &narrow_texts {
        assert!(
            line_columns(text) <= narrow,
            "{text} is {} columns",
            line_columns(text)
        );
    }
    assert!(narrow_texts[1].starts_with('>'));
    assert!(narrow_texts[0].starts_with(' '));
    assert!(narrow_texts.iter().all(|text| text.contains("verified")));

    // Wide enough for a truncated CJK label, the device id, and the trust word.
    let wide = 40;
    let wide_lines = device_picker_lines(&app, wide);
    let wide_texts: Vec<String> = wide_lines.iter().map(line_text).collect();
    for text in &wide_texts {
        assert!(
            line_columns(text) <= wide,
            "{text} is {} columns",
            line_columns(text)
        );
    }
    assert!(wide_texts[0].contains("verified"));
    assert!(wide_texts[0].contains("..."));
    assert!(!wide_texts[0].contains("電話電話電話"));
    assert!(wide_texts[1].contains("not verified"));
    assert!(wide_texts[1].starts_with('>'));

    // A few columns cannot hold the trust word. The marker stays at the head.
    let tiny = 3;
    let tiny_lines = device_picker_lines(&app, tiny);
    let tiny_texts: Vec<String> = tiny_lines.iter().map(line_text).collect();
    for text in &tiny_texts {
        assert!(
            line_columns(text) <= tiny,
            "{text} is {} columns",
            line_columns(text)
        );
    }
    assert!(tiny_texts[1].starts_with('>'));
    assert!(tiny_texts[0].starts_with(' '));

    app.device_picker.as_mut().unwrap().phase = DevicePickerPhase::Ready;
    app.device_picker.as_mut().unwrap().rows.clear();
    let empty = device_picker_lines(&app, narrow);
    let empty_text = empty.iter().map(line_text).collect::<Vec<_>>().join(" ");
    assert!(empty_text.contains("No other devices"));
    for line in &empty {
        assert!(line_columns(&line_text(line)) <= narrow);
    }
}

#[tokio::test]
async fn device_picker_hides_the_list_until_this_session_is_known() {
    for unknown in [None, Some("")] {
        let mut app = signed_in_app(unknown);
        allow_picker_fetch(&mut app);
        app.handle_command(Command::Verify(None)).await;
        assert_eq!(app.mode, Mode::Popup(PopupKind::DevicePicker));
        assert_eq!(
            app.device_picker.as_ref().unwrap().phase,
            DevicePickerPhase::UnknownSelf
        );
        assert!(app.device_picker.as_ref().unwrap().rows.is_empty());
        assert_eq!(app.status.text(false), DEVICE_PICKER_UNKNOWN_SELF);
        let lines = device_picker_lines(&app, 24);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join(" ");
        assert!(text.contains("not known yet"));
        for line in &lines {
            assert!(line_columns(&line_text(line)) <= 24);
        }

        let generation = picker_generation(&app);
        apply_devices(&mut app, generation, sample_devices()).await;
        assert_eq!(
            app.device_picker.as_ref().unwrap().phase,
            DevicePickerPhase::UnknownSelf
        );
        assert!(app.device_picker.as_ref().unwrap().rows.is_empty());

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
            .await;
        assert_eq!(
            app.device_picker.as_ref().unwrap().phase,
            DevicePickerPhase::UnknownSelf
        );
    }

    let mut app = signed_in_app(None);
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let before = picker_generation(&app);
    let mut account = app.accounts.client_visible[0].clone();
    account.device_id = Some("SELF".to_owned());
    app.set_accounts(vec![account]);
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );
    assert_ne!(picker_generation(&app), before);
    let generation = picker_generation(&app);
    apply_devices(&mut app, generation, sample_devices()).await;
    assert_eq!(row_ids(&app), vec!["AAA", "BBB", "EEE", "CCC", "DDD"]);
}

#[tokio::test]
async fn retry_during_loading_does_not_start_another_list() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT))
        .await;
    assert_eq!(picker_generation(&app), generation);
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );
}

#[tokio::test]
async fn enter_without_a_sender_keeps_the_device_list() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    apply_devices(&mut app, generation, sample_devices()).await;
    app.lifecycle_tx = None;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
        .await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::DevicePicker));
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Ready
    );
    assert_eq!(row_ids(&app), vec!["AAA", "BBB", "EEE", "CCC", "DDD"]);
    assert_eq!(app.status.text(false), "Select a device to verify");
}

#[tokio::test]
async fn enter_verifies_the_account_the_list_was_opened_for() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    apply_devices(&mut app, generation, sample_devices()).await;
    app.set_accounts(vec![
        account_with_id(
            Uuid::from_u128(PICKER_ACCOUNT),
            "@me:example.com",
            AccountState::Active,
        ),
        account_with_id(
            Uuid::from_u128(8),
            "@other:example.com",
            AccountState::Active,
        ),
    ]);
    app.accounts.selected = AccountSelection::Account(1);
    assert_eq!(app.active_account_filter(), Some(Uuid::from_u128(8)));
    enable_verification_send(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
        .await;
    assert_eq!(app.mode, Mode::Verification);
    assert!(app.device_picker.is_none());
    let flow = app.verification.as_ref().expect("flow");
    assert_eq!(flow.account_id, Uuid::from_u128(PICKER_ACCOUNT));
    assert_eq!(flow.device_id, "AAA");
}

#[tokio::test]
async fn explicit_verify_still_uses_the_current_account_filter() {
    let mut app = signed_in_app(Some("SELF"));
    app.set_accounts(vec![
        account_with_id(
            Uuid::from_u128(PICKER_ACCOUNT),
            "@me:example.com",
            AccountState::Active,
        ),
        account_with_id(
            Uuid::from_u128(8),
            "@other:example.com",
            AccountState::Active,
        ),
    ]);
    app.accounts.selected = AccountSelection::All;
    enable_verification_send(&mut app);
    app.handle_command(Command::Verify(Some("ABCDEF".to_owned())))
        .await;
    assert_ne!(app.mode, Mode::Verification);
    assert!(app.device_picker.is_none());
    assert!(app.status.text(false).contains("select an account"));
}

#[tokio::test]
async fn retry_after_the_http_timeout_starts_another_list() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    let bound = HTTP_REQUEST_TIMEOUT + Duration::from_secs(1);
    app.device_picker.as_mut().unwrap().loading_since =
        Instant::now().checked_sub(bound).expect("bound fits");
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    assert_ne!(picker_generation(&app), generation);
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );
}

#[tokio::test]
async fn device_picker_letters_ignore_shift_and_honor_lock_state() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    apply_devices(&mut app, generation, sample_devices()).await;
    for key in [
        KeyEvent::new(KeyCode::Char('P'), KeyModifiers::empty()),
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Char('R'), KeyModifiers::empty()),
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT),
    ] {
        app.handle_key(key).await;
    }
    assert_eq!(app.mode, Mode::Popup(PopupKind::DevicePicker));
    assert_eq!(picker_generation(&app), generation);
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Ready
    );
    assert_eq!(row_ids(&app), vec!["AAA", "BBB", "EEE", "CCC", "DDD"]);

    app.handle_key(KeyEvent::new_with_kind_and_state(
        KeyCode::Char('r'),
        KeyModifiers::empty(),
        KeyEventKind::Press,
        KeyEventState::CAPS_LOCK | KeyEventState::NUM_LOCK,
    ))
    .await;
    assert_ne!(picker_generation(&app), generation);
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Ready
    );
    assert!(app.device_picker.as_ref().unwrap().refreshing);
    assert_eq!(row_ids(&app), vec!["AAA", "BBB", "EEE", "CCC", "DDD"]);

    app.handle_key(KeyEvent::new_with_kind_and_state(
        KeyCode::Char('p'),
        KeyModifiers::empty(),
        KeyEventKind::Press,
        KeyEventState::NUM_LOCK,
    ))
    .await;
    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.input.buffer, "/verify ");
}

#[tokio::test]
async fn leaving_the_picker_by_message_navigation_drops_it() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    assert!(app.device_picker.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(app.mode, Mode::MessageList);
    assert!(app.device_picker.is_none());
}

#[tokio::test]
async fn bare_verify_without_a_sender_stays_closed() {
    let mut app = signed_in_app(Some("SELF"));
    app.handle_command(Command::Verify(None)).await;
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.device_picker.is_none());
    assert_eq!(app.status.text(false), VERIFICATION_NOT_RUNNING);
}

#[tokio::test]
async fn verify_does_not_replace_a_verification_in_progress() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    let mut flow = outgoing_flow();
    flow.account_id = Uuid::from_u128(PICKER_ACCOUNT);
    flow.stage = VerificationStage::Compare;
    app.verification = Some(flow);
    app.handle_command(Command::Verify(None)).await;
    assert!(app.device_picker.is_none());
    assert_eq!(app.status.text(false), VERIFICATION_IN_PROGRESS);
    assert_eq!(
        app.verification.as_ref().unwrap().stage,
        VerificationStage::Compare
    );

    app.handle_command(Command::Verify(Some("ABCDEF".to_owned())))
        .await;
    assert_eq!(app.verification.as_ref().unwrap().device_id, "DEV");
    assert_eq!(
        app.verification.as_ref().unwrap().stage,
        VerificationStage::Compare
    );

    app.verification.as_mut().unwrap().stage = VerificationStage::Done;
    app.handle_command(Command::Verify(Some("ABCDEF".to_owned())))
        .await;
    assert_eq!(app.mode, Mode::Verification);
    assert_eq!(app.verification.as_ref().unwrap().device_id, "ABCDEF");
}

#[tokio::test]
async fn enter_does_not_replace_a_verification_in_progress() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    apply_devices(&mut app, generation, sample_devices()).await;
    app.verification = Some(outgoing_flow());
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
        .await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::DevicePicker));
    assert!(app.device_picker.is_some());
    assert_eq!(app.verification.as_ref().unwrap().device_id, "DEV");
    assert_eq!(app.status.text(false), VERIFICATION_IN_PROGRESS);
}

#[tokio::test]
async fn failed_refresh_keeps_the_ready_list() {
    let mut app = signed_in_app(Some("SELF"));
    allow_picker_fetch(&mut app);
    app.handle_command(Command::Verify(None)).await;
    let generation = picker_generation(&app);
    apply_devices(&mut app, generation, sample_devices()).await;
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    let generation = picker_generation(&app);
    let picker = app.device_picker.as_ref().unwrap();
    assert_eq!(picker.phase, DevicePickerPhase::Ready);
    assert!(picker.refreshing);
    assert_eq!(picker.selection, 1);
    assert_eq!(row_ids(&app), vec!["AAA", "BBB", "EEE", "CCC", "DDD"]);
    assert_eq!(app.status.text(false), DEVICE_PICKER_REFRESHING);
    let text = device_picker_lines(&app, 40)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Refreshing"));

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    assert_eq!(picker_generation(&app), generation);

    app.handle_lifecycle_outcome(LifecycleOutcome::DevicesListed {
        account_id: Uuid::from_u128(PICKER_ACCOUNT),
        generation,
        result: Err("HTTP 500: devices unavailable".to_owned()),
    })
    .await;
    let picker = app.device_picker.as_ref().unwrap();
    assert_eq!(picker.phase, DevicePickerPhase::Ready);
    assert!(!picker.refreshing);
    assert_eq!(picker.selection, 1);
    assert_eq!(row_ids(&app), vec!["AAA", "BBB", "EEE", "CCC", "DDD"]);
    assert!(app
        .status
        .text(false)
        .contains("Could not refresh the device list"));

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    let generation = picker_generation(&app);
    apply_devices(
        &mut app,
        generation,
        vec![device("AAA", Some("Laptop"), true)],
    )
    .await;
    assert_eq!(row_ids(&app), vec!["AAA"]);
    assert_eq!(app.device_picker.as_ref().unwrap().selection, 0);
    assert!(!app.device_picker.as_ref().unwrap().refreshing);
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Ready
    );
}

#[tokio::test]
async fn unknown_self_r_rereads_the_account_list() {
    let mut app = signed_in_app(None);
    app.client = AxonClient::new("http://127.0.0.1:1".to_owned(), None);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_lifecycle_sender(tx);
    app.handle_command(Command::Verify(None)).await;
    let before = picker_generation(&app);
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    let generation = picker_generation(&app);
    assert_ne!(generation, before);
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::UnknownSelf
    );
    assert!(app.device_picker.as_ref().unwrap().refreshing);
    assert_eq!(app.status.text(false), DEVICE_PICKER_CHECKING_SELF);
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()))
        .await;
    assert_eq!(picker_generation(&app), generation);

    let outcome = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("account refresh timed out")
        .expect("account refresh outcome");
    app.handle_lifecycle_outcome(outcome).await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::UnknownSelf
    );
    assert!(!app.device_picker.as_ref().unwrap().refreshing);
    assert!(app
        .status
        .text(false)
        .contains("Could not refresh accounts"));

    let mut account = app.accounts.client_visible[0].clone();
    account.device_id = Some("SELF".to_owned());
    app.handle_lifecycle_outcome(LifecycleOutcome::DevicePickerAccounts {
        account_id: Uuid::from_u128(PICKER_ACCOUNT),
        generation: picker_generation(&app),
        result: Ok(vec![account]),
    })
    .await;
    assert_eq!(
        app.device_picker.as_ref().unwrap().phase,
        DevicePickerPhase::Loading
    );
    assert_eq!(
        app.accounts.client_visible[0].device_id.as_deref(),
        Some("SELF")
    );
}
