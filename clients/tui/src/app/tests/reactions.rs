//! Reaction tallies — live frames, local echo, and own-reaction bookkeeping — and
//! the react/unreact input modes built on top of them.

use super::support::*;
use crate::app::*;

/// Build a live `m.reaction` frame annotating `target` with `key`.
fn reaction_frame(event_id: &str, target: &str, key: &str, sender: &str) -> EventDto {
    let mut event = event_with_id(event_id, "m.reaction", None, serde_json::json!({}));
    event.sender = sender.to_owned();
    event.relates_to = Some(serde_json::json!({
        "rel_type": "m.annotation",
        "event_id": target,
        "key": key,
    }));
    event
}

/// A non-`m.reaction` annotation must not reach the badge.
///
/// Matrix permits any event type to carry `m.annotation`; ADR 0033
/// restricts aggregation to `m.reaction`, and #112 tracks the others
/// separately. Folding one into the tally would inflate the badge for a key
/// the server's own aggregation never includes, while the annotation also
/// rendered as its own row.
#[test]
fn a_non_reaction_annotation_does_not_touch_the_tally() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );

    let mut annotation = reaction_frame(
        "$approval:example.com",
        "$target:example.com",
        "\u{1f44d}",
        "@bob:example.com",
    );
    annotation.event_type = "com.example.approval".to_owned();
    app.handle_live_frame(LiveFrame::Timeline(Box::new(annotation)));

    assert_eq!(
        app.selected_reactions().get("$target:example.com"),
        None,
        "only m.reaction annotations aggregate (ADR 0033)"
    );
}

/// A reaction from someone else must reach the badge as it arrives.
///
/// `append_live_event` had a merge branch for edits and none for reactions,
/// so the raw `m.reaction` row was appended (and filtered out of the
/// rendered timeline) while the target's aggregate — which the badge
/// actually renders from — went untouched until a room switch refetched it.
/// A reaction we sent from another device must arrive withdrawable.
///
/// `own_reactions_for` gates on `me && !my_event_ids.is_empty()`, because a
/// withdrawal redacts an id. Setting `me` alone renders the badge as ours
/// while Shift-U reports nothing to remove — until a full room reload.
#[test]
fn a_reaction_from_our_other_device_is_withdrawable() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );
    // This client knows who it is; the reaction was simply not sent here.
    app.live
        .own_senders
        .insert(room.account_id, "@alice:example.com".to_owned());

    app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
        "$from-phone:example.com",
        "$target:example.com",
        "\u{1f44d}",
        "@alice:example.com",
    ))));

    let own = app
        .own_reactions_for("$target:example.com")
        .expect("the target message is loaded");
    assert_eq!(
        own.len(),
        1,
        "the badge is ours, so it must be withdrawable"
    );
    assert_eq!(own[0].key, "\u{1f44d}");
    assert_eq!(
        own[0].event_ids,
        vec!["$from-phone:example.com".to_owned()],
        "withdrawing redacts the reaction event that actually arrived"
    );
}

/// Withdrawing must clear the badge even when the reaction was recorded
/// before this client could name itself.
///
/// `record_reaction` counts our contribution through its fallback arm when
/// the user id is unresolved, adding to `count` without a `senders` entry.
/// If the id resolves before the withdrawal, a removal that gated its
/// decrement on finding us in `senders` found nothing and left a phantom
/// count behind — the same failure family as the bug the record step fixed,
/// living in the fix itself (#220 review, pass 3).
#[test]
fn withdrawing_clears_a_reaction_recorded_before_the_user_id_resolved() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );

    // Unresolvable at record time: every tier stripped.
    app.live.own_senders.clear();
    app.rooms.rooms[0].account_user_id = None;
    let saved_accounts = std::mem::take(&mut app.accounts.accounts);
    app.accounts.client_visible.clear();

    app.apply_local_reaction(
        "$target:example.com",
        "\u{1f44d}",
        "$mine:example.com".to_owned(),
    );
    assert_eq!(
        app.selected_reactions().get("$target:example.com"),
        Some(&vec![("\u{1f44d}".to_owned(), 1)]),
        "the fallback arm counts it once"
    );

    // The id resolves before the user withdraws.
    app.accounts.accounts = saved_accounts;
    app.rooms.rooms[0].account_user_id = Some("@alice:example.com".to_owned());

    app.remove_local_reaction(
        "$target:example.com",
        "\u{1f44d}",
        &["$mine:example.com".to_owned()],
    );

    assert_eq!(
        app.selected_reactions().get("$target:example.com"),
        None,
        "the badge clears rather than leaving a phantom count"
    );
}

