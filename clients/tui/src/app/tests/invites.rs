//! Pending-invite inbox: list merge, popup, accept, and reject.

use std::time::{Duration, Instant};

use super::support::*;
use crate::api::{ApiError, InviteDto, InviteListSnapshot, LiveFrame};
use crate::app::invites::InviteVerb;
use crate::app::room_actions::{
    PendingMembership, RoomActionFailure, PENDING_MEMBERSHIP_CHECK_DELAYS,
    PENDING_MEMBERSHIP_FINAL_GRACE,
};
use crate::app::*;
use crate::command::{parse, HELP_COMMAND_GROUPS};
use crate::config::KeyBinding;
use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use uuid::Uuid;

fn invite_row(account_id: Uuid, room_id: &str, name: Option<&str>) -> InviteDto {
    InviteDto {
        account_id,
        account_user_id: "@alice:example.com".to_owned(),
        room_id: room_id.to_owned(),
        inviter_user_id: "@bob:example.com".to_owned(),
        is_direct: false,
        encrypted: false,
        invited_at: "2026-01-02T03:04:05Z".to_owned(),
        name: name.map(str::to_owned),
        avatar_url: None,
        topic: None,
        canonical_alias: None,
        room_type: None,
        inviter_display_name: Some("Bob".to_owned()),
    }
}

fn fresh_app() -> App {
    let mut app = App::new(
        AxonClient::new("http://127.0.0.1:1".to_owned(), None),
        None,
        TuiConfig::test_default(),
        ratatui_image::picker::Picker::halfblocks(),
    );
    app.bootstrap = BootstrapStage::Done;
    app.show_input_help = false;
    app.status = Status::Info(String::new());
    app
}

fn key_of(invite: &InviteDto) -> RoomKey {
    RoomKey::from(invite)
}

fn snapshot(invites: Vec<InviteDto>, started: Instant, not_found: bool) -> InviteListSnapshot {
    InviteListSnapshot {
        invites,
        started,
        not_found,
    }
}

fn pending_hold(title: &str, since: Instant) -> PendingMembership {
    PendingMembership {
        room_title: title.to_owned(),
        since,
        step: 0,
        next_check: since + PENDING_MEMBERSHIP_CHECK_DELAYS[0],
        retryable: false,
        seen_listed: false,
    }
}

fn failure(message: &str, timed_out: bool) -> RoomActionFailure {
    RoomActionFailure {
        message: message.to_owned(),
        timed_out,
    }
}

fn status_text(app: &App) -> String {
    app.status.text(false)
}

fn line_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

async fn outcome_channel(app: &mut App) -> mpsc::UnboundedReceiver<InviteActionOutcome> {
    let (tx, rx) = mpsc::unbounded_channel();
    app.set_invite_action_sender(tx);
    rx
}

#[test]
fn help_account_group_still_starts_at_login() {
    let start = HELP_COMMAND_GROUPS[2].0;
    assert!(
        HELP_COMMANDS[start].label.starts_with("/login"),
        "account management starts at {}",
        HELP_COMMANDS[start].label
    );
}

#[test]
fn pending_command_parses_and_leaves_invite_alone() {
    assert_eq!(parse("/pending"), Command::Invites);
    assert_eq!(parse("/pending   "), Command::Invites);
    assert_eq!(
        parse("/pending extra"),
        Command::Invalid("/pending takes no arguments".to_owned())
    );
    assert!(matches!(parse("/invites"), Command::Unknown(_)));
    assert_eq!(
        parse("/invite @bob:example.com"),
        Command::Invite("@bob:example.com".to_owned())
    );
    assert!(matches!(parse("/invite"), Command::Invalid(_)));
}

#[test]
fn typing_pending_completes_and_invite_completes_on_its_own_prefix() {
    let mut app = fresh_app();
    app.input.buffer = "/pen".to_owned();
    app.complete_input();
    assert_eq!(app.input.buffer, "/pending");
    assert_eq!(status_text(&app), "completed command: /pending");

    // `/inv` used to match both the inbox and the outbound invite.
    let mut app = fresh_app();
    app.input.buffer = "/inv".to_owned();
    app.complete_input();
    assert_eq!(app.input.buffer, "/invite ");
    assert_eq!(status_text(&app), "completed command: /invite");
}

#[tokio::test]
async fn refresh_and_the_invites_command_request_the_list() {
    let mut app = fresh_app();
    app.handle_command(Command::Refresh).await;
    assert!(app.invites_fetch_inflight);
    assert!(app.rooms_fetch_inflight);

    let mut app = fresh_app();
    app.handle_command(Command::Invites).await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert!(app.invites_fetch_inflight);
    assert_eq!(app.bootstrap, BootstrapStage::Done);
}

#[tokio::test]
async fn accounts_stage_starts_the_invite_fetch_without_advancing_on_it() {
    let mut app = fresh_app();
    app.bootstrap = BootstrapStage::Accounts;
    let (tx, _rx) = mpsc::unbounded_channel();
    app.set_bootstrap_sender(tx);
    app.handle_bootstrap_outcome(BootstrapOutcome::Accounts(Ok(Vec::new())))
        .await;
    assert_eq!(app.bootstrap, BootstrapStage::Rooms);
    assert!(app.invites_fetch_inflight);
    assert!(app.rooms_fetch_inflight);

    app.bootstrap = BootstrapStage::Rooms;
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        Vec::new(),
        Instant::now(),
        false,
    ))))
    .await;
    assert_eq!(app.bootstrap, BootstrapStage::Rooms);
    assert!(!app.invites_fetch_inflight);
}

