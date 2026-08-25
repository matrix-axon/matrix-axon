//! Completing and resolving a room target from an alias, a name, or a prefix.

use super::support::*;
use crate::app::*;

#[test]
fn tab_completion_reports_missing_slash_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/zzz".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/zzz");
    assert_eq!(app.status.text(false), "no command matches: /zzz");
}

#[test]
fn room_completion_adds_missing_hash_for_qualified_alias() {
    let mut app = app_with_rooms(vec![room(
        "!test:example.com",
        Some("#test:example.com"),
        Some("Test"),
    )]);
    app.input.buffer = "/room test:ex".to_owned();

    app.complete_room_input(false);

    assert_eq!(app.input.buffer, "/room #test:example.com");
}

#[test]
fn room_completion_reports_ambiguous_room_matches() {
    let mut app = app_with_rooms(vec![
        room("!one:example.com", Some("#test:example.com"), Some("Test")),
        room(
            "!two:example.com",
            Some("#testing:example.com"),
            Some("Testing"),
        ),
    ]);
    app.input.buffer = "/room test".to_owned();

    app.complete_room_input(false);

    assert_eq!(app.input.buffer, "/room test");
    assert!(app.status.text(false).contains("#test:example.com"));
    assert!(app.status.text(false).contains("#testing:example.com"));
}

#[test]
fn room_completion_extends_to_common_prefix_and_shows_suffixes() {
    let mut app = app_with_rooms(vec![
        room("!one:example.com", None, Some("axontest")),
        room("!two:example.com", None, Some("axondev")),
    ]);
    app.input.buffer = "/room ax".to_owned();

    app.complete_room_input(false);

    assert_eq!(app.input.buffer, "/room axon");
    assert!(app.status.text(false).contains("test"));
    assert!(app.status.text(false).contains("dev"));
}

#[tokio::test]
async fn enter_does_not_submit_partial_switch_completion() {
    let mut app = app_with_rooms(vec![
        room("!one:example.com", None, Some("axontest")),
        room("!two:example.com", None, Some("axondev")),
    ]);
    app.input.buffer = "/room ax".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    assert_eq!(app.input.buffer, "/room axon");
    assert_eq!(
        app.input.partial_room_completions,
        Some(vec!["test".to_owned(), "dev".to_owned()])
    );

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(app.input.buffer, "/room axon");
    assert_eq!(app.rooms.selected, None);
    assert_eq!(
        app.status.text(false),
        "room completion is partial: test, dev - type more or press Tab"
    );

    app.handle_key(KeyEvent::from(KeyCode::Char('t'))).await;
    assert!(app.input.partial_room_completions.is_none());
}

#[test]
fn room_completion_uses_matching_names_when_rooms_have_aliases() {
    let mut app = app_with_rooms(vec![
        room(
            "!one:example.com",
            Some("#test:example.com"),
            Some("axontest"),
        ),
        room(
            "!two:example.com",
            Some("#dev:example.com"),
            Some("axondev"),
        ),
    ]);
    app.input.buffer = "/room ax".to_owned();

    app.complete_room_input(false);

    assert_eq!(app.input.buffer, "/room axon");
    assert!(app.status.text(false).contains("test"));
    assert!(app.status.text(false).contains("dev"));
}

#[test]
fn room_completion_still_completes_unique_match_fully() {
    let mut app = app_with_rooms(vec![
        room("!one:example.com", None, Some("axontest")),
        room("!two:example.com", None, Some("axondev")),
    ]);
    app.input.buffer = "/room axont".to_owned();

    app.complete_room_input(false);

    assert_eq!(app.input.buffer, "/room axontest");
}

#[test]
fn room_completion_replaces_unique_name_match_with_canonical_alias() {
    let mut app = app_with_rooms(vec![
        room(
            "!one:example.com",
            Some("#test:example.com"),
            Some("axontest"),
        ),
        room(
            "!two:example.com",
            Some("#dev:example.com"),
            Some("axondev"),
        ),
    ]);
    app.input.buffer = "/room axont".to_owned();

    app.complete_room_input(false);

    assert_eq!(app.input.buffer, "/room #test:example.com");
}