/// One person reacting from two devices is one distinct sender, even when
/// this client cannot name itself.
///
/// The local path had a fourth identity tier — a reaction we know is ours
/// names us in its raw row — and the remote path did not, so the two
/// disagreed about what "ours" meant. A second device's reaction with the
/// same key then read as a new sender and the badge counted one person
/// twice (#220 review, pass 4). Realistic rather than contrived:
/// `own_senders` is seeded only by sending a plain message, so an account
/// that only reacts, against a server omitting `account_user_id`, never
/// resolves its id by any other tier.
#[test]
fn one_person_reacting_from_two_devices_counts_once() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );
    // Unresolvable by every tier except the reaction rows themselves.
    app.live.own_senders.clear();
    app.rooms.rooms[0].account_user_id = None;
    app.accounts.accounts.clear();
    app.accounts.client_visible.clear();

    // React here. The id is recorded as ours; nothing yet names us.
    app.apply_local_reaction(
        "$target:example.com",
        "\u{1f44d}",
        "$device-one:example.com".to_owned(),
    );
    // Our own echo lands, so the raw row for that id is now held.
    app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
        "$device-one:example.com",
        "$target:example.com",
        "\u{1f44d}",
        "@alice:example.com",
    ))));
    // The same person reacts again from a second device: a different event
    // id, the same sender.
    app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
        "$device-two:example.com",
        "$target:example.com",
        "\u{1f44d}",
        "@alice:example.com",
    ))));

    assert_eq!(
        app.selected_reactions().get("$target:example.com"),
        Some(&vec![("\u{1f44d}".to_owned(), 1)]),
        "count is distinct senders, and both devices are one sender"
    );
}

/// The WS echo can beat the HTTP response that carries the reaction's id,
/// so the local optimistic apply runs *second*.
///
/// This ordering was the gap the per-symptom patches left: the echo
/// recorded a sender and a count while `me` stayed false, and the local
/// apply then saw `!me` and counted the same reaction again. Both paths now
/// share one guard, so order does not matter.
#[test]
fn an_echo_arriving_before_the_local_apply_counts_once() {
    for own_user_id_known in [true, false] {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![message_with_reactions("$target:example.com", Vec::new())],
        );
        if !own_user_id_known {
            // Strip every tier the resolver consults.
            app.live.own_senders.clear();
            app.rooms.rooms[0].account_user_id = None;
            app.accounts.accounts.clear();
            app.accounts.client_visible.clear();
        }

        // The echo lands first.
        app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
            "$mine:example.com",
            "$target:example.com",
            "\u{1f44d}",
            "@alice:example.com",
        ))));
        // Then the HTTP response resolves and the optimistic patch runs.
        app.apply_local_reaction(
            "$target:example.com",
            "\u{1f44d}",
            "$mine:example.com".to_owned(),
        );

        assert_eq!(
            app.selected_reactions().get("$target:example.com"),
            Some(&vec![("\u{1f44d}".to_owned(), 1)]),
            "one reaction counts once (own_user_id_known = {own_user_id_known})"
        );
    }
}

/// Withdrawing must take the badge to zero, not leave it stuck at one.
///
/// When both paths had recorded us, `count` was decremented once while the
/// sender entry was also dropped — two removals for one contribution in the
/// old code, or none in the new one if removal did not mirror the record.
#[test]
fn withdrawing_after_an_echo_clears_the_badge() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );

    app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
        "$mine:example.com",
        "$target:example.com",
        "\u{1f44d}",
        "@alice:example.com",
    ))));
    app.apply_local_reaction(
        "$target:example.com",
        "\u{1f44d}",
        "$mine:example.com".to_owned(),
    );
    app.remove_local_reaction(
        "$target:example.com",
        "\u{1f44d}",
        &["$mine:example.com".to_owned()],
    );

    assert_eq!(
        app.selected_reactions().get("$target:example.com"),
        None,
        "the badge clears rather than sticking at 1"
    );
}

