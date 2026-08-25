//! Fixtures shared by more than one of the sibling test modules.
//!
//! The imports below are re-exported so each sibling can open with a single
//! `use super::support::*;` — a helper moving between modules then does not
//! drag an import list with it.

pub(super) use crate::api::{AccountDto, AccountState, MemberDto, TimelinePage, VerificationFrame};
pub(super) use crate::app::render::message_layout;
pub(super) use crate::app::search_flow::{SearchJumpAction, SearchJumpThreadLoad, SearchOutcome};
pub(super) use crate::command::HELP_COMMANDS;
pub(super) use crate::config::TimeFormat;
pub(super) use crate::ui::{entry_status_text, popup_shortcuts_lines, popup_status_lines};
pub(super) use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::*;

pub(super) fn room(room_id: &str, alias: Option<&str>, name: Option<&str>) -> RoomDto {
    RoomDto {
        account_id: Uuid::nil(),
        account_user_id: Some("@alice:example.com".to_owned()),
        room_id: room_id.to_owned(),
        name: name.map(str::to_owned),
        topic: None,
        avatar_url: None,
        canonical_alias: alias.map(str::to_owned),
        last_activity_ts: 0,
        last_event_id: None,
    }
}

pub(super) fn event_with_id(
    event_id: &str,
    event_type: &str,
    body: Option<&str>,
    content: serde_json::Value,
) -> EventDto {
    event_with_state_key(event_id, event_type, None, body, content)
}

pub(super) fn event_with_state_key(
    event_id: &str,
    event_type: &str,
    state_key: Option<&str>,
    body: Option<&str>,
    content: serde_json::Value,
) -> EventDto {
    EventDto {
        account_id: Uuid::nil(),
        event_id: event_id.to_owned(),
        room_id: "!room:example.com".to_owned(),
        sender: "@alice:example.com".to_owned(),
        state_key: state_key.map(str::to_owned),
        arrival_order: 0,
        origin_ts: 0,
        event_type: event_type.to_owned(),
        content: Some(content),
        body: body.map(str::to_owned),
        relates_to: None,
        redacted: false,
        redaction_event_id: None,
        reactions: None,
        sender_trust: None,
    }
}

pub(super) fn event(event_type: &str, body: Option<&str>, content: serde_json::Value) -> EventDto {
    event_with_id(
        &format!("${event_type}:example.com"),
        event_type,
        body,
        content,
    )
}

pub(super) fn tally(count: i64, me: bool, my_event_ids: &[&str]) -> crate::api::ReactionTally {
    // `count` is the cardinality of `senders` server-side, so a fixture that
    // sets one without the other is a tally no server would send — and the
    // code now keeps the two in lockstep (#220). Synthesize senders to
    // match: the account's own user first when `me`, then filler.
    let mut senders: Vec<String> = Vec::new();
    if me {
        senders.push("@alice:example.com".to_owned());
    }
    while (senders.len() as i64) < count {
        senders.push(format!("@other{}:example.com", senders.len()));
    }
    crate::api::ReactionTally {
        count,
        me,
        senders,
        my_event_ids: my_event_ids.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// A message event carrying a server-aggregated reaction tally — the M8 shape
/// the timeline now returns in place of raw `m.reaction` events.
pub(super) fn message_with_reactions(
    event_id: &str,
    reactions: Vec<(&str, crate::api::ReactionTally)>,
) -> EventDto {
    let mut event = event_with_id(
        event_id,
        "m.room.message",
        Some("message"),
        serde_json::json!({ "msgtype": "m.text", "body": "message" }),
    );
    event.reactions = Some(
        reactions
            .into_iter()
            .map(|(key, tally)| (key.to_owned(), tally))
            .collect(),
    );
    event
}

pub(super) fn app_with_rooms(rooms: Vec<RoomDto>) -> App {
    let mut app = App::new(
        AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
        None,
        TuiConfig::test_default(),
        Picker::halfblocks(),
    );
    app.rooms.rooms = rooms;
    app.show_input_help = false;
    app.status = Status::Info(String::new());
    app
}

pub(super) fn seed_room_caches(app: &mut App, key: &RoomKey) {
    app.rooms.unread.insert(key.clone(), 2);
    app.rooms.display_names.insert(key.clone(), HashMap::new());
    app.room_titles
        .insert(key.clone(), "Cached title".to_owned());
    app.messages.events.insert(
        key.clone(),
        vec![event_with_id(
            "$cached",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        )],
    );
    app.messages
        .history_cursors
        .insert(key.clone(), "cursor".to_owned());
    app.thread_summaries.insert(key.clone(), HashMap::new());
    app.relation_refresh_latest.insert(key.clone(), 1);
    app.members_refresh_after
        .insert(key.clone(), std::time::Instant::now());
    app.unread_threads.insert(key.clone(), HashMap::new());
}

pub(super) fn assert_room_caches_pruned(app: &App, key: &RoomKey) {
    assert!(!app.rooms.unread.contains_key(key));
    assert!(!app.rooms.display_names.contains_key(key));
    assert!(!app.room_titles.contains_key(key));
    assert!(!app.messages.events.contains_key(key));
    assert!(!app.messages.history_cursors.contains_key(key));
    assert!(!app.thread_summaries.contains_key(key));
    assert!(!app.relation_refresh_latest.contains_key(key));
    assert!(!app.members_refresh_after.contains_key(key));
    assert!(!app.unread_threads.contains_key(key));
}

pub(super) async fn spawn_api_stub(responses: Vec<String>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test API stub");
    let address = listener.local_addr().expect("test API stub address");
    tokio::spawn(async move {
        for body in responses {
            let (mut socket, _) = listener.accept().await.expect("accept API request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.expect("read API request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write API response");
        }
    });
    format!("http://{address}")
}

pub(super) fn rooms_response_body(rooms: &[RoomDto]) -> String {
    let data: Vec<_> = rooms
        .iter()
        .map(|room| {
            serde_json::json!({
                "account_id": room.account_id,
                "account_user_id": room.account_user_id,
                "room_id": room.room_id,
                "name": room.name,
                "topic": room.topic,
                "avatar_url": room.avatar_url,
                "canonical_alias": room.canonical_alias,
                "last_activity_ts": room.last_activity_ts,
                "last_event_id": room.last_event_id,
            })
        })
        .collect();
    serde_json::json!({ "data": data }).to_string()
}

pub(super) fn empty_timeline_response_body() -> String {
    serde_json::json!({
        "data": {
            "events": [],
            "next_cursor": null,
        }
    })
    .to_string()
}

pub(super) fn empty_members_response_body() -> String {
    serde_json::json!({ "data": [] }).to_string()
}

pub(super) fn account(user_id: &str, state: AccountState) -> AccountDto {
    account_with_id(Uuid::from_u128(1), user_id, state)
}

pub(super) fn account_with_id(account_id: Uuid, user_id: &str, state: AccountState) -> AccountDto {
    AccountDto {
        account_id,
        user_id: user_id.to_owned(),
        state,
        device_id: None,
        verified: Some(false),
    }
}
