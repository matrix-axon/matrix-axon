//! Slash commands that report on the current context — `/whoami`, `/whereami`,
//! message-action commands — and the send-path expansion they share.

use super::support::*;
use crate::app::*;

#[test]
fn send_path_expands_home_slash_for_filesystem_reads() {
    assert_eq!(
        expand_send_path_with_home(
            "~/Downloads/photo.png",
            Some(std::ffi::OsString::from("/home/ada"))
        ),
        PathBuf::from("/home/ada/Downloads/photo.png")
    );
    assert_eq!(
        expand_send_path_with_home(
            "~other/photo.png",
            Some(std::ffi::OsString::from("/home/ada"))
        ),
        PathBuf::from("~other/photo.png")
    );
    assert_eq!(
        expand_send_path_with_home("~/photo.png", None),
        PathBuf::from("~/photo.png")
    );
}

#[tokio::test]
async fn whoami_shows_current_user_id_and_display_name() {
    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_user_id = Some("@me:example.com".to_owned());
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let membership = event_with_state_key(
        "$member:example.com",
        "m.room.member",
        Some("@me:example.com"),
        None,
        serde_json::json!({
            "membership": "join",
            "displayname": "Me Myself"
        }),
    );
    app.rebuild_display_names(&room, &[membership]);

    app.handle_command(Command::Whoami).await;

    assert_eq!(
        app.status.text(false),
        "Matrix ID: @me:example.com; Display Name: Me Myself; Device: unknown"
    );
}

#[tokio::test]
async fn whoami_reports_unknown_display_name() {
    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_user_id = Some("@me:example.com".to_owned());
    let mut app = app_with_rooms(vec![room]);
    app.rooms.selected = Some(0);

    app.handle_command(Command::Whoami).await;

    assert_eq!(
        app.status.text(false),
        "Matrix ID: @me:example.com; Display Name: unknown; Device: unknown"
    );
}

#[tokio::test]
async fn whoami_includes_account_matrix_device_id() {
    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_user_id = Some("@me:example.com".to_owned());
    let account_id = room.account_id;
    let mut app = app_with_rooms(vec![room]);
    app.rooms.selected = Some(0);
    let mut acc = account("@me:example.com", AccountState::Active);
    acc.account_id = account_id;
    acc.device_id = Some("AXONDEV".to_owned());
    app.set_accounts(vec![acc]);

    app.handle_command(Command::Whoami).await;

    assert_eq!(
        app.status.text(false),
        "Matrix ID: @me:example.com; Display Name: unknown; Device: AXONDEV"
    );
}

#[tokio::test]
async fn whoami_device_name_enriches_status_via_lifecycle_outcome() {
    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_user_id = Some("@me:example.com".to_owned());
    let account_id = room.account_id;
    let mut app = app_with_rooms(vec![room]);
    app.rooms.selected = Some(0);
    let mut acc = account("@me:example.com", AccountState::Active);
    acc.account_id = account_id;
    acc.device_id = Some("AXONDEV".to_owned());
    app.set_accounts(vec![acc]);

    app.handle_command(Command::Whoami).await;
    let provisional = app.status.text(false).to_owned();
    assert_eq!(
        provisional,
        "Matrix ID: @me:example.com; Display Name: unknown; Device: AXONDEV"
    );

    app.handle_lifecycle_outcome(LifecycleOutcome::WhoamiDevice {
        provisional: provisional.clone(),
        enriched: "Matrix ID: @me:example.com; Display Name: unknown; Device: axon (AXONDEV)"
            .to_owned(),
    })
    .await;

    assert_eq!(
        app.status.text(false),
        "Matrix ID: @me:example.com; Display Name: unknown; Device: axon (AXONDEV)"
    );
}

#[tokio::test]
async fn whoami_device_name_does_not_clobber_a_later_status() {
    let mut app = app_with_rooms(Vec::new());
    app.status = Status::Info("select a room before using /whoami".to_owned());

    app.handle_lifecycle_outcome(LifecycleOutcome::WhoamiDevice {
        provisional: "Matrix ID: @me:example.com; Display Name: unknown; Device: AXONDEV"
            .to_owned(),
        enriched: "Matrix ID: @me:example.com; Display Name: unknown; Device: axon (AXONDEV)"
            .to_owned(),
    })
    .await;

    assert_eq!(app.status.text(false), "select a room before using /whoami");
}