/// Our own reaction, echoed back over the WS, must not be counted twice —
/// including when the account's user id was never resolved.
///
/// `apply_local_reaction` counts the reaction optimistically and records its
/// event id, but can only add us to `senders` when `own_user_id` is known.
/// An older server without `RoomDto.account_user_id`, or a session where the
/// user has not yet sent a plain message, leaves it `None` — so a
/// sender-only dedup misses the echo and the badge reads 2 for one person's
/// reaction until the next full reload.
#[test]
fn an_own_reaction_echo_is_not_double_counted_without_a_known_user_id() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );
    // The premise, and it takes all three tiers: `own_user_id_for` falls
    // back from the room DTO to `own_senders` to the account list, so
    // clearing only one of them leaves the id resolvable and the test
    // passes for an unrelated reason (#220 review, pass 3).
    app.live.own_senders.clear();
    app.rooms.rooms[0].account_user_id = None;
    app.accounts.accounts.clear();
    app.accounts.client_visible.clear();

    app.apply_local_reaction(
        "$target:example.com",
        "\u{1f44d}",
        "$mine:example.com".to_owned(),
    );
    assert_eq!(
        app.selected_reactions().get("$target:example.com"),
        Some(&vec![("\u{1f44d}".to_owned(), 1)]),
        "the optimistic patch counts it once"
    );

    // The same reaction comes back over the WS.
    app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
        "$mine:example.com",
        "$target:example.com",
        "\u{1f44d}",
        "@alice:example.com",
    ))));

    assert_eq!(
        app.selected_reactions().get("$target:example.com"),
        Some(&vec![("\u{1f44d}".to_owned(), 1)]),
        "the echo is recognised by its event id, not by its sender"
    );
}

#[test]
fn a_remote_reaction_updates_the_target_tally_live() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );

    app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
        "$react:example.com",
        "$target:example.com",
        "\u{1f44d}",
        "@bob:example.com",
    ))));

    let tallies = app.selected_reactions();
    let target = tallies
        .get("$target:example.com")
        .expect("the target message carries a tally");
    assert_eq!(target, &vec![("\u{1f44d}".to_owned(), 1)]);
}

/// The same frame twice — a duplicate delivery, or our own reaction echoed
/// back over the WS after the optimistic patch — must not double-count.
#[test]
fn a_repeated_reaction_from_one_sender_counts_once() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );

    for id in ["$react:example.com", "$again:example.com"] {
        app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
            id,
            "$target:example.com",
            "\u{1f44d}",
            "@bob:example.com",
        ))));
    }

    let tallies = app.selected_reactions();
    assert_eq!(
        tallies.get("$target:example.com"),
        Some(&vec![("\u{1f44d}".to_owned(), 1)]),
        "one sender is one distinct-sender contribution however many frames arrive"
    );
}

/// Two different senders on the same key each contribute once.
#[test]
fn two_senders_on_one_key_both_count() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions("$target:example.com", Vec::new())],
    );

    for (id, sender) in [
        ("$one:example.com", "@bob:example.com"),
        ("$two:example.com", "@carol:example.com"),
    ] {
        app.handle_live_frame(LiveFrame::Timeline(Box::new(reaction_frame(
            id,
            "$target:example.com",
            "\u{1f44d}",
            sender,
        ))));
    }

    let tallies = app.selected_reactions();
    assert_eq!(
        tallies.get("$target:example.com"),
        Some(&vec![("\u{1f44d}".to_owned(), 2)])
    );
}

#[tokio::test]
async fn global_message_navigation_abandons_edit_mode() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_id(
                "$one:example.com",
                "m.room.message",
                Some("first"),
                serde_json::json!({ "msgtype": "m.text", "body": "first" }),
            ),
            event_with_id(
                "$two:example.com",
                "m.room.message",
                Some("second"),
                serde_json::json!({ "msgtype": "m.text", "body": "second" }),
            ),
        ],
    );
    app.messages.selection = Some("$one:example.com".to_owned());
    app.start_edit_selected_message();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .await;

    assert_eq!(app.mode, Mode::MessageList);
    assert_eq!(app.input.buffer, "");
    assert_eq!(app.input.cursor, 0);
    assert_eq!(app.selected_message_id(), Some("$two:example.com"));
}

#[tokio::test]
async fn focus_cycle_abandons_edit_mode_to_compose() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::Editing {
        event_id: "$old:example.com".to_owned(),
    };
    app.input.buffer = "old body".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
        .await;

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.input.buffer, "");
    assert_eq!(app.input.cursor, 0);
}

#[tokio::test]
async fn action_shortcuts_do_not_steal_compose_text_input() {
    for text in ["testing", "editing", "dog", "replying", "Reacting"] {
        let mut app = app_with_rooms(Vec::new());
        for ch in text.chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(ch))).await;
        }

        assert_eq!(app.input.buffer, text);
        assert_eq!(app.status.text(false), "");
    }
}

