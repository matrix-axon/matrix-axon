//! Login, logout, and account deletion: confirmation prompts, argument prompts,
//! and recovery-key entry.

use super::support::*;
use crate::app::*;

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
