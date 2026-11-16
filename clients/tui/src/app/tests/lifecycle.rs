//! Login, logout, and account deletion: confirmation prompts, argument prompts,
//! and recovery-key entry.

use super::support::*;
use crate::app::*;

fn recover_response_body() -> String {
    serde_json::json!({
        "data": {
            "account_id": Uuid::from_u128(1), "user_id": "@alice:example.com",
            "state": "active", "verified": true,
            "backup": {"exists_on_server": false},
            "backup_action": "export_pending",
            "redecrypt": {
                "selected": 10, "attempted": 2, "decrypted": 0,
                "still_pending": 10, "timed_out": true
            }
        }
    })
    .to_string()
}

#[tokio::test]
async fn recover_http_results_reach_the_completed_response_even_after_refresh_failure() {
    // The one-response stub closes after recover, so follow-up reads fail.
    // Their warning must not replace the completed recovery's own results.
    let base = spawn_api_stub(vec![recover_response_body()]).await;
    let mut app = app_with_rooms(Vec::new());
    app.client = AxonClient::new(base, None);
    let result = app
        .client
        .recover(Uuid::from_u128(1), "test-only-key")
        .await
        .unwrap();
    let before_completion = chrono::Utc::now();
    app.handle_lifecycle_outcome(LifecycleOutcome::Recover {
        account_id: Uuid::from_u128(1),
        user_id: "@alice:example.com".to_owned(),
        result: Ok(result),
    })
    .await;

    let status = app.status.text(false);
    assert!(status.contains("4S secret storage imported"));
    assert!(status.contains("device verified"));
    assert!(status.contains("megolm backup: none on homeserver"));
    assert!(status.contains("megolm backup export pending"));
    assert!(status.contains("10 selected, 2 attempted, 0 decrypted, 10 still pending"));
    assert!(status.contains("timed out (incomplete)"));
    assert!(status.contains("account refresh failed"));
    assert!(status.contains("room refresh failed"));
    assert_eq!(
        app.pending_command_response.as_deref(),
        Some(status.as_str())
    );
    assert!(!status.contains("test-only-key"));
    assert!(!status.contains("recovered encryption keys"));
    let completed_at = app.last_recovery_results[&Uuid::from_u128(1)].completed_at;
    assert!(completed_at >= before_completion);
    assert!(completed_at <= chrono::Utc::now());

    app.display.input_lines = 1;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::prepare(&mut app, frame.area());
            crate::ui::draw(frame, &mut app);
        })
        .unwrap();
    assert_eq!(app.mode, Mode::Popup(PopupKind::CommandResponse));
    let buffer = terminal.backend().buffer();
    let rendered = (0..30)
        .map(|y| {
            (0..120)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("megolm backup export pending"));
    assert!(rendered.contains("10 selected, 2 attempted, 0 decrypted"));
    assert!(rendered.contains("timed out (incomplete)"));
    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
    assert_eq!(app.mode, Mode::Compose);

    // A later account refresh and unrelated command do not erase the result.
    app.set_accounts(vec![account("@alice:example.com", AccountState::Active)]);
    app.handle_command(Command::Help).await;
    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
    app.handle_command(Command::Status).await;
    assert_eq!(app.mode, Mode::Popup(PopupKind::Status));
    let replay = popup_status_lines(&app).join("\n");
    assert!(replay.contains(&format!(
        "Last recovery result (this session; {}):",
        completed_at.format("%Y-%m-%d %H:%M:%S UTC")
    )));
    assert!(replay.contains("megolm backup export pending"));
    assert!(replay.contains("10 selected, 2 attempted, 0 decrypted, 10 still pending"));
    assert!(replay.contains("timed out (incomplete)"));
    assert!(!replay.contains("test-only-key"));
    assert_eq!(
        app.last_recovery_results[&Uuid::from_u128(1)].completed_at,
        completed_at
    );
}