#[tokio::test]
async fn older_server_404_clears_the_inbox_and_keeps_suppression() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    app.invites = vec![row.clone()];
    app.invite_suppressed.insert(key.clone());
    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", Instant::now()));
    app.invites_fetch_inflight = true;
    app.invite_live_adds.push((key.clone(), row));
    app.status = Status::from("keep".to_owned());
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        Vec::new(),
        Instant::now(),
        true,
    ))))
    .await;
    assert!(app.invites.is_empty());
    assert!(app.invite_live_adds.is_empty());
    assert!(app.invite_live_removals.is_empty());
    assert!(app.invite_suppressed.contains(&key));
    assert!(app.pending_invite_rejects.contains_key(&key));
    assert!(!app.pending_invite_rejects[&key].seen_listed);
    assert_eq!(status_text(&app), "keep");
}

#[tokio::test]
async fn in_flight_list_cannot_resurrect_a_suppressed_invite() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    app.invites_fetch_inflight = true;
    app.invite_suppressed.insert(key.clone());
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        vec![row],
        Instant::now(),
        false,
    ))))
    .await;
    assert!(app.invites.is_empty());
    assert!(app.invite_suppressed.contains(&key));
}

#[tokio::test]
async fn server_row_wins_over_a_live_add_and_newest_live_adds_lead() {
    let mut app = fresh_app();
    let mut server = invite_row(Uuid::nil(), "!room:example.com", Some("Server"));
    server.canonical_alias = Some("#ops:example.com".to_owned());
    let mut live = server.clone();
    live.name = Some("Live".to_owned());
    let newer = invite_row(Uuid::nil(), "!new:example.com", Some("Newer"));
    let older = invite_row(Uuid::nil(), "!old:example.com", Some("Older"));
    app.invites_fetch_inflight = true;
    app.invite_live_adds = vec![
        (key_of(&newer), newer),
        (key_of(&live), live),
        (key_of(&older), older),
    ];
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        vec![server],
        Instant::now(),
        false,
    ))))
    .await;
    let titles: Vec<_> = app.invites.iter().map(InviteDto::title).collect();
    // Live adds that the server omitted stay in front, newest first.
    // The server copy of a key replaces the live add and stays with the server rows.
    assert_eq!(titles, vec!["Newer", "Older", "Server"]);
}

#[tokio::test]
async fn failed_read_keeps_the_list_and_the_retry_keeps_its_live_maps() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row.clone()];
    app.invites_fetch_inflight = true;
    app.invite_live_adds.push((key_of(&row), row.clone()));
    let since = Instant::now();
    app.pending_invite_rejects
        .insert(key_of(&row), pending_hold("Ops", since));
    app.pending_invite_rejects
        .get_mut(&key_of(&row))
        .unwrap()
        .seen_listed = true;
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Err(ApiError::Request(
        "nope".to_owned(),
    ))))
    .await;
    assert_eq!(app.invites.len(), 1);
    assert!(app.pending_invite_rejects[&key_of(&row)].seen_listed);
    assert_eq!(app.invite_live_adds.len(), 1);
    assert_eq!(status_text(&app), "invite refresh failed: nope");
    assert!(!app.invites_fetch_inflight);

    app.status = Status::from("keep".to_owned());
    app.mode = Mode::Verification;
    app.invites_fetch_inflight = true;
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Err(ApiError::Request(
        "again".to_owned(),
    ))))
    .await;
    assert_eq!(status_text(&app), "keep");

    // A retry must still merge the add. An empty snapshot from that retry
    // is not a reason to drop an invite that arrived during the failed read.
    app.mode = Mode::Compose;
    app.request_invites_refresh();
    assert_eq!(app.invite_live_adds.len(), 1);
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        Vec::new(),
        Instant::now(),
        false,
    ))))
    .await;
    assert_eq!(app.invites.len(), 1);
    assert_eq!(app.invites[0].room_id, "!room:example.com");
}

#[tokio::test]
async fn a_failed_read_does_not_let_the_retry_resurrect_a_removal() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    app.invites = vec![row.clone()];
    app.invites_fetch_inflight = true;
    app.handle_live_frame(LiveFrame::InviteRemoved {
        account_id: row.account_id,
        room_id: row.room_id.clone(),
    });
    assert!(app.invites.is_empty());
    assert!(app.invite_live_removals.contains(&key));

    app.invites_fetch_again = true;
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Err(ApiError::Request(
        "nope".to_owned(),
    ))))
    .await;
    assert!(app.invites_fetch_inflight);
    assert!(app.invite_live_removals.contains(&key));
    assert!(!app.invites_fetch_again);

    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        vec![row],
        Instant::now(),
        false,
    ))))
    .await;
    assert!(app.invites.is_empty());
}

#[tokio::test]
async fn invite_added_clears_suppression_and_does_not_end_a_pending_reject() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    app.invite_suppressed.insert(key.clone());
    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", Instant::now()));
    let action = app.handle_live_frame(LiveFrame::InviteAdded(row));
    assert_eq!(action, LiveFrameAction::None);
    assert!(app.invites.iter().any(|invite| key_of(invite) == key));
    assert!(!app.invite_suppressed.contains(&key));
    assert!(app.pending_invite_rejects.contains_key(&key));
    assert_ne!(status_text(&app), "rejected Ops");
}

#[tokio::test]
async fn invite_removed_ends_a_pending_reject_without_suppressing() {
    let mut app = fresh_app();
    let alice = Uuid::from_u128(1);
    let carol = Uuid::from_u128(2);
    let alice_row = invite_row(alice, "!room:example.com", Some("Ops"));
    let carol_row = invite_row(carol, "!room:example.com", Some("Ops"));
    app.invites = vec![alice_row, carol_row];
    app.pending_invite_rejects.insert(
        RoomKey {
            account_id: alice,
            room_id: "!room:example.com".to_owned(),
        },
        pending_hold("Ops", Instant::now()),
    );
    let action = app.handle_live_frame(LiveFrame::InviteRemoved {
        account_id: alice,
        room_id: "!room:example.com".to_owned(),
    });
    assert_eq!(action, LiveFrameAction::None);
    assert_eq!(app.invites.len(), 1);
    assert_eq!(app.invites[0].account_id, carol);
    assert!(app.invite_suppressed.is_empty());
    assert!(app.pending_invite_rejects.is_empty());
    assert_eq!(status_text(&app), "rejected Ops");
}