#[tokio::test]
async fn reaction_tab_completion_shows_and_cycles_matching_emoji() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::Reacting {
        event_id: "$message:example.com".to_owned(),
    };
    app.input.buffer = "face".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    let first_status = app.status.text(false);
    assert_eq!(app.input.react_tab, Some(0));
    assert!(first_status.contains("[1/"));
    assert!(first_status.contains("Tab/Shift-Tab to cycle, Enter to send"));

    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
    let second_status = app.status.text(false);
    assert_eq!(app.input.react_tab, Some(1));
    assert!(second_status.contains("[2/"));
    assert_ne!(second_status, first_status);

    app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;
    assert_eq!(app.input.react_tab, Some(0));
    assert_eq!(app.status.text(false), first_status);

    app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;
    assert_eq!(app.input.react_tab, Some(emoji_matches("face").len() - 1));
    assert!(app.status.text(false).contains(&format!(
        "[{}/{}]",
        emoji_matches("face").len(),
        emoji_matches("face").len()
    )));
}

#[tokio::test]
async fn reaction_submit_rejects_unknown_text_without_leaving_reacting_mode() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::Reacting {
        event_id: "$message:example.com".to_owned(),
    };
    app.input.buffer = "not-a-known-emoji".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

    assert_eq!(
        app.mode,
        Mode::Reacting {
            event_id: "$message:example.com".to_owned()
        }
    );
    assert_eq!(app.input.buffer, "not-a-known-emoji");
    assert_eq!(
        app.status.text(false),
        "no emoji matches 'not-a-known-emoji'"
    );
}

#[test]
fn reaction_input_accepts_only_known_or_selected_emoji() {
    let mut app = app_with_rooms(Vec::new());

    assert_eq!(app.take_reaction_key("🚀"), Some("🚀".to_owned()));
    assert_eq!(app.take_reaction_key("rocket"), Some("🚀".to_owned()));
    assert_eq!(app.take_reaction_key("not-a-known-emoji"), None);

    let matches = emoji_matches("face");
    assert!(matches.len() > 1);
    assert_eq!(app.take_reaction_key("face"), None);

    app.input.react_tab = Some(1);
    assert_eq!(
        app.take_reaction_key("face"),
        Some(matches[1].as_str().to_owned())
    );
}

#[test]
fn react_command_argument_prepares_immediate_reaction_for_most_recent_message() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("message"),
            serde_json::json!({ "msgtype": "m.text", "body": "message" }),
        )],
    );

    assert_eq!(
        app.prepare_reaction("+1"),
        Ok(("$message:example.com".to_owned(), "👍".to_owned()))
    );
    assert_eq!(app.mode, Mode::Compose);
}

#[test]
fn react_command_argument_rejects_unknown_emoji() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("message"),
            serde_json::json!({ "msgtype": "m.text", "body": "message" }),
        )],
    );

    assert_eq!(
        app.prepare_reaction("not-a-known-emoji"),
        Err("unknown or ambiguous emoji: not-a-known-emoji".to_owned())
    );
    assert_eq!(app.mode, Mode::Compose);
}

#[test]
fn own_reactions_group_duplicate_keys_and_ignore_other_or_redacted_reactions() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    // The server-aggregated tally: the account user's own 👍 (two reaction
    // events, deduplicated to one count), a 🎉 from someone else (`me` false),
    // and no redacted 🚀 (the server drops it from the tally).
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions(
            "$message:example.com",
            vec![
                (
                    "👍",
                    tally(1, true, &["$one:example.com", "$two:example.com"]),
                ),
                ("🎉", tally(1, false, &[])),
            ],
        )],
    );

    assert_eq!(
        app.own_reactions_for("$message:example.com"),
        Ok(vec![OwnReaction {
            key: "👍".to_owned(),
            event_ids: vec!["$one:example.com".to_owned(), "$two:example.com".to_owned()],
        }])
    );
}

#[tokio::test]
async fn unreact_with_multiple_reactions_enters_and_cycles_selection_mode() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.selection = Some("$message:example.com".to_owned());
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions(
            "$message:example.com",
            vec![
                ("🚀", tally(1, true, &["$rocket:example.com"])),
                ("👍", tally(1, true, &["$thumb:example.com"])),
            ],
        )],
    );

    app.start_unreact_from_selected_message().await;

    let Mode::Unreacting {
        choices, selected, ..
    } = &app.mode
    else {
        panic!("expected unreact selection mode");
    };
    assert_eq!(choices.len(), 2);
    assert_eq!(*selected, 0);
    let first_status = app.status.text(false);

    app.handle_key(KeyEvent::from(KeyCode::Tab)).await;

    let Mode::Unreacting { selected, .. } = app.mode else {
        panic!("expected unreact selection mode");
    };
    assert_eq!(selected, 1);
    assert_ne!(app.status.text(false), first_status);

    app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;

    let Mode::Unreacting { selected, .. } = app.mode else {
        panic!("expected unreact selection mode");
    };
    assert_eq!(selected, 0);
    assert_eq!(app.status.text(false), first_status);

    app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;

    let Mode::Unreacting { selected, .. } = app.mode else {
        panic!("expected unreact selection mode");
    };
    assert_eq!(selected, 1);

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.status.text(false), "unreact canceled");
}