#[tokio::test]
async fn recovery_status_keeps_only_the_latest_attempt_for_each_account_id() {
    let mut app = app_with_rooms(Vec::new());
    let first = account_with_id(
        Uuid::from_u128(1),
        "@alice:example.com",
        AccountState::Active,
    );
    let mut second = account_with_id(
        Uuid::from_u128(2),
        "@alice:example.com",
        AccountState::Active,
    );
    app.set_accounts(vec![first.clone(), second.clone()]);
    assert!(!popup_status_lines(&app)
        .join("\n")
        .contains("Last recovery result"));
    for (account_id, error) in [
        (first.account_id, "first attempt"),
        (second.account_id, "second account attempt"),
        (first.account_id, "latest attempt"),
    ] {
        app.handle_lifecycle_outcome(LifecycleOutcome::Recover {
            account_id,
            user_id: first.user_id.clone(),
            result: Err(error.to_owned()),
        })
        .await;
    }
    assert_eq!(app.last_recovery_results.len(), 2);
    let lines = popup_status_lines(&app);
    let first_heading = lines
        .iter()
        .position(|line| line.contains(&format!("[{}]", first.account_id)))
        .unwrap();
    let second_heading = lines
        .iter()
        .position(|line| line.contains(&format!("[{}]", second.account_id)))
        .unwrap();
    let first_block = lines[first_heading..second_heading].join("\n");
    let second_block = lines[second_heading..].join("\n");
    assert!(first_block.contains("recovery failed for @alice:example.com: latest attempt"));
    assert!(!first_block.contains("first attempt"));
    assert!(!first_block.contains("second account attempt"));
    assert!(second_block.contains("second account attempt"));
    assert!(!second_block.contains("latest attempt"));

    // Logout retains the session result; an authoritative deletion removes it.
    second.state = AccountState::Deactivated;
    app.set_accounts(vec![first.clone(), second.clone()]);
    assert_eq!(app.last_recovery_results.len(), 2);
    assert!(popup_status_lines(&app)
        .join("\n")
        .contains("second account attempt"));
    app.set_accounts(vec![second]);
    assert!(!app.last_recovery_results.contains_key(&first.account_id));
    assert_eq!(app.last_recovery_results.len(), 1);
    assert!(app_with_rooms(Vec::new()).last_recovery_results.is_empty());
}

#[tokio::test]
async fn multiple_recoveries_behind_an_interaction_are_presented_together_once() {
    for mode in [
        Mode::Popup(PopupKind::Help),
        Mode::Popup(PopupKind::CommandResponse),
        Mode::LoginUsername,
    ] {
        let mut app = app_with_rooms(Vec::new());
        app.client = AxonClient::new("http://127.0.0.1:1".to_owned(), None);
        app.mode = mode.clone();
        app.status = Status::from("active interaction".to_owned());
        let previous = (mode == Mode::Popup(PopupKind::CommandResponse))
            .then(|| "previous command result".to_owned());
        app.pending_command_response = previous.clone();
        for (id, user_id) in [(1, "@alice:example.com"), (2, "@bob:example.com")] {
            let mut response: crate::api::ApiResponse<crate::api::RecoverResponse> =
                serde_json::from_str(&recover_response_body()).unwrap();
            response.data.account.account_id = Uuid::from_u128(id);
            response.data.account.user_id = user_id.to_owned();
            app.handle_lifecycle_outcome(LifecycleOutcome::Recover {
                account_id: Uuid::from_u128(id),
                user_id: user_id.to_owned(),
                result: Ok(response.data),
            })
            .await;
        }
        assert_eq!(app.mode, mode);
        assert_eq!(app.status.text(false), "active interaction");
        assert_eq!(app.pending_command_response, previous);
        assert_eq!(app.last_recovery_results.len(), 2);
        let waiting = app.pending_recovery_response.as_deref().unwrap();
        assert!(waiting.contains("4S secret storage imported: @alice:example.com"));
        assert!(waiting.contains("4S secret storage imported: @bob:example.com"));

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
        crate::ui::prepare(&mut app, ratatui::layout::Rect::new(0, 0, 120, 30));
        assert_eq!(app.mode, Mode::Popup(PopupKind::CommandResponse));
        let result = app.pending_command_response.as_deref().unwrap();
        assert!(
            result
                .find("4S secret storage imported: @alice:example.com")
                .unwrap()
                < result
                    .find("4S secret storage imported: @bob:example.com")
                    .unwrap()
        );
        assert_eq!(result.matches("4S secret storage imported:").count(), 2);
        assert_eq!(
            result
                .matches("10 selected, 2 attempted, 0 decrypted, 10 still pending")
                .count(),
            2
        );
        assert!(app.pending_recovery_response.is_none());
        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
        crate::ui::prepare(&mut app, ratatui::layout::Rect::new(0, 0, 120, 30));
        assert_eq!(app.mode, Mode::Compose, "combined results must not reopen");
    }
}