#[tokio::test]
async fn known_contact_add_then_remove_leaves_an_empty_inbox() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!dm:example.com", Some("Ada"));
    app.handle_live_frame(LiveFrame::InviteAdded(row));
    app.handle_live_frame(LiveFrame::InviteRemoved {
        account_id: Uuid::nil(),
        room_id: "!dm:example.com".to_owned(),
    });
    assert!(app.invites.is_empty());
    assert!(app.invite_suppressed.is_empty());
    assert!(status_text(&app).is_empty());
}

#[tokio::test]
async fn reject_schedule_settles_from_the_invite_list() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    let since = Instant::now();
    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", since));
    app.invites = vec![row.clone()];

    let early = snapshot(Vec::new(), since - Duration::from_secs(1), false);
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(early)))
        .await;
    assert!(app.pending_invite_rejects.contains_key(&key));
    assert!(!app.pending_invite_rejects[&key].seen_listed);

    app.invites_fetch_inflight = true;
    app.invite_live_removals.insert(key.clone());
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        vec![row.clone()],
        since + Duration::from_secs(1),
        false,
    ))))
    .await;
    assert!(!app.pending_invite_rejects.contains_key(&key));
    assert_eq!(status_text(&app), "rejected Ops");

    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", since));
    app.status = Status::Info(String::new());
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        vec![row.clone()],
        since + Duration::from_secs(1),
        false,
    ))))
    .await;
    assert!(app.pending_invite_rejects[&key].seen_listed);

    app.mode = Mode::Verification;
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        Vec::new(),
        since + Duration::from_secs(2),
        false,
    ))))
    .await;
    assert!(!app.pending_invite_rejects.contains_key(&key));
    assert!(status_text(&app).is_empty());

    app.mode = Mode::Compose;
    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", since));
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        Vec::new(),
        since + Duration::from_secs(3),
        true,
    ))))
    .await;
    assert!(app.pending_invite_rejects.contains_key(&key));

    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Err(ApiError::Request(
        "down".to_owned(),
    ))))
    .await;
    assert!(app.pending_invite_rejects.contains_key(&key));
    assert!(!app.pending_invite_rejects[&key].seen_listed);
}

#[test]
fn reject_checks_land_on_the_leave_schedule() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    let since = Instant::now();
    let mut held = pending_hold("Ops", since);
    held.seen_listed = true;
    app.pending_invite_rejects.insert(key.clone(), held);

    let mut at = since;
    for delay in PENDING_MEMBERSHIP_CHECK_DELAYS {
        at += delay;
        app.poll_pending_invite_rejects(at);
        assert!(app.pending_invite_rejects.contains_key(&key));
    }
    let pending = &app.pending_invite_rejects[&key];
    assert!(pending.retryable);
    assert_eq!(pending.step, PENDING_MEMBERSHIP_CHECK_DELAYS.len());
    at += PENDING_MEMBERSHIP_FINAL_GRACE;
    app.poll_pending_invite_rejects(at);
    assert!(!app.pending_invite_rejects.contains_key(&key));
    assert_eq!(
        status_text(&app),
        "Ops is still invited; the reject may not have gone through, so reject it again"
    );

    let mut quiet = fresh_app();
    let mut held = pending_hold("Ops", since);
    held.seen_listed = false;
    quiet.pending_invite_rejects.insert(key.clone(), held);
    let mut at = since;
    for delay in PENDING_MEMBERSHIP_CHECK_DELAYS {
        at += delay;
        quiet.poll_pending_invite_rejects(at);
    }
    at += PENDING_MEMBERSHIP_FINAL_GRACE;
    quiet.poll_pending_invite_rejects(at);
    assert!(status_text(&quiet).is_empty());
}

#[tokio::test]
async fn repeat_reject_is_held_until_the_first_check() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    app.pending_invite_rejects
        .insert(key_of(&row), pending_hold("Ops", Instant::now()));
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    assert!(!app.room_action_busy);
    assert!(status_text(&app).contains("no need to reject it again"));
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));

    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    assert!(!app.room_action_busy);
    assert!(matches!(app.mode, Mode::ConfirmInviteReject { .. }));
    assert!(status_text(&app).contains("Reject Ops?"));
    app.handle_key(KeyEvent::from(KeyCode::Char('y'))).await;
    assert!(!app.room_action_busy);
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert!(!status_text(&app).contains("rejecting"));
}

#[tokio::test]
async fn a_retry_after_the_first_check_goes_out_and_a_hard_failure_ends_the_hold() {
    let (base, request_rx) = serve_one(
        "500 Internal Server Error",
        r#"{"error":{"code":"M_UNKNOWN","message":"nope"}}"#,
    )
    .await;
    let mut app = fresh_app();
    app.client = AxonClient::new(base, None);
    let mut rx = outcome_channel(&mut app).await;
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    let since = Instant::now();
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key.clone());
    app.mode = Mode::Popup(PopupKind::Invites);
    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", since));
    app.poll_pending_invite_rejects(since + PENDING_MEMBERSHIP_CHECK_DELAYS[0]);
    assert!(app.pending_invite_rejects[&key].retryable);

    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    assert!(
        rx.try_recv().is_err(),
        "the reject key asks first and does not send"
    );
    assert!(matches!(app.mode, Mode::ConfirmInviteReject { .. }));
    app.handle_key(KeyEvent::from(KeyCode::Char('y'))).await;
    let outcome = rx.recv().await.expect("reject was sent");
    let request = request_rx.await.unwrap();
    assert!(request.contains("POST "), "{request}");
    assert!(request.contains("/leave"), "{request}");
    assert!(matches!(
        outcome.result,
        Err(ref err) if !err.timed_out
    ));
    app.handle_invite_action_outcome(outcome).await;
    assert!(!app.pending_invite_rejects.contains_key(&key));
    assert!(status_text(&app).contains("reject failed"));
    assert_eq!(app.invites.len(), 1);
}