#[tokio::test]
async fn unreact_hotkey_targets_selected_message() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.selection = Some("$message:example.com".to_owned());
    app.mode = Mode::MessageList;
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions(
            "$message:example.com",
            vec![
                ("🚀", tally(1, true, &["$rocket:example.com"])),
                ("👍", tally(1, true, &["$thumb:example.com"])),
            ],
        )],
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT))
        .await;

    assert!(matches!(app.mode, Mode::Unreacting { .. }));
}

#[test]
fn reaction_badges_come_from_aggregated_tally_and_skip_redacted_messages() {
    // Badges are read from the message's server-aggregated tally; the server
    // has already dropped redacted reactions from it.
    let mut message =
        message_with_reactions("$message:example.com", vec![("👍", tally(1, false, &[]))]);

    let reactions = collect_reactions(std::slice::from_ref(&message));

    assert_eq!(
        reactions.get("$message:example.com"),
        Some(&vec![("👍".to_owned(), 1)])
    );

    // A redacted message shows no badges at all.
    message.redacted = true;
    assert!(collect_reactions(&[message]).is_empty());
}

#[test]
fn local_react_makes_badge_and_unreact_available_before_reload() {
    // A successful react must update the target message's aggregated tally in
    // place: the collapsed timeline no longer carries the raw `m.reaction` row,
    // so without the optimistic patch the badge would not appear and the
    // reaction could not be withdrawn until the next full reload.
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("message"),
            serde_json::json!({ "msgtype": "m.text", "body": "message" }),
        )],
    );

    app.apply_local_reaction("$message:example.com", "👍", "$mine:example.com".to_owned());

    let events = &app.messages.events[&RoomKey::from(&room)];
    assert_eq!(
        collect_reactions(events).get("$message:example.com"),
        Some(&vec![("👍".to_owned(), 1)]),
        "badge appears immediately after react"
    );
    assert_eq!(
        app.own_reactions_for("$message:example.com"),
        Ok(vec![OwnReaction {
            key: "👍".to_owned(),
            event_ids: vec!["$mine:example.com".to_owned()],
        }]),
        "the reaction is withdrawable immediately after react"
    );
}

#[test]
fn local_unreact_clears_badge_and_choice_before_reload() {
    // Withdrawing the only reaction must clear the badge and the unreact choice
    // in place; the redacted raw `m.reaction` row is absent from the collapsed
    // timeline, so the aggregate has to be patched directly.
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions(
            "$message:example.com",
            vec![("👍", tally(1, true, &["$mine:example.com"]))],
        )],
    );

    app.remove_local_reaction(
        "$message:example.com",
        "👍",
        &["$mine:example.com".to_owned()],
    );

    let events = &app.messages.events[&RoomKey::from(&room)];
    assert!(
        collect_reactions(events).is_empty(),
        "badge disappears after the last reaction is withdrawn"
    );
    assert_eq!(
        app.own_reactions_for("$message:example.com"),
        Ok(Vec::new()),
        "no withdrawable reaction remains"
    );
    assert!(
        events[0].reactions.is_none(),
        "an emptied tally is cleared from the row"
    );
}

#[test]
fn local_unreact_keeps_other_senders_count_and_drops_my_contribution() {
    // When others also reacted with the same key, withdrawing my reaction drops
    // only my distinct-sender contribution and clears `me`; the badge persists
    // with the remaining count and is no longer withdrawable by me.
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![message_with_reactions(
            "$message:example.com",
            vec![("👍", tally(2, true, &["$mine:example.com"]))],
        )],
    );

    app.remove_local_reaction(
        "$message:example.com",
        "👍",
        &["$mine:example.com".to_owned()],
    );

    let events = &app.messages.events[&RoomKey::from(&room)];
    assert_eq!(
        collect_reactions(events).get("$message:example.com"),
        Some(&vec![("👍".to_owned(), 1)]),
        "the other sender's reaction still shows"
    );
    assert_eq!(
        app.own_reactions_for("$message:example.com"),
        Ok(Vec::new()),
        "I can no longer withdraw a reaction I removed"
    );
}