#[tokio::test]
async fn recovery_result_waits_for_an_open_popup_or_prompt_without_losing_its_details() {
    for mode in [
        Mode::Popup(PopupKind::Help),
        Mode::Popup(PopupKind::CommandResponse),
        Mode::LoginUsername,
    ] {
        let mut app = app_with_rooms(Vec::new());
        app.client = AxonClient::new("http://127.0.0.1:1".to_owned(), None);
        app.mode = mode.clone();
        app.status = Status::from("active interaction".to_owned());
        let previous = (mode == Mode::Popup(PopupKind::CommandResponse))
            .then(|| "previous command result".to_owned());
        app.pending_command_response = previous.clone();
        let response: crate::api::ApiResponse<crate::api::RecoverResponse> =
            serde_json::from_str(&recover_response_body()).unwrap();
        app.handle_lifecycle_outcome(LifecycleOutcome::Recover {
            account_id: Uuid::from_u128(1),
            user_id: "@alice:example.com".to_owned(),
            result: Ok(response.data),
        })
        .await;
        crate::ui::prepare(&mut app, ratatui::layout::Rect::new(0, 0, 120, 30));
        assert_eq!(app.mode, mode);
        assert_eq!(app.status.text(false), "active interaction");
        assert_eq!(app.pending_command_response, previous);

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
        crate::ui::prepare(&mut app, ratatui::layout::Rect::new(0, 0, 120, 30));
        assert_eq!(app.mode, Mode::Popup(PopupKind::CommandResponse));
        let result = app.pending_command_response.as_deref().unwrap();
        assert!(result.contains("4S secret storage imported"));
        assert!(result.contains("megolm backup export pending"));
        assert!(result.contains("10 selected, 2 attempted, 0 decrypted, 10 still pending"));

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
        crate::ui::prepare(&mut app, ratatui::layout::Rect::new(0, 0, 120, 30));
        assert_eq!(app.mode, Mode::Compose, "dismissed results must not reopen");
    }
}

#[test]
fn logout_prompts_for_confirmation_when_enabled() {
    let mut app = app_with_rooms(Vec::new());
    app.display.confirm_logout = true;

    app.request_logout(account("@alice:example.com", AccountState::Active));

    assert!(matches!(app.mode, Mode::ConfirmLogout { .. }));
    assert!(app
        .status
        .text(false)
        .contains("Log out @alice:example.com"));
}

#[test]
fn logout_skips_confirmation_when_disabled() {
    let mut app = app_with_rooms(Vec::new());
    app.display.confirm_logout = false;

    app.request_logout(account("@alice:example.com", AccountState::Active));

    // Without a lifecycle sender the spawned logout is a no-op in tests, but
    // we should never have entered the confirmation prompt.
    assert!(!matches!(app.mode, Mode::ConfirmLogout { .. }));
    assert_eq!(app.mode, Mode::Compose);
}

