//! Tab completion for slash commands and their arguments.

use super::support::*;
use crate::app::*;

#[test]
fn room_completion_fills_unique_room_alias_match() {
    let mut app = app_with_rooms(vec![
        room("!one:example.com", Some("#one:example.com"), Some("One")),
        room("!test:example.com", Some("#test:example.com"), Some("Test")),
    ]);
    app.input.buffer = "/room te".to_owned();

    app.complete_room_input(false);

    assert_eq!(app.input.buffer, "/room #test:example.com");
}

#[test]
fn room_completion_ignores_rooms_hidden_by_account_filter() {
    let visible_account = Uuid::from_u128(1);
    let hidden_account = Uuid::from_u128(2);
    let mut visible = room("!visible:example.com", None, Some("General"));
    visible.account_id = visible_account;
    let mut hidden = room("!hidden:example.com", None, Some("General"));
    hidden.account_id = hidden_account;
    let mut app = app_with_rooms(vec![visible, hidden]);
    app.set_accounts(vec![
        account_with_id(
            visible_account,
            "@visible:example.com",
            AccountState::Active,
        ),
        account_with_id(hidden_account, "@hidden:example.com", AccountState::Active),
    ]);
    app.accounts.selected = AccountSelection::Account(0);
    app.input.buffer = "/room Gen".to_owned();

    app.complete_room_input(false);

    assert_eq!(app.input.buffer, "/room General");
    assert!(app.input.room_command_completion.is_none());
}

#[test]
fn tab_completion_keeps_parsed_command_aliases_discoverable() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/roo".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/roo");
    assert!(app.status.text(false).contains("/room, /rooms"));

    app.input.buffer = "/sw".to_owned();
    app.complete_input();

    assert_eq!(app.input.buffer, "/switch ");
}

#[tokio::test]
async fn account_search_accepts_n_and_uppercase_n_as_query_text() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::Search(SearchKind::Accounts, "a".to_owned());

    app.handle_key(KeyEvent::from(KeyCode::Char('n'))).await;
    app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT))
        .await;

    assert_eq!(
        app.mode,
        Mode::Search(SearchKind::Accounts, "anN".to_owned())
    );
}

#[tokio::test]
async fn submitting_account_search_selects_first_match() {
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        account_with_id(
            Uuid::from_u128(1),
            "@alice:example.com",
            AccountState::Active,
        ),
        account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
    ]);
    app.mode = Mode::Search(SearchKind::Accounts, "bob".to_owned());

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(app.mode, Mode::AccountList);
    assert_eq!(app.accounts.selected, AccountSelection::Account(1));
    assert_eq!(app.last_search.as_deref(), Some("bob"));
}

#[tokio::test]
async fn submitting_account_search_reports_no_match() {
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![account_with_id(
        Uuid::from_u128(1),
        "@alice:example.com",
        AccountState::Active,
    )]);
    app.mode = Mode::Search(SearchKind::Accounts, "missing".to_owned());

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(app.accounts.selected, AccountSelection::All);
    assert_eq!(app.status.text(false), "no account matches: missing");
}

#[test]
fn account_numbers_match_panel_labels() {
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        account_with_id(
            Uuid::from_u128(1),
            "@alice:example.com",
            AccountState::Active,
        ),
        account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
    ]);

    assert!(app.switch_account("0"));
    assert_eq!(app.accounts.selected, AccountSelection::All);
    assert!(app.switch_account("2"));
    assert_eq!(app.accounts.selected, AccountSelection::Account(1));
    assert_eq!(AccountSelection::All.display_number(), 0);
    assert_eq!(AccountSelection::Account(1).display_number(), 2);

    app.accounts.selected = AccountSelection::All;
    assert!(app.commit_account_search("2".to_owned()));
    assert_eq!(app.accounts.selected, AccountSelection::Account(1));
}

#[test]
fn logout_completion_cycles_only_matching_active_accounts() {
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![
        account("@alice:example.com", AccountState::Active),
        account("@alice:work.example", AccountState::Active),
        account("@bob:example.com", AccountState::Active),
        account("@alice:old.example", AccountState::Deactivated),
    ];
    app.input.buffer = "/logout alice".to_owned();

    app.complete_input();
    assert_eq!(app.input.buffer, "/logout @alice:example.com");
    assert!(app.status.text(false).contains("[1/2]"));

    app.complete_input();
    assert_eq!(app.input.buffer, "/logout @alice:work.example");
    assert!(app.status.text(false).contains("[2/2]"));

    app.complete_input_reverse();
    assert_eq!(app.input.buffer, "/logout @alice:example.com");
}