#[test]
fn room_completion_cycles_duplicate_named_rooms_with_disambiguator() {
    let mut app = app_with_rooms(vec![
        room("!one:example.com", None, Some("General")),
        room("!two:example.com", None, Some("General")),
    ]);
    app.input.buffer = "/room General".to_owned();

    app.complete_room_input(false);
    assert_eq!(app.input.buffer, "/room !one:example.com");
    assert!(app.status.text(false).contains("[1/2]"));
    assert!(app.status.text(false).contains("General"));
    assert!(app.status.text(false).contains("!one:example.com"));
    assert!(app.status.text(false).contains("Tab/Shift-Tab to cycle"));

    app.complete_room_input(false);
    assert_eq!(app.input.buffer, "/room !two:example.com");
    assert!(app.status.text(false).contains("[2/2]"));
    assert!(app.status.text(false).contains("!two:example.com"));

    app.complete_room_input(true);
    assert_eq!(app.input.buffer, "/room !one:example.com");
    assert!(app.status.text(false).contains("[1/2]"));
}

#[test]
fn room_completion_deduplicates_same_room_across_accounts() {
    // Regression: the same Matrix room joined by two accounts appears twice
    // in the room list (one per account_id). If one account hasn't synced the
    // canonical_alias state event yet, the room shows up as both
    // "#scratch:example.com" and "scratch", producing a spurious third match.
    // visible_rooms_for_completion deduplicates by room_id, keeping the entry
    // with a canonical alias.
    let mut account_b_entry = room("!scratch:example.com", None, Some("scratch"));
    account_b_entry.account_id = Uuid::from_u128(2);

    let mut app = app_with_rooms(vec![
        room(
            "!scratch:example.com",
            Some("#scratch:example.com"),
            Some("scratch"),
        ),
        room(
            "!scratch2:example.com",
            Some("#scratch-2:example.com"),
            Some("scratch-2"),
        ),
        account_b_entry,
    ]);
    app.input.buffer = "/room scratch".to_owned();

    app.complete_room_input(false);

    // Should see exactly 2 candidates, not 3.
    let status = app.status.text(false);
    assert!(
        status.contains("#scratch:example.com"),
        "expected alias in status: {status}"
    );
    assert!(
        status.contains("#scratch-2:example.com"),
        "expected alias-2 in status: {status}"
    );
    assert!(
        !status.contains("completions: #scratch:example.com, #scratch-2:example.com, scratch"),
        "spurious bare 'scratch' entry in status: {status}"
    );
}

#[tokio::test]
async fn room_completion_enter_selects_after_prefix_expansion_then_cycling() {
    // Regression: partial_room_completions set during prefix expansion must be
    // cleared when cycling begins, otherwise Enter is incorrectly blocked.
    let mut app = app_with_rooms(vec![
        room("!one:example.com", None, Some("General")),
        room("!two:example.com", None, Some("General")),
    ]);
    app.input.buffer = "/room G".to_owned();
    app.input.cursor = app.input.buffer.len();

    // First Tab: prefix-expands "G" → "General", sets partial_room_completions
    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    assert_eq!(app.input.buffer, "/room General");
    assert!(app.input.partial_room_completions.is_some());

    // Second Tab: enters cycling mode — partial_room_completions must be cleared
    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    assert!(app.input.buffer.starts_with("/room !"));
    assert!(app.input.partial_room_completions.is_none());

    // Enter must not be blocked
    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;
    assert!(app.rooms.selected.is_some());
}

#[test]
fn room_completion_typing_after_cycling_resets_to_normal_completion() {
    let mut app = app_with_rooms(vec![
        room("!one:example.com", None, Some("General")),
        room("!two:example.com", None, Some("General")),
    ]);
    app.input.buffer = "/room General".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.complete_room_input(false);
    assert!(app.input.room_command_completion.is_some());

    app.insert_char('x');
    assert!(app.input.room_command_completion.is_none());
}