#[tokio::test]
async fn reject_outcomes_follow_the_list_rules() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key.clone());
    app.mode = Mode::Popup(PopupKind::Invites);
    app.invites_fetch_inflight = true;
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row.clone(),
        verb: InviteVerb::Reject,
        result: Ok(None),
    })
    .await;
    assert!(app.invites.is_empty());
    assert!(app.invite_suppressed.contains(&key));
    assert!(app.invite_live_removals.contains(&key));
    assert!(app.invites_fetch_again);
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert_eq!(status_text(&app), "rejected Ops");

    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        vec![row.clone()],
        Instant::now(),
        false,
    ))))
    .await;
    assert!(app.invites.is_empty());
    assert!(app.invite_suppressed.contains(&key));

    let mut app = fresh_app();
    app.invites = vec![row.clone()];
    app.mode = Mode::Popup(PopupKind::Invites);
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row.clone(),
        verb: InviteVerb::Reject,
        result: Err(failure("timeout", true)),
    })
    .await;
    assert_eq!(app.invites.len(), 1);
    assert!(!app.invite_suppressed.contains(&key));
    assert!(!app.pending_invite_rejects[&key].retryable);
    assert!(status_text(&app).contains("hasn't finished rejecting Ops"));
    let first_since = app.pending_invite_rejects[&key].since;

    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row.clone(),
        verb: InviteVerb::Reject,
        result: Err(failure("timeout", true)),
    })
    .await;
    assert!(app.pending_invite_rejects[&key].since >= first_since);
    assert_eq!(app.pending_invite_rejects[&key].step, 0);

    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row,
        verb: InviteVerb::Reject,
        result: Err(failure("nope", false)),
    })
    .await;
    assert!(app.pending_invite_rejects.is_empty());
    assert!(status_text(&app).contains("reject failed for Ops"));
}

#[tokio::test]
async fn accept_uses_join_and_opens_a_listed_room_without_adding_an_invite_row() {
    let (base, request_rx) = serve_one(
        "200 OK",
        &serde_json::json!({ "data": { "room_id": "!resolved:example.com" } }).to_string(),
    )
    .await;
    let mut app = fresh_app();
    app.client = AxonClient::new(base, None);
    let mut rx = outcome_channel(&mut app).await;
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row];
    app.invite_selection = app.invites.first().map(RoomKey::from);
    app.mode = Mode::Popup(PopupKind::Invites);
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Enter));
    let outcome = rx.recv().await.expect("accept was sent");
    let request = request_rx.await.unwrap();
    assert!(request.contains("/rooms/join"), "{request}");
    assert!(matches!(outcome.result, Ok(Some(ref id)) if id == "!resolved:example.com"));

    let ops = room("!resolved:example.com", None, Some("Ops"));
    let base = spawn_api_stub(vec![
        rooms_response_body(std::slice::from_ref(&ops)),
        empty_timeline_response_body(),
        empty_members_response_body(),
    ])
    .await;
    let mut app = fresh_app();
    app.client = AxonClient::new(base, None);
    app.input.buffer = "hello".to_owned();
    app.mode = Mode::Popup(PopupKind::Invites);
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Tea"));
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row,
        verb: InviteVerb::Accept,
        result: Ok(Some("!resolved:example.com".to_owned())),
    })
    .await;
    assert_eq!(app.rooms.rooms.len(), 1);
    assert_eq!(app.rooms.rooms[0].room_id, "!resolved:example.com");
    assert!(app.invites.is_empty());
    assert!(!app.input.buffer.starts_with("/join"));
    assert_eq!(app.mode, Mode::Compose);
    assert!(status_text(&app).contains("joined"));
    assert!(app.pending_memberships.is_empty());
    assert!(
        app.invites_fetch_inflight || app.invites_fetch_again,
        "a 2xx accept asks for a list that can clear suppression"
    );
}

#[tokio::test]
async fn accept_when_unlisted_watches_the_resolved_room_and_keeps_the_composer() {
    let other = room("!other:example.com", None, Some("Other"));
    let base = spawn_api_stub(vec![rooms_response_body(std::slice::from_ref(&other))]).await;
    let mut app = fresh_app();
    app.client = AxonClient::new(base, None);
    app.input.buffer = "hello".to_owned();
    app.mode = Mode::Popup(PopupKind::Invites);
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Tea"));
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row.clone(),
        verb: InviteVerb::Accept,
        result: Ok(Some("!resolved:example.com".to_owned())),
    })
    .await;
    assert!(app
        .rooms
        .rooms
        .iter()
        .all(|room| room.room_id != "!resolved:example.com"));
    assert_eq!(
        app.pending_join_open
            .as_ref()
            .map(|pending| pending.target.as_str()),
        Some("!resolved:example.com")
    );
    assert_eq!(app.input.buffer, "hello");
    assert!(app.invite_suppressed.contains(&key_of(&row)));
    assert!(app.invites_fetch_inflight || app.invites_fetch_again);

    let mut app = fresh_app();
    app.input.buffer = "hello".to_owned();
    app.mode = Mode::Popup(PopupKind::Invites);
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Tea"));
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row.clone(),
        verb: InviteVerb::Accept,
        result: Ok(None),
    })
    .await;
    assert_eq!(
        app.pending_join_open
            .as_ref()
            .map(|pending| pending.target.as_str()),
        Some("!room:example.com")
    );
    assert_eq!(app.input.buffer, "hello");
}

