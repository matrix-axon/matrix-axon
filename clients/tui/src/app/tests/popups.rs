//! Popup navigation, and the search form and results views reached through it.

use super::support::*;
use crate::app::*;

#[tokio::test]
async fn popup_keys_scroll_and_close_popup() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::Popup(PopupKind::CommandResponse);
    app.pending_command_response = Some("full command response".to_owned());

    app.handle_key(KeyEvent::from(KeyCode::Down)).await;
    assert_eq!(app.popup_scroll, 1);

    app.handle_key(KeyEvent::from(KeyCode::PageDown)).await;
    assert_eq!(app.popup_scroll, 9);

    app.handle_key(KeyEvent::from(KeyCode::PageUp)).await;
    assert_eq!(app.popup_scroll, 1);

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
    assert_eq!(app.popup_scroll, 0);
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.pending_command_response.is_none());
}

#[tokio::test]
async fn dismissing_search_help_popup_clears_entry_status() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "stale".to_owned();
    app.input.cursor = app.input.buffer.len();
    app.status = Status::Info(crate::search::SEARCH_HELP_TEXT.to_owned());
    app.pending_command_response = Some(crate::search::SEARCH_HELP_TEXT.to_owned());
    app.mode = Mode::Popup(PopupKind::CommandResponse);

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.status.text(false), "");
    assert!(app.pending_command_response.is_none());
    assert!(app.input.buffer.is_empty());
    assert_eq!(app.input.cursor, 0);
}

#[tokio::test]
async fn help_popup_selects_command_into_input() {
    let mut app = app_with_rooms(Vec::new());
    app.handle_command(Command::Help).await;

    // Down twice to reach "//<text>" (the new "Alt+Enter" newline entry sits
    // between it and "plain text").
    app.handle_key(KeyEvent::from(KeyCode::Down)).await;
    app.handle_key(KeyEvent::from(KeyCode::Down)).await;
    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.input.buffer, "//");
    assert_eq!(app.input.cursor, "//".len());
    assert_eq!(app.status.text(false), "selected command: //<text>");
}

#[tokio::test]
async fn help_popup_selection_wraps_and_esc_resets_it() {
    let mut app = app_with_rooms(Vec::new());
    app.handle_command(Command::Help).await;

    app.handle_key(KeyEvent::from(KeyCode::Up)).await;

    assert_eq!(app.help_selection, HELP_COMMANDS.len() - 1);

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.popup_scroll, 0);
    assert_eq!(app.help_selection, 0);
}

#[test]
fn shortcuts_popup_lists_all_configurable_shortcuts() {
    let config = TuiConfig::test_default();
    let text = popup_shortcuts_lines(&config.shortcuts)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("F6"));
    assert!(text.contains("Ctrl-N"));
    assert!(text.contains("Ctrl-P"));
    assert!(text.contains("Ctrl-J"));
    assert!(text.contains("Ctrl-K"));
    assert!(text.contains("PageUp"));
    assert!(text.contains("PageDown"));
    assert!(text.contains("select previous / next message"));
}

#[test]
pub(crate) fn new_app_starts_with_one_time_input_help() {
    let app = App::new(
        AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
        None,
        TuiConfig::test_default(),
        Picker::halfblocks(),
    );

    assert!(app.show_input_help);
    assert!(app.input.buffer.is_empty());
}

#[tokio::test]
async fn first_input_action_dismisses_input_help() {
    let mut app = App::new(
        AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
        None,
        TuiConfig::test_default(),
        Picker::halfblocks(),
    );

    app.handle_key(KeyEvent::from(KeyCode::Char('/'))).await;

    assert!(!app.show_input_help);
    assert_eq!(app.input.buffer, "/");
}

#[tokio::test]
async fn room_switch_shortcut_dismisses_input_help_when_no_rooms_exist() {
    let mut app = App::new(
        AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
        None,
        TuiConfig::test_default(),
        Picker::halfblocks(),
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
        .await;

    assert!(!app.show_input_help);
    assert_eq!(app.status.text(false), "no rooms to switch");
}

#[tokio::test]
async fn room_switch_shortcut_abandons_edit_mode() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::Editing {
        event_id: "$old:example.com".to_owned(),
    };
    app.input.buffer = "old body".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
        .await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.input.buffer, "");
    assert_eq!(app.input.cursor, 0);
    assert_eq!(app.status.text(false), "no rooms to switch");
}

#[tokio::test]
async fn search_results_edit_key_reopens_existing_query_form() {
    let mut app = app_with_rooms(Vec::new());
    let edit_form = crate::search::SearchFormState {
        scope: crate::search::SearchScope::SpecificAccount,
        query: "backup key".to_owned(),
        account: "@alice:example.org".to_owned(),
        sender: "@bob:example.org".to_owned(),
        ..Default::default()
    };
    app.search_results = Some(crate::search::SearchResultsState {
        request: crate::search::SearchRequest {
            q: "backup key".to_owned(),
            account_id: None,
            room_id: None,
            sender: Some("@bob:example.org".to_owned()),
            from: None,
            to: None,
            limit: crate::search::DEFAULT_SEARCH_LIMIT,
            cursor: None,
        },
        edit_form: edit_form.clone(),
        results: Vec::new(),
        total: 0,
        next_cursor: None,
        selected: 0,
        loading: false,
        sort_order: crate::search::SearchSortOrder::NewestFirst,
        grouping: crate::search::SearchGrouping::None,
        context_cache: Default::default(),
    });
    app.mode = Mode::SearchResults;

    app.handle_key(KeyEvent::from(KeyCode::Char('e'))).await;

    assert_eq!(app.mode, Mode::SearchForm);
    assert_eq!(app.search_form, edit_form);
    assert_eq!(app.status.text(false), "edit search");

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

    assert_eq!(app.mode, Mode::SearchResults);
    assert!(app.search_results.is_some());
}

#[tokio::test]
async fn search_form_current_account_requires_concrete_account() {
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![
        account_with_id(
            Uuid::from_u128(1),
            "@alice:example.com",
            AccountState::Active,
        ),
        account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
    ];
    app.accounts.selected = AccountSelection::All;
    app.search_form.scope = crate::search::SearchScope::CurrentAccount;
    app.search_form.query = "needle".to_owned();

    app.submit_search_form().await;

    assert_eq!(app.mode, Mode::SearchForm);
    assert_eq!(
        app.search_form.error.as_deref(),
        Some("select an account or choose all accounts")
    );
    assert_eq!(
        app.status.text(false),
        "select an account or choose all accounts"
    );
}