#[test]
fn room_resolution_accepts_unique_name_prefix() {
    let app = app_with_rooms(vec![
        room(
            "!one:example.com",
            Some("#test:example.com"),
            Some("axontest"),
        ),
        room(
            "!two:example.com",
            Some("#dev:example.com"),
            Some("axondev"),
        ),
    ]);

    assert_eq!(
        app.resolve_room_target("axont"),
        RoomTargetResolution::Match(0)
    );
    assert_eq!(
        app.resolve_room_target("axon"),
        RoomTargetResolution::Ambiguous(vec!["test".to_owned(), "dev".to_owned()])
    );
}

#[test]
fn room_resolution_can_be_scoped_to_account() {
    let account_a = Uuid::from_u128(1);
    let account_b = Uuid::from_u128(2);
    let mut first = room("!one:example.com", None, Some("General"));
    first.account_id = account_a;
    let mut second = room("!two:example.com", None, Some("General"));
    second.account_id = account_b;
    let mut app = app_with_rooms(vec![first, second]);
    app.accounts.accounts = vec![
        account_with_id(account_a, "@alice:example.com", AccountState::Active),
        account_with_id(account_b, "@bob:example.com", AccountState::Active),
    ];
    app.accounts.selected = AccountSelection::Account(0);

    assert_eq!(
        app.resolve_room_target("General"),
        RoomTargetResolution::Match(0)
    );
    assert_eq!(
        app.resolve_room_target_in_account("General", None),
        RoomTargetResolution::Ambiguous(vec!["General".to_owned(), "General".to_owned()])
    );
    assert_eq!(
        app.resolve_room_target_in_account("General", Some(account_b)),
        RoomTargetResolution::Match(1)
    );
}

#[tokio::test]
async fn switch_command_reports_ambiguous_name_suffixes() {
    let mut app = app_with_rooms(vec![
        room(
            "!one:example.com",
            Some("#test:example.com"),
            Some("axontest"),
        ),
        room(
            "!two:example.com",
            Some("#dev:example.com"),
            Some("axondev"),
        ),
    ]);

    app.handle_command(Command::Room("axon".to_owned())).await;

    assert_eq!(app.status.text(false), "room name is ambiguous: test, dev");
    assert_eq!(app.rooms.selected, None);
}

#[test]
fn room_completion_only_runs_for_switch_command() {
    let mut app = app_with_rooms(vec![room(
        "!test:example.com",
        Some("#test:example.com"),
        Some("Test"),
    )]);
    app.input.buffer = "/event te".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/event te");
}

#[test]
pub(crate) fn find_room_adds_missing_hash_to_fully_qualified_alias() {
    let app = app_with_rooms(vec![room(
        "!abc:example.com",
        Some("#test:example.com"),
        Some("Test Room"),
    )]);

    assert_eq!(
        app.resolve_room_target("test:example.com"),
        RoomTargetResolution::Match(0)
    );
    assert_eq!(
        app.resolve_room_target("test:other.example"),
        RoomTargetResolution::Missing
    );
}

#[test]
pub(crate) fn find_room_keeps_exact_alias_and_name_matches() {
    let app = app_with_rooms(vec![room(
        "!abc:example.com",
        Some("#test:example.com"),
        Some("Friendly Name"),
    )]);

    assert_eq!(
        app.resolve_room_target("#test:example.com"),
        RoomTargetResolution::Match(0)
    );
    assert_eq!(
        app.resolve_room_target("friendly name"),
        RoomTargetResolution::Match(0)
    );
}

#[test]
pub(crate) fn find_room_does_not_local_match_fully_qualified_wrong_server() {
    let app = app_with_rooms(vec![room(
        "!abc:example.com",
        Some("#test:example.com"),
        Some("Test Room"),
    )]);

    assert_eq!(
        app.resolve_room_target("#test:other.example"),
        RoomTargetResolution::Missing
    );
}