#[tokio::test]
async fn accept_504_watches_the_room_list_and_does_not_restore_join() {
    let mut app = fresh_app();
    app.input.buffer = "hello".to_owned();
    app.mode = Mode::Popup(PopupKind::Invites);
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Tea"));
    app.invites = vec![row.clone()];
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row.clone(),
        verb: InviteVerb::Accept,
        result: Err(failure("timeout", true)),
    })
    .await;
    assert_eq!(app.input.buffer, "hello");
    assert_eq!(app.mode, Mode::Compose);
    assert!(app
        .invites
        .iter()
        .any(|invite| invite.room_id == row.room_id));
    assert!(app.invite_suppressed.is_empty());
    assert!(app.pending_memberships.is_empty());
    assert!(app.pending_invite_rejects.is_empty());
    assert_eq!(
        app.pending_join_open
            .as_ref()
            .map(|pending| pending.target.as_str()),
        Some("!room:example.com")
    );
    assert!(status_text(&app).contains("unknown"));
    assert!(app.invites_fetch_inflight || app.invites_fetch_again);
    assert!(app.invite_accept_followup.is_some());
    let follow = app.invite_accept_followup.clone().unwrap();
    app.poll_invite_accept_followup(follow.next_read_at);
    assert!(app.invites_fetch_again);
    let follow = app.invite_accept_followup.clone().unwrap();
    app.poll_invite_accept_followup(follow.next_read_at);
    assert!(app.invite_accept_followup.is_none());

    let mut app = fresh_app();
    app.mode = Mode::Popup(PopupKind::Invites);
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Tea"));
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row,
        verb: InviteVerb::Accept,
        result: Err(failure("timeout", true)),
    })
    .await;
    assert!(app.input.buffer.is_empty());
    assert!(app.invite_accept_followup.is_some());

    let ops = room("!room:example.com", None, Some("Ops"));
    let base = spawn_api_stub(vec![
        rooms_response_body(std::slice::from_ref(&ops)),
        empty_timeline_response_body(),
        empty_members_response_body(),
    ])
    .await;
    let mut app = fresh_app();
    app.client = AxonClient::new(base, None);
    app.input.buffer.clear();
    app.mode = Mode::Popup(PopupKind::Invites);
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Tea"));
    app.invites = vec![row.clone()];
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row,
        verb: InviteVerb::Accept,
        result: Err(failure("timeout", true)),
    })
    .await;
    assert_eq!(app.rooms.rooms.len(), 1);
    assert!(app.invite_suppressed.is_empty());
    assert!(app.pending_memberships.is_empty());
    assert!(app.pending_join_open.is_none());
    assert!(!app.input.buffer.starts_with("/join"));
    assert!(status_text(&app).contains("joined"));
    assert!(app.invites_fetch_inflight || app.invites_fetch_again);
    assert!(app.invite_accept_followup.is_some());
}

#[tokio::test]
async fn accept_failure_keeps_the_popup_and_a_busy_action_does_not_send() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    app.handle_invite_action_outcome(InviteActionOutcome {
        invite: row.clone(),
        verb: InviteVerb::Accept,
        result: Err(failure("nope", false)),
    })
    .await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert_eq!(app.invites.len(), 1);
    assert_eq!(status_text(&app), "accept failed for Ops: nope");

    app.room_action_busy = true;
    let mut rx = outcome_channel(&mut app).await;
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(status_text(&app), "a room action is already in progress");
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert!(rx.try_recv().is_err());

    app.room_action_busy = false;
    app.invites.clear();
    app.invite_selection = None;
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(status_text(&app), "no invite selected");
}

#[tokio::test]
async fn shift_r_and_ctrl_r_do_not_reject_and_submit_bound_to_r_accepts() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    let mut rx = outcome_channel(&mut app).await;
    app.handle_invites_popup_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));
    app.handle_invites_popup_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT));
    app.handle_invites_popup_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(!app.room_action_busy);
    assert!(rx.try_recv().is_err());
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));

    app.handle_invites_popup_key(KeyEvent::new_with_kind_and_state(
        KeyCode::Char('r'),
        KeyModifiers::empty(),
        KeyEventKind::Press,
        KeyEventState::CAPS_LOCK | KeyEventState::NUM_LOCK,
    ));
    assert_eq!(app.mode, Mode::ConfirmInviteReject { key: key_of(&row) });
    app.mode = Mode::Popup(PopupKind::Invites);

    let (base, _request_rx) = serve_one(
        "200 OK",
        &serde_json::json!({ "data": { "room_id": "!resolved:example.com" } }).to_string(),
    )
    .await;
    app.client = AxonClient::new(base, None);
    app.shortcuts.submit = KeyBinding::from_spec("r");
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    let outcome = rx.recv().await.expect("accept was sent");
    assert_eq!(outcome.verb, InviteVerb::Accept);
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
}