#[test]
fn logout_completion_without_target_cycles_all_active_accounts() {
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![
        account("@alice:example.com", AccountState::Active),
        account("@bob:example.com", AccountState::Active),
        account("@old:example.com", AccountState::Deactivated),
    ];
    app.input.buffer = "/logout".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/logout @alice:example.com");
    assert!(app.status.text(false).contains("[1/2]"));
}

#[test]
fn logout_completion_normalizes_server_qualified_username_forms() {
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![account("@alice:example.com", AccountState::Active)];

    app.input.buffer = "/logout alice:example.com".to_owned();
    app.complete_input();
    assert_eq!(app.input.buffer, "/logout @alice:example.com");

    app.input.logout_command_completion = None;
    app.input.buffer = "/logout alice@example.com".to_owned();
    app.complete_input();
    assert_eq!(app.input.buffer, "/logout @alice:example.com");
}

#[test]
fn logout_completion_selects_duplicate_user_ids_by_account_id() {
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![
        account_with_id(first_id, "@alice:example.com", AccountState::Active),
        account_with_id(second_id, "@alice:example.com", AccountState::Active),
    ];
    app.input.buffer = "/logout alice".to_owned();

    app.complete_input();
    assert_eq!(app.input.buffer, format!("/logout {first_id}"));

    app.complete_input();
    assert_eq!(app.input.buffer, format!("/logout {second_id}"));
    assert!(matches!(
        app.resolve_logout_target(Some(&second_id.to_string())),
        crate::app::lifecycle::LogoutResolution::Match(AccountDto { account_id, .. })
            if account_id == second_id
    ));
}

#[test]
fn delete_completion_selects_duplicate_user_ids_by_account_id() {
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![
        account_with_id(first_id, "@alice:example.com", AccountState::Active),
        account_with_id(second_id, "@alice:example.com", AccountState::Deactivated),
    ];
    app.input.buffer = "/delete alice".to_owned();

    app.complete_input();
    assert_eq!(app.input.buffer, format!("/delete {first_id}"));

    app.complete_input();
    assert_eq!(app.input.buffer, format!("/delete {second_id}"));
    assert!(matches!(
        app.resolve_delete_target(Some(&second_id.to_string())),
        crate::app::lifecycle::DeleteResolution::Match(AccountDto { account_id, .. })
            if account_id == second_id
    ));
}

#[test]
fn backup_completion_fills_enable_then_cycles_active_accounts() {
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![
        account("@alice:example.com", AccountState::Active),
        account("@bob:example.com", AccountState::Active),
        account("@old:example.com", AccountState::Deactivated),
    ];

    app.input.buffer = "/backup".to_owned();
    app.complete_input();
    assert_eq!(app.input.buffer, "/backup enable");
    assert!(app.status.text(false).contains("completed subcommand"));

    app.complete_input();
    assert_eq!(app.input.buffer, "/backup enable @alice:example.com");
    assert!(app.status.text(false).contains("[1/2]"));
    app.complete_input();
    assert_eq!(app.input.buffer, "/backup enable @bob:example.com");
}

#[test]
fn backup_enable_completion_selects_duplicate_user_ids_by_account_id() {
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![
        account_with_id(first_id, "@alice:example.com", AccountState::Active),
        account_with_id(second_id, "@alice:example.com", AccountState::Active),
    ];
    app.input.buffer = "/backup enable alice".to_owned();

    app.complete_input();
    assert_eq!(app.input.buffer, format!("/backup enable {first_id}"));
    app.complete_input();
    assert_eq!(app.input.buffer, format!("/backup enable {second_id}"));
}

#[test]
fn verify_completion_matches_room_users_and_excludes_self() {
    let r = room("!dm:example.com", None, Some("DM"));
    let mut app = app_with_rooms(vec![r.clone()]);
    app.rooms.selected = Some(0);
    let mut names = HashMap::new();
    // The own user (@alice, from room.account_user_id) must be excluded.
    names.insert("@alice:example.com".to_owned(), "Alice".to_owned());
    names.insert("@bob:example.com".to_owned(), "Bob".to_owned());
    names.insert("@carol:example.com".to_owned(), "Carol".to_owned());
    app.rooms.display_names.insert(RoomKey::from(&r), names);

    // A localpart prefix resolves to the single matching user.
    app.input.buffer = "/verify @bo".to_owned();
    app.complete_input();
    assert_eq!(app.input.buffer, "/verify @bob:example.com");

    // An empty target cycles through every room user except our own.
    app.input.buffer = "/verify ".to_owned();
    app.input.verify_command_completion = None;
    app.complete_input();
    assert_eq!(app.input.buffer, "/verify @bob:example.com");
    app.complete_input();
    assert_eq!(app.input.buffer, "/verify @carol:example.com");
}