#[tokio::test]
async fn logout_confirmation_cancels_on_no() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::ConfirmLogout {
        account: account("@alice:example.com", AccountState::Active),
    };

    app.handle_key(KeyEvent::from(KeyCode::Char('n'))).await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.status.text(false), "");
}

#[tokio::test]
async fn logout_confirmation_ignores_unrelated_keys() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::ConfirmLogout {
        account: account("@alice:example.com", AccountState::Active),
    };

    app.handle_key(KeyEvent::from(KeyCode::Char('x'))).await;

    assert!(matches!(app.mode, Mode::ConfirmLogout { .. }));
}

#[tokio::test]
async fn delete_confirmation_non_yes_enter_clears_buffer() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::ConfirmDelete {
        account: account("@alice:example.com", AccountState::Active),
    };
    app.input.buffer = "no".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert!(app.input.buffer.is_empty());
}

#[tokio::test]
async fn delete_confirmation_escape_clears_buffer() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::ConfirmDelete {
        account: account("@alice:example.com", AccountState::Active),
    };
    app.input.buffer = "YES".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert!(app.input.buffer.is_empty());
}

#[tokio::test]
async fn in_flight_lifecycle_rejects_new_login_and_logout() {
    let mut app = app_with_rooms(Vec::new());
    app.lifecycle_busy = true;

    app.handle_command(Command::Login {
        username: None,
        password: None,
        homeserver: None,
    })
    .await;
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.status.text(false).contains("already in progress"));

    app.status = Status::Info(String::new());
    app.handle_command(Command::Logout(None)).await;
    assert!(app.status.text(false).contains("already in progress"));

    app.status = Status::Info(String::new());
    app.handle_command(Command::BackupEnable(None)).await;
    assert_eq!(app.mode, Mode::Compose);
    assert!(app.status.text(false).contains("already in progress"));
}

#[tokio::test]
async fn login_without_arguments_prompts_for_username_and_escape_clears_it() {
    let mut app = app_with_rooms(Vec::new());

    app.handle_command(Command::Login {
        username: None,
        password: None,
        homeserver: None,
    })
    .await;
    assert_eq!(app.mode, Mode::LoginUsername);

    app.input.buffer = "@alice:example.com".to_owned();
    app.input.cursor = app.input.buffer.len();
    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert!(app.input.buffer.is_empty());
    assert_eq!(app.status.text(false), "");
}

#[tokio::test]
async fn empty_recovery_key_skips_post_login_recovery() {
    let mut app = app_with_rooms(Vec::new());
    let account = account("@alice:example.com", AccountState::Active);
    app.mode = Mode::RecoveryKey {
        account,
        origin: RecoveryOrigin::PostLogin,
    };

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(
        app.status.text(false),
        "recovery skipped for @alice:example.com"
    );
    assert!(app.input.buffer.is_empty());
}

#[tokio::test]
async fn escape_cancels_standalone_recovery_and_clears_secret() {
    let mut app = app_with_rooms(Vec::new());
    let account = account("@alice:example.com", AccountState::Active);
    app.mode = Mode::RecoveryKey {
        account,
        origin: RecoveryOrigin::Command,
    };
    app.input.buffer = "secret recovery key".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(
        app.status.text(false),
        "recovery cancelled for @alice:example.com"
    );
    assert!(app.input.buffer.is_empty());
}

#[tokio::test]
async fn account_navigation_clears_recovery_key_input() {
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        account_with_id(
            Uuid::from_u128(1),
            "@alice:example.com",
            AccountState::Active,
        ),
        account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
    ]);
    app.mode = Mode::RecoveryKey {
        account: account("@alice:example.com", AccountState::Active),
        origin: RecoveryOrigin::Command,
    };
    app.input.buffer = "secret recovery key".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT))
        .await;

    assert_eq!(app.mode, Mode::Compose);
    assert!(app.input.buffer.is_empty());
}