#[test]
fn rooms_title_appends_the_filtered_invite_count() {
    let mut app = fresh_app();
    assert_eq!(crate::ui::rooms_pane_title(&app), "Rooms — Recent");

    app.invites = vec![invite_row(Uuid::nil(), "!room:example.com", Some("Ops"))];
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms — Recent · 1 invite"
    );

    app.invites.push(invite_row(
        Uuid::from_u128(2),
        "!two:example.com",
        Some("Two"),
    ));
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms — Recent · 2 invites"
    );

    app.room_filter = RoomFilter::Dms;
    app.invites[0].is_direct = true;
    app.invites.truncate(1);
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms — DMs · Recent · 1 invite"
    );

    app.room_filter = RoomFilter::Groups;
    assert_eq!(crate::ui::rooms_pane_title(&app), "Rooms — Groups · Recent");

    app.room_filter = RoomFilter::Favorites;
    app.invites[0].is_direct = false;
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms — Favorites · Recent"
    );

    app.room_filter = RoomFilter::Unread;
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms — Unread · Recent · 1 invite"
    );

    app.room_filter = RoomFilter::All;
    app.spaces.order_error = Some("unsaved".to_owned());
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms — Recent — order unsaved/unavailable · 1 invite"
    );

    app.bootstrap = BootstrapStage::Accounts;
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms — connecting… — order unsaved/unavailable · 1 invite"
    );

    app.bootstrap = BootstrapStage::Done;
    app.spaces.order_error = None;
    app.mode = Mode::Search(SearchKind::Rooms, "ops".to_owned());
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms  Search: ops · 1 invite"
    );
    app.mode = Mode::Search(SearchKind::RoomNameFilter, "ops".to_owned());
    assert_eq!(
        crate::ui::rooms_pane_title(&app),
        "Rooms  Filter: ops · 1 invite"
    );

    app.mode = Mode::Compose;
    app.room_filter = RoomFilter::Name("bob".to_owned());
    assert!(crate::ui::rooms_pane_title(&app).ends_with("· 1 invite"));
    app.room_filter = RoomFilter::Name("missing".to_owned());
    assert!(!crate::ui::rooms_pane_title(&app).contains("invite"));

    app.room_filter = RoomFilter::All;
    app.accounts.inactive_ids.insert(Uuid::nil());
    assert_eq!(crate::ui::rooms_pane_title(&app), "Rooms — Recent");
}

#[test]
fn invites_popup_filters_wraps_and_scrolls_the_selected_row() {
    let mut app = fresh_app();
    app.mode = Mode::Popup(PopupKind::Invites);
    crate::ui::prepare(&mut app, Rect::new(0, 0, 80, 24));
    let reject_label = app.shortcuts.reject_invite.label();
    let title = app.invites_popup_title();
    let popup = app.frame.popup.as_ref().unwrap();
    assert_eq!(popup.title, title);
    assert!(popup.title.contains("Enter"));
    assert!(popup.title.contains(&reject_label));
    assert_eq!(line_text(&popup.lines[0]), "No invites.");

    let mut hidden = invite_row(Uuid::from_u128(9), "!hidden:example.com", Some("Hidden"));
    hidden.is_direct = true;
    app.invites = vec![hidden];
    app.account_filter = Some(Uuid::nil());
    crate::ui::prepare(&mut app, Rect::new(0, 0, 80, 24));
    assert_eq!(
        line_text(&app.frame.popup.as_ref().unwrap().lines[0]),
        "No invites."
    );

    app.account_filter = None;
    app.room_filter = RoomFilter::Groups;
    crate::ui::prepare(&mut app, Rect::new(0, 0, 80, 24));
    assert_eq!(
        line_text(&app.frame.popup.as_ref().unwrap().lines[0]),
        "No invites match this filter."
    );

    app.room_filter = RoomFilter::All;
    let mut direct = invite_row(Uuid::nil(), "!dm:example.com", Some("Ada"));
    direct.is_direct = true;
    direct.account_user_id = "@alice:example.com".to_owned();
    let mut group = invite_row(Uuid::from_u128(2), "!group:example.com", Some("Ops"));
    group.account_user_id = "@carol:example.com".to_owned();
    group.inviter_display_name = None;
    app.invites = vec![direct, group.clone()];
    app.accounts.accounts = vec![
        account_with_id(Uuid::nil(), "@alice:example.com", AccountState::Active),
        account_with_id(
            Uuid::from_u128(2),
            "@carol:example.com",
            AccountState::Active,
        ),
    ];
    app.pending_invite_rejects
        .insert(key_of(&group), pending_hold("Ops", Instant::now()));
    app.invite_selection = Some(key_of(&group));
    crate::ui::prepare(&mut app, Rect::new(0, 0, 80, 24));
    let lines: Vec<_> = app
        .frame
        .popup
        .as_ref()
        .unwrap()
        .lines
        .iter()
        .map(line_text)
        .collect();
    assert!(lines.iter().any(|line| line.contains("[carol]")));
    assert!(lines.iter().any(|line| line.contains("(reject pending)")));
    assert!(lines.iter().any(|line| line.starts_with('>')));
    assert!(lines
        .iter()
        .any(|line| line.contains("from @bob:example.com")));

    app.accounts.accounts.truncate(1);
    app.invites = vec![invite_row(
        Uuid::nil(),
        "!room:example.com",
        Some(&"Name".repeat(80)),
    )];
    app.invite_selection = app.invites.first().map(RoomKey::from);
    crate::ui::prepare(&mut app, Rect::new(0, 0, 40, 20));
    let popup = app.frame.popup.as_ref().unwrap();
    let inner = usize::from(popup.area.width.saturating_sub(2));
    assert!(popup.lines.len() > 1);
    for line in &popup.lines {
        assert!(line_text(line).chars().count() <= inner);
    }

    app.invites = (0..20)
        .map(|index| {
            invite_row(
                Uuid::nil(),
                &format!("!room{index}:example.com"),
                Some("Row"),
            )
        })
        .collect();
    app.invite_selection = app.invites.last().map(RoomKey::from);
    crate::ui::prepare(&mut app, Rect::new(0, 0, 40, 12));
    assert!(app.popup_scroll > 0);

    let first = key_of(&app.invites[0]);
    app.invite_selection = Some(first.clone());
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Up));
    assert_eq!(app.invite_selection.as_ref(), Some(&first));
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::PageDown));
    let selected = app.invite_selection.clone().unwrap();
    let index = app
        .filtered_invites()
        .position(|invite| key_of(invite) == selected)
        .unwrap();
    assert_eq!(index, 8);
    app.invite_selection = app.invites.last().map(RoomKey::from);
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Down));
    assert_eq!(
        app.invite_selection
            .as_ref()
            .map(|key| key.room_id.as_str()),
        Some("!room19:example.com")
    );
}