#[tokio::test]
async fn whoami_requires_selected_room_with_user_id() {
    let mut app = app_with_rooms(Vec::new());

    app.handle_command(Command::Whoami).await;

    assert_eq!(app.status.text(false), "select a room before using /whoami");

    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_user_id = None;
    app.rooms.rooms = vec![room];
    app.rooms.selected = Some(0);

    app.handle_command(Command::Whoami).await;

    assert_eq!(
        app.status.text(false),
        "current user is unavailable for this room"
    );
}

#[tokio::test]
async fn whereami_opens_room_info_popup_for_selected_room() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Room"),
    )]);
    app.rooms.selected = Some(0);
    app.popup_scroll = 4;

    app.handle_command(Command::Whereami).await;

    assert_eq!(app.mode, Mode::Popup(PopupKind::RoomInfo));
    assert_eq!(app.popup_scroll, 0);
}

#[tokio::test]
async fn whereami_requires_selected_room() {
    let mut app = app_with_rooms(Vec::new());

    app.handle_command(Command::Whereami).await;

    assert_eq!(
        app.status.text(false),
        "select a room before using /whereami"
    );
    assert_eq!(app.mode, Mode::Compose);
}

#[tokio::test]
async fn unsupported_and_unknown_commands_report_distinct_statuses() {
    let mut app = app_with_rooms(Vec::new());

    app.handle_command(Command::ApiUnsupported(
        "/join is not supported by the current Axon API".to_owned(),
    ))
    .await;
    assert_eq!(
        app.status.text(false),
        "/join is not supported by the current Axon API"
    );

    app.handle_command(Command::Unknown("unknown command: /frobnicate".to_owned()))
        .await;
    assert_eq!(app.status.text(false), "unknown command: /frobnicate");
}

#[tokio::test]
async fn slash_command_response_waits_for_layout_fit_check() {
    let mut app = app_with_rooms(Vec::new());

    app.handle_command(Command::Whoami).await;

    assert_eq!(
        app.pending_command_response.as_deref(),
        Some("select a room before using /whoami")
    );
}

#[tokio::test]
async fn message_action_commands_target_most_recent_message_without_selection() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_id(
                "$older:example.com",
                "m.room.message",
                Some("older"),
                serde_json::json!({ "msgtype": "m.text", "body": "older" }),
            ),
            event_with_id(
                "$newest:example.com",
                "m.room.message",
                Some("newest"),
                serde_json::json!({ "msgtype": "m.text", "body": "newest" }),
            ),
        ],
    );

    app.handle_command(Command::React(None)).await;
    assert_eq!(app.selected_message_id(), Some("$newest:example.com"));
    assert_eq!(
        app.mode,
        Mode::Reacting {
            event_id: "$newest:example.com".to_owned()
        }
    );

    app.mode = Mode::Compose;
    app.messages.selection = None;
    app.handle_command(Command::Reply).await;
    assert_eq!(app.selected_message_id(), Some("$newest:example.com"));
    assert_eq!(app.pending_reply.as_deref(), Some("$newest:example.com"));

    app.messages.selection = None;
    app.handle_command(Command::Thread).await;
    assert_eq!(app.selected_message_id(), Some("$newest:example.com"));
    assert_eq!(app.pending_thread.as_deref(), Some("$newest:example.com"));
}

#[tokio::test]
async fn message_action_commands_preserve_an_existing_selection() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_id(
                "$selected:example.com",
                "m.room.message",
                Some("selected"),
                serde_json::json!({ "msgtype": "m.text", "body": "selected" }),
            ),
            event_with_id(
                "$newest:example.com",
                "m.room.message",
                Some("newest"),
                serde_json::json!({ "msgtype": "m.text", "body": "newest" }),
            ),
        ],
    );
    app.messages.selection = Some("$selected:example.com".to_owned());

    app.handle_command(Command::React(None)).await;

    assert_eq!(app.selected_message_id(), Some("$selected:example.com"));
    assert_eq!(
        app.mode,
        Mode::Reacting {
            event_id: "$selected:example.com".to_owned()
        }
    );
}