#[tokio::test]
async fn empty_backup_enable_key_kicks_upload_instead_of_cancelling() {
    let mut app = app_with_rooms(Vec::new());
    let mut account = account("@alice:example.com", AccountState::Active);
    account.verified = Some(true);
    app.mode = Mode::RecoveryKey {
        account,
        origin: RecoveryOrigin::BackupEnable,
    };

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(
        app.status.text(false),
        "enabling megolm backup for @alice:example.com…"
    );
    assert!(app.input.buffer.is_empty());
}

#[tokio::test]
async fn escape_cancels_backup_enable_and_clears_secret() {
    let mut app = app_with_rooms(Vec::new());
    let mut account = account("@alice:example.com", AccountState::Active);
    account.verified = Some(true);
    app.mode = Mode::RecoveryKey {
        account,
        origin: RecoveryOrigin::BackupEnable,
    };
    app.input.buffer = "secret recovery key".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(
        app.status.text(false),
        "backup enable cancelled for @alice:example.com"
    );
    assert!(app.input.buffer.is_empty());
}

#[test]
fn recover_resolution_uses_active_accounts_and_uuid_for_duplicates() {
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    let mut app = app_with_rooms(Vec::new());
    app.accounts.accounts = vec![
        account_with_id(first_id, "@alice:example.com", AccountState::Active),
        account_with_id(second_id, "@alice:example.com", AccountState::Active),
        account_with_id(
            Uuid::from_u128(3),
            "@bob:example.com",
            AccountState::Deactivated,
        ),
    ];

    assert!(matches!(
        app.resolve_recover_target(Some(&second_id.to_string())),
        crate::app::lifecycle::RecoverResolution::Match(AccountDto { account_id, .. })
            if account_id == second_id
    ));
    assert!(matches!(
        app.resolve_recover_target(Some("bob")),
        crate::app::lifecycle::RecoverResolution::Missing
    ));

    app.input.buffer = "/recover alice".to_owned();
    app.complete_input();
    assert_eq!(app.input.buffer, format!("/recover {first_id}"));
    app.complete_input();
    assert_eq!(app.input.buffer, format!("/recover {second_id}"));
}

#[tokio::test]
async fn invalid_login_username_stays_editable() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "alice".to_owned();
    app.input.cursor = app.input.buffer.len();
    app.mode = Mode::LoginUsername;

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(app.mode, Mode::LoginUsername);
    assert_eq!(app.input.buffer, "alice");
    assert!(app.status.text(false).contains("name@domain"));
}

#[tokio::test]
async fn login_username_prompt_canonicalizes_common_email_style() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "alice@example.com".to_owned();
    app.input.cursor = app.input.buffer.len();
    app.mode = Mode::LoginUsername;

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(
        app.mode,
        Mode::LoginPassword {
            username: "@alice:example.com".to_owned(),
            homeserver: None,
        }
    );
    assert!(app.input.buffer.is_empty());
}

#[tokio::test]
async fn login_username_prompt_captures_optional_homeserver() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "@alice:example.com hs.example.org".to_owned();
    app.input.cursor = app.input.buffer.len();
    app.mode = Mode::LoginUsername;

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(
        app.mode,
        Mode::LoginPassword {
            username: "@alice:example.com".to_owned(),
            homeserver: Some("https://hs.example.org".to_owned()),
        }
    );
}

#[tokio::test]
async fn login_username_prompt_rejects_extra_tokens() {
    let mut app = app_with_rooms(Vec::new());
    app.input.buffer = "@alice:example.com hs.example.org junk".to_owned();
    app.input.cursor = app.input.buffer.len();
    app.mode = Mode::LoginUsername;

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    // Stays on the username step with the input intact for correction.
    assert_eq!(app.mode, Mode::LoginUsername);
    assert_eq!(app.input.buffer, "@alice:example.com hs.example.org junk");
    assert!(app.status.text(false).contains("at most"));
}