#[tokio::test]
async fn a_hidden_selection_is_not_accepted_or_rejected_on_that_key() {
    let mut app = fresh_app();
    let mut direct = invite_row(Uuid::nil(), "!dm:example.com", Some("Ada"));
    direct.is_direct = true;
    let group = invite_row(Uuid::nil(), "!group:example.com", Some("Ops"));
    app.invites = vec![direct.clone(), group.clone()];
    app.invite_selection = Some(key_of(&direct));
    app.mode = Mode::Popup(PopupKind::Invites);
    app.room_filter = RoomFilter::Groups;
    let mut rx = outcome_channel(&mut app).await;

    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Enter));
    assert!(rx.try_recv().is_err());
    assert!(!app.room_action_busy);
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert_eq!(app.invite_selection.as_ref(), Some(&key_of(&group)));

    app.invite_selection = Some(key_of(&direct));
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    assert!(rx.try_recv().is_err());
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert_eq!(app.invite_selection.as_ref(), Some(&key_of(&group)));

    app.invite_selection = Some(key_of(&direct));
    crate::ui::prepare(&mut app, Rect::new(0, 0, 80, 24));
    assert_eq!(app.invite_selection.as_ref(), Some(&key_of(&group)));
    let lines: Vec<_> = app
        .frame
        .popup
        .as_ref()
        .unwrap()
        .lines
        .iter()
        .map(line_text)
        .collect();
    assert!(lines
        .iter()
        .any(|line| line.starts_with('>') && line.contains("Ops")));
    assert!(lines.iter().all(|line| !line.contains("Ada")));

    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Enter));
    let outcome = rx.recv().await.expect("the now-visible invite is accepted");
    assert_eq!(outcome.verb, InviteVerb::Accept);
    assert_eq!(outcome.invite.room_id, "!group:example.com");
}

#[tokio::test]
async fn reject_asks_before_sending_and_cancel_stays_in_the_inbox() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    app.input.buffer = "keep me".to_owned();
    let now = Instant::now();
    app.pending_join_open = Some(crate::app::room_actions::PendingJoinOpen {
        account_id: Uuid::nil(),
        target: "!watched:example.com".to_owned(),
        anchor: None,
        room_choice: app.room_choice,
        since: now,
        reads_remaining: 2,
        next_read_at: now,
        deadline: now,
        last_read_started: None,
        ready: false,
        held_status: None,
    });
    let mut rx = outcome_channel(&mut app).await;

    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    assert!(matches!(
        &app.mode,
        Mode::ConfirmInviteReject { key } if key == &key_of(&row)
    ));
    assert!(status_text(&app).contains("Reject Ops?"));
    assert!(!app.room_action_busy);
    assert!(app.pending_join_open.is_some());
    assert_eq!(app.input.buffer, "keep me");
    assert!(rx.try_recv().is_err());

    app.handle_key(KeyEvent::from(KeyCode::Char('n'))).await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert_eq!(status_text(&app), "reject canceled");
    assert_eq!(app.invites.len(), 1);
    assert_eq!(app.input.buffer, "keep me");
    assert!(app.pending_join_open.is_some());
    assert!(rx.try_recv().is_err());

    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    app.handle_key(KeyEvent::from(KeyCode::Char('N'))).await;
    assert_eq!(status_text(&app), "reject canceled");
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert_eq!(status_text(&app), "reject canceled");
    assert!(rx.try_recv().is_err());

    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    app.handle_key(KeyEvent::from(KeyCode::Char('r'))).await;
    assert!(matches!(app.mode, Mode::ConfirmInviteReject { .. }));
    assert!(rx.try_recv().is_err());

    app.room_action_busy = true;
    app.handle_key(KeyEvent::from(KeyCode::Char('y'))).await;
    assert_eq!(status_text(&app), "a room action is already in progress");
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert!(app.pending_join_open.is_some());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn reject_invite_follows_the_configured_key() {
    let (base, request_rx) = serve_one(
        "500 Internal Server Error",
        r#"{"error":{"code":"M_UNKNOWN","message":"nope"}}"#,
    )
    .await;
    let mut app = fresh_app();
    app.client = AxonClient::new(base, None);
    let mut rx = outcome_channel(&mut app).await;
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    app.shortcuts.reject_invite = KeyBinding::from_spec("x");

    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert!(rx.try_recv().is_err());

    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('x')));
    assert!(matches!(app.mode, Mode::ConfirmInviteReject { .. }));
    app.handle_key(KeyEvent::from(KeyCode::Char('y'))).await;
    let request = request_rx.await.unwrap();
    assert!(request.contains("POST "), "{request}");
    assert!(request.contains("/leave"), "{request}");
    let outcome = rx.recv().await.expect("configured reject was sent");
    assert_eq!(outcome.verb, InviteVerb::Reject);
}

