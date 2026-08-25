//! What the entry status line shows, and which updates are allowed to overwrite
//! an in-flight or prompting lifecycle status.

use super::support::*;
use crate::app::*;

#[test]
fn entry_status_hides_event_codes_unless_debug_is_enabled() {
    let mut app = app_with_rooms(Vec::new());
    app.status = Status::EventAction {
        debug: "editing $message:example.com - Esc to cancel".to_owned(),
        redacted: "editing message - Esc to cancel",
    };

    assert_eq!(entry_status_text(&app), "editing message - Esc to cancel");

    app.display.debug = true;

    assert_eq!(
        entry_status_text(&app),
        "editing $message:example.com - Esc to cancel"
    );
}

#[test]
fn entry_status_hides_live_socket_status_unless_debug_is_enabled() {
    let mut app = app_with_rooms(Vec::new());
    app.status = Status::Debug("live WebSocket connected".to_owned());

    assert_eq!(entry_status_text(&app), "");

    app.display.debug = true;

    assert_eq!(entry_status_text(&app), "live WebSocket connected");
}

#[test]
fn reconnecting_live_socket_status_is_visible() {
    let mut app = app_with_rooms(Vec::new());

    let action = app.handle_live_frame(LiveFrame::Reconnecting {
        reason: "connection reset".to_owned(),
        delay: std::time::Duration::from_secs(4),
    });

    assert_eq!(action, LiveFrameAction::None);
    assert_eq!(
        entry_status_text(&app),
        "live WebSocket reconnecting in 4s: connection reset"
    );
}

#[test]
fn in_flight_lifecycle_status_ignores_live_socket_updates() {
    let mut app = app_with_rooms(Vec::new());
    app.lifecycle_busy = true;
    app.status = Status::Info("logging in @alice:example.com…".to_owned());

    let action = app.handle_live_frame(LiveFrame::Reconnecting {
        reason: "connection reset".to_owned(),
        delay: std::time::Duration::from_secs(4),
    });

    assert_eq!(action, LiveFrameAction::None);
    assert_eq!(app.status.text(false), "logging in @alice:example.com…");
}

#[test]
fn lifecycle_prompt_status_survives_background_room_refresh() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::LoginUsername;
    app.status = Status::Info("Matrix ID: @user:example.com".to_owned());

    app.apply_room_refresh(Vec::new());

    assert_eq!(app.status.text(false), "Matrix ID: @user:example.com");
}

#[test]
fn in_flight_lifecycle_status_survives_background_room_refresh() {
    let mut app = app_with_rooms(Vec::new());
    app.lifecycle_busy = true;
    app.status = Status::Info("deleting @alice:example.com…".to_owned());

    app.apply_room_refresh(Vec::new());

    assert_eq!(app.status.text(false), "deleting @alice:example.com…");
}