#[test]
fn tab_completion_fills_argument_slash_command_with_space() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/acco".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/account ");
}

#[test]
fn tab_completion_fills_help_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/he".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/help");
}

#[test]
fn tab_completion_fills_shortcuts_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/sh".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/shortcuts");
}

#[test]
fn tab_completion_fills_react_command_with_argument_space() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/rea".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/react ");
}

#[test]
fn tab_completion_cycles_emoji_matches_after_react_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/react face".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.complete_input();
    let first = app.input.buffer.clone();
    assert!(first.starts_with("/react "));
    assert!(app.status.text(false).contains("[1/"));

    app.complete_input();
    let second = app.input.buffer.clone();
    assert!(app.status.text(false).contains("[2/"));
    assert_ne!(second, first);
}

#[tokio::test]
async fn shift_tab_cycles_react_command_emoji_matches_backward() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/react face".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    let first = app.input.buffer.clone();
    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    assert_ne!(app.input.buffer, first);

    app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;
    assert_eq!(app.input.buffer, first);
    assert!(app.status.text(false).contains("[1/"));

    app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;
    let match_count = emoji_matches("face").len();
    assert!(app
        .status
        .text(false)
        .contains(&format!("[{match_count}/{match_count}]")));
}

#[tokio::test]
async fn compose_tab_completes_react_emoji_and_edit_resets_cycle() {
    let mut app = app_with_rooms(Vec::new());
    for ch in "/react face".chars() {
        app.handle_key(KeyEvent::from(KeyCode::Char(ch))).await;
    }

    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    assert!(app.input.react_command_completion.is_some());
    assert!(app.input.buffer.starts_with("/react "));

    app.handle_key(KeyEvent::from(KeyCode::Char('x'))).await;
    assert!(app.input.react_command_completion.is_none());
}

#[test]
fn react_command_emoji_completion_reports_no_matches() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/react not-a-known-emoji".to_owned();

    app.complete_input();

    assert_eq!(
        app.status.text(false),
        "no emoji matches 'not-a-known-emoji'"
    );
    assert_eq!(app.input.buffer, "/react not-a-known-emoji");
}

#[test]
fn tab_completion_fills_filter_argument() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/filter un".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/filter unread");
    assert_eq!(
        app.status.text(false),
        "[1/1] unread - Tab/Shift-Tab to cycle, Enter to filter"
    );
}

#[test]
fn tab_completion_cycles_filter_argument_aliases() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/filter g".to_owned();

    app.complete_input();
    assert_eq!(app.input.buffer, "/filter groups");

    app.complete_input();
    assert_eq!(app.input.buffer, "/filter group");

    app.complete_input_reverse();
    assert_eq!(app.input.buffer, "/filter groups");
}

#[test]
fn tab_completion_cycles_filter_arguments_without_target() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/filter ".to_owned();

    app.complete_input();
    assert_eq!(app.input.buffer, "/filter all");
    assert!(app.status.text(false).contains("[1/11] all"));
}

#[tokio::test]
async fn filter_completion_edit_resets_cycle() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/filter g".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    assert!(app.input.filter_command_completion.is_some());

    app.handle_key(KeyEvent::from(KeyCode::Char('x'))).await;
    assert!(app.input.filter_command_completion.is_none());
}

#[test]
fn tab_completion_fills_unreact_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/unreac".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/unreact");
}

#[test]
fn tab_completion_reports_ambiguous_slash_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/");
    assert!(app.status.text(false).contains("/room"));
    assert!(app.status.text(false).contains("/status"));
    assert!(app.status.text(false).contains("/event"));
    assert!(app.status.text(false).contains("/whoami"));
    assert!(app.status.text(false).contains("/whereami"));
    assert!(app.status.text(false).contains("/react"));
    assert!(app.status.text(false).contains("/unreact"));
    assert!(app.status.text(false).contains("/reply"));
    assert!(app.status.text(false).contains("/thread"));
    assert!(app.status.text(false).contains("/help"));
    assert!(app.status.text(false).contains("/shortcuts"));
    assert!(app.status.text(false).contains("/refresh"));
    assert!(app.status.text(false).contains("/quit"));
    assert!(app.status.text(false).contains("/join"));
    assert!(app.status.text(false).contains("/leave"));
    assert!(app.status.text(false).contains("/part"));
}

#[test]
fn tab_completion_fills_refresh_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/ref".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/refresh");
}

#[test]
fn tab_completion_fills_known_api_unsupported_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/jo".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/join ");
}

#[test]
fn tab_completion_fills_whoami_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/who".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/whoami");
}

#[test]
fn tab_completion_fills_whereami_command() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "/where".to_owned();

    app.complete_input();

    assert_eq!(app.input.buffer, "/whereami");
}