#[tokio::test]
async fn a_logged_out_account_is_not_settled_when_the_list_omits_it() {
    let mut app = fresh_app();
    let account = Uuid::from_u128(7);
    let row = invite_row(account, "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    let since = Instant::now();
    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", since));
    app.accounts.inactive_ids.insert(account);

    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        vec![row.clone()],
        since + Duration::from_secs(1),
        false,
    ))))
    .await;
    assert!(app.pending_invite_rejects.contains_key(&key));
    assert!(!app.pending_invite_rejects[&key].seen_listed);

    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        Vec::new(),
        since + Duration::from_secs(2),
        false,
    ))))
    .await;
    assert!(app.pending_invite_rejects.contains_key(&key));
    assert!(!app.pending_invite_rejects[&key].seen_listed);
    assert!(!status_text(&app).contains("rejected"));

    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", since));
    app.handle_bootstrap_outcome(BootstrapOutcome::Invites(Ok(snapshot(
        Vec::new(),
        since + Duration::from_secs(1),
        false,
    ))))
    .await;
    assert!(!app.pending_invite_rejects.contains_key(&key));
    assert_eq!(status_text(&app), "rejected Ops");
}

#[test]
fn a_logged_out_reject_does_not_advance_or_refresh() {
    let mut app = fresh_app();
    let account = Uuid::from_u128(7);
    let row = invite_row(account, "!room:example.com", Some("Ops"));
    let key = key_of(&row);
    let since = Instant::now();
    app.pending_invite_rejects
        .insert(key.clone(), pending_hold("Ops", since));
    app.accounts.inactive_ids.insert(account);
    let next_check = app.pending_invite_rejects[&key].next_check;

    app.poll_pending_invite_rejects(since + Duration::from_secs(10_000));
    let pending = &app.pending_invite_rejects[&key];
    assert_eq!(pending.step, 0);
    assert!(!pending.retryable);
    assert_eq!(pending.next_check, next_check);
    assert!(!app.invites_fetch_inflight);
    assert!(!app.invites_fetch_again);
    assert!(!status_text(&app).contains("still invited"));

    app.accounts.inactive_ids.remove(&account);
    app.poll_pending_invite_rejects(since + Duration::from_secs(10_000));
    assert_eq!(app.pending_invite_rejects[&key].step, 1);
    assert!(app.pending_invite_rejects[&key].retryable);
    assert!(app.invites_fetch_inflight);
}

#[test]
fn confirm_keeps_the_inbox_and_pins_the_question() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    app.status = Status::from("overwritten".to_owned());
    crate::ui::prepare(&mut app, Rect::new(0, 0, 80, 24));
    let popup = app.frame.popup.as_ref().unwrap();
    assert!(popup.title.contains("Reject Ops?"));
    assert!(popup.title.contains("[y/N]"));
    assert!(line_text(&popup.lines[0]).contains("Ops"));
    assert_eq!(status_text(&app), "Reject Ops? [y/N]");
    assert!(matches!(
        &app.mode,
        Mode::ConfirmInviteReject { key } if key == &key_of(&row)
    ));

    app.invites = vec![invite_row(
        Uuid::nil(),
        "!room:example.com",
        Some(&"Name".repeat(40)),
    )];
    crate::ui::prepare(&mut app, Rect::new(0, 0, 40, 12));
    let title = app.frame.popup.as_ref().unwrap().title.clone();
    assert!(title.ends_with("[y/N]"), "{title}");
    assert!(title.contains("..."), "{title}");
}

#[tokio::test]
async fn confirming_y_does_not_reject_an_invite_that_left() {
    let mut app = fresh_app();
    let row = invite_row(Uuid::nil(), "!room:example.com", Some("Ops"));
    let other = invite_row(Uuid::nil(), "!other:example.com", Some("Other"));
    app.invites = vec![row.clone(), other.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    let mut rx = outcome_channel(&mut app).await;
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    app.handle_live_frame(LiveFrame::InviteRemoved {
        account_id: row.account_id,
        room_id: row.room_id.clone(),
    });
    app.handle_key(KeyEvent::from(KeyCode::Char('y'))).await;
    assert!(rx.try_recv().is_err());
    assert!(!app.room_action_busy);
    assert_eq!(app.mode, Mode::Popup(PopupKind::Invites));
    assert_eq!(status_text(&app), "that invite is no longer pending");
    assert_eq!(app.invites.len(), 1);
    assert_eq!(app.invites[0].room_id, "!other:example.com");
}

#[tokio::test]
async fn confirming_y_still_rejects_an_invite_a_filter_would_hide() {
    let mut app = fresh_app();
    let mut row = invite_row(Uuid::nil(), "!dm:example.com", Some("Ada"));
    row.is_direct = true;
    app.invites = vec![row.clone()];
    app.invite_selection = Some(key_of(&row));
    app.mode = Mode::Popup(PopupKind::Invites);
    let mut rx = outcome_channel(&mut app).await;
    app.handle_invites_popup_key(KeyEvent::from(KeyCode::Char('r')));
    app.room_filter = RoomFilter::Groups;
    app.handle_key(KeyEvent::from(KeyCode::Char('y'))).await;
    let outcome = rx.recv().await.expect("the confirmed invite is rejected");
    assert_eq!(outcome.verb, InviteVerb::Reject);
    assert_eq!(outcome.invite.room_id, "!dm:example.com");
}

#[test]
fn the_popup_title_omits_a_reject_key_that_cannot_fire() {
    let mut app = fresh_app();
    assert!(app.invites_popup_title().contains("reject"));

    app.shortcuts.reject_invite = app.shortcuts.submit.clone();
    let title = app.invites_popup_title();
    assert!(!title.contains("reject"), "{title}");
    assert!(title.contains("accept"), "{title}");

    app.shortcuts.reject_invite = app.shortcuts.message_down.clone();
    assert!(!app.invites_popup_title().contains("reject"));

    app.shortcuts.reject_invite = app.shortcuts.message_page_down.clone();
    assert!(app.invites_popup_title().contains("reject"));
}

async fn serve_one(status: &str, body: &str) -> (String, tokio::sync::oneshot::Receiver<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let status = status.to_owned();
    let body = body.to_owned();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&request).into_owned());
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });
    (format!("http://{address}"), rx)
}
