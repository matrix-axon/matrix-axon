//! The room list: visibility filters, refresh reconciliation, cache pruning, and
//! the titles derived from a room summary or its members.

use super::support::*;
use crate::app::*;

#[test]
fn visible_room_indices_filters_dms_groups_unread_and_favorites() {
    // index 0: a DM (no name/alias); 1: a named group; 2: another DM.
    let dm1 = room("!dm1:example.com", None, None);
    let group = room("!g:example.com", None, Some("Team"));
    let dm2 = room("!dm2:example.com", None, None);
    let mut app = app_with_rooms(vec![dm1.clone(), group.clone(), dm2.clone()]);
    // No selection, so the "keep selected visible" rule never interferes.
    app.rooms.selected = None;

    app.room_filter = RoomFilter::All;
    assert_eq!(app.visible_room_indices(), vec![0, 1, 2]);

    app.room_filter = RoomFilter::Dms;
    assert_eq!(app.visible_room_indices(), vec![0, 2]);

    app.room_filter = RoomFilter::Groups;
    assert_eq!(app.visible_room_indices(), vec![1]);

    // Mark the group unread; only it should show under the unread filter.
    app.rooms.unread.insert(RoomKey::from(&group), 4);
    app.room_filter = RoomFilter::Unread;
    assert_eq!(app.visible_room_indices(), vec![1]);

    // Pin dm2; favorites shows only pinned rooms.
    app.pinned_rooms = vec![RoomKey::from(&dm2)];
    app.room_filter = RoomFilter::Favorites;
    assert_eq!(app.visible_room_indices(), vec![2]);

    // Name filter matches on the group's name, case-insensitively.
    app.room_filter = RoomFilter::Name("team".to_owned());
    assert_eq!(app.visible_room_indices(), vec![1]);
}

#[test]
fn room_filter_name_cycles_to_all() {
    assert_eq!(RoomFilter::Name("team".to_owned()).next(), RoomFilter::All);
}

#[test]
fn name_filter_matches_member_derived_room_title() {
    let dm = room("!dm:example.com", None, None);
    let group = room("!g:example.com", None, Some("Team"));
    let mut app = app_with_rooms(vec![dm.clone(), group]);
    app.rooms.selected = None;
    app.room_titles
        .insert(RoomKey::from(&dm), "Alice Example".to_owned());

    app.room_filter = RoomFilter::Name("alice".to_owned());

    assert_eq!(app.visible_room_indices(), vec![0]);
}

#[test]
fn cancel_reediting_name_filter_restores_existing_name_filter() {
    let mut app = app_with_rooms(Vec::new());
    app.room_filter = RoomFilter::Name("team".to_owned());

    app.begin_room_name_filter();
    app.update_room_name_filter("te".to_owned());
    app.cancel_room_name_filter();

    assert_eq!(app.room_filter, RoomFilter::Name("team".to_owned()));
}

#[test]
fn date_jump_counts_as_mid_command() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::DateJump;

    assert!(app.is_mid_command());
}

#[tokio::test]
async fn date_jump_prompt_ignores_message_navigation_shortcuts() {
    let mut app = app_with_rooms(Vec::new());
    app.mode = Mode::DateJump;
    app.input.buffer = "2026-06-25".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .await;

    assert_eq!(app.mode, Mode::DateJump);
    assert_eq!(app.input.buffer, "2026-06-25");
}

#[test]
fn visible_room_indices_always_keeps_selected_room_visible() {
    let dm = room("!dm:example.com", None, None);
    let group = room("!g:example.com", None, Some("Team"));
    let mut app = app_with_rooms(vec![dm, group]);
    // Select the DM, then apply a Groups filter that would hide it.
    app.rooms.selected = Some(0);
    app.room_filter = RoomFilter::Groups;
    // The selected DM stays visible alongside the matching group.
    assert_eq!(app.visible_room_indices(), vec![0, 1]);
}

#[test]
fn media_cache_keys_are_account_scoped() {
    let url = "mxc://example.com/media".to_owned();

    assert_ne!(
        MediaKey::new(Uuid::from_u128(1), url.clone()),
        MediaKey::new(Uuid::from_u128(2), url)
    );
}

#[test]
fn bounded_cache_never_evicts_in_flight_work() {
    let mut cache = HashMap::from([("ready".to_owned(), false), ("fetching".to_owned(), true)]);
    let mut order = VecDeque::from(["ready".to_owned(), "fetching".to_owned()]);

    assert!(evict_lru_where(&mut cache, &mut order, 2, |in_flight| {
        !*in_flight
    }));
    assert!(!cache.contains_key("ready"));
    assert!(cache.contains_key("fetching"));

    cache.insert("encoding".to_owned(), true);
    order.push_back("encoding".to_owned());
    assert!(!evict_lru_where(&mut cache, &mut order, 2, |in_flight| {
        !*in_flight
    }));
    assert_eq!(cache.len(), 2);
}

#[test]
fn account_refresh_preserves_selected_account_by_id() {
    let first_id = Uuid::from_u128(1);
    let selected_id = Uuid::from_u128(2);
    let added_id = Uuid::from_u128(3);
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        account_with_id(first_id, "@first:example.com", AccountState::Active),
        account_with_id(selected_id, "@selected:example.com", AccountState::Active),
    ]);
    app.accounts.selected = AccountSelection::Account(1);

    app.set_accounts(vec![
        account_with_id(selected_id, "@selected:example.com", AccountState::Active),
        account_with_id(added_id, "@added:example.com", AccountState::Active),
    ]);

    assert_eq!(app.active_account_filter(), Some(selected_id));
    assert_eq!(app.accounts.selected, AccountSelection::Account(0));
}

#[test]
fn cli_account_filter_restricts_account_navigation_state() {
    let filter_id = Uuid::from_u128(1);
    let other_id = Uuid::from_u128(2);
    let mut app = App::new(
        AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
        Some(filter_id),
        TuiConfig::test_default(),
        Picker::halfblocks(),
    );

    app.set_accounts(vec![
        account_with_id(filter_id, "@filtered:example.com", AccountState::Active),
        account_with_id(other_id, "@other:example.com", AccountState::Active),
    ]);

    assert_eq!(app.accounts.accounts.len(), 1);
    assert_eq!(app.accounts.accounts[0].account_id, filter_id);
}

#[test]
fn room_refresh_preserves_selected_room_by_key() {
    let first = room("!one:example.com", Some("#one:example.com"), Some("One"));
    let second = room("!two:example.com", Some("#two:example.com"), Some("Two"));
    let mut app = app_with_rooms(vec![first.clone(), second.clone()]);
    app.rooms.selected = Some(1);

    app.apply_room_refresh(vec![second.clone(), first]);

    assert_eq!(
        app.selected_room().map(|room| room.room_id.as_str()),
        Some("!two:example.com")
    );
    assert_eq!(app.rooms.selected, Some(0));
}

#[test]
fn room_refresh_drops_rooms_for_logged_out_accounts() {
    let active_id = Uuid::from_u128(1);
    let logged_out_id = Uuid::from_u128(2);

    let mut active_room = room("!active:example.com", None, Some("Active"));
    active_room.account_id = active_id;
    let mut stale_room = room("!stale:example.com", None, Some("Stale"));
    stale_room.account_id = logged_out_id;

    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        AccountDto {
            account_id: active_id,
            user_id: "@alice:example.com".to_owned(),
            state: AccountState::Active,
            device_id: None,
            verified: Some(false),
        },
        AccountDto {
            account_id: logged_out_id,
            user_id: "@bob:example.com".to_owned(),
            state: AccountState::Deactivated,
            device_id: None,
            verified: Some(false),
        },
    ]);

    app.apply_room_refresh(vec![active_room, stale_room]);

    assert_eq!(
        app.rooms
            .rooms
            .iter()
            .map(|room| room.room_id.as_str())
            .collect::<Vec<_>>(),
        vec!["!active:example.com"]
    );
}

#[test]
fn room_refresh_prunes_caches_for_rooms_that_drop_out() {
    let kept = room("!kept:example.com", None, Some("Kept"));
    let departed = room("!departed:example.com", None, Some("Departed"));
    let departed_key = RoomKey::from(&departed);
    let mut app = app_with_rooms(vec![kept.clone(), departed]);
    app.rooms.selected = Some(1);
    seed_room_caches(&mut app, &departed_key);

    app.apply_room_refresh(vec![kept]);

    assert_room_caches_pruned(&app, &departed_key);
    assert_eq!(
        app.selected_room().map(|room| room.room_id.as_str()),
        Some("!kept:example.com")
    );
}

#[tokio::test]
async fn leave_outcome_prunes_departed_caches_after_post_leave_refresh() {
    let departed = room("!departed:example.com", None, Some("Departed"));
    let next = room("!next:example.com", None, Some("Next"));
    let departed_key = RoomKey::from(&departed);
    let base_url = spawn_api_stub(vec![
        rooms_response_body(std::slice::from_ref(&next)),
        empty_timeline_response_body(),
        empty_members_response_body(),
    ])
    .await;
    let mut app = App::new(
        AxonClient::new(base_url, None),
        None,
        TuiConfig::test_default(),
        Picker::halfblocks(),
    );
    app.rooms.rooms = vec![departed.clone(), next.clone()];
    app.rooms.selected = Some(0);
    seed_room_caches(&mut app, &departed_key);

    app.handle_room_action_outcome(RoomActionOutcome {
        action: PendingRoomAction {
            kind: crate::app::room_actions::RoomActionKind::Leave,
            key: departed_key.clone(),
            room_title: departed.title().to_owned(),
            user_id: None,
            reason: None,
        },
        result: Ok(()),
    })
    .await;

    assert_eq!(
        app.rooms
            .rooms
            .iter()
            .map(|room| room.room_id.as_str())
            .collect::<Vec<_>>(),
        vec!["!next:example.com"]
    );
    assert_eq!(
        app.selected_room().map(|room| room.room_id.as_str()),
        Some("!next:example.com")
    );
    assert_room_caches_pruned(&app, &departed_key);
    assert_eq!(app.status.text(false), "left Departed");
}

#[test]
fn status_lists_active_and_deactivated_accounts() {
    let active_id = Uuid::from_u128(1);
    let logged_out_id = Uuid::from_u128(2);
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        account_with_id(active_id, "@alice:example.com", AccountState::Active),
        account_with_id(logged_out_id, "@bob:example.com", AccountState::Deactivated),
    ]);

    assert_eq!(
        app.accounts
            .accounts
            .iter()
            .map(|account| account.account_id)
            .collect::<Vec<_>>(),
        vec![active_id],
        "active navigation remains active-only"
    );

    let status = popup_status_lines(&app).join("\n");
    assert!(status.contains("@alice:example.com  (logged in, 0 rooms)"));
    assert!(status.contains("@bob:example.com  (logged out, 0 rooms)"));
}

/// `/status` is a user-facing summary; the internal counters and timings
/// belong behind `display.debug`.
///
/// Three places already described them as gated while they were not
/// (`App::protocol_drops`' doc comment, #189's overlay work, and the
/// `Debug overlay diagnostics (display.debug)` row in
/// docs/demo-coverage.md), so this pins the behaviour those claims assume.
#[test]
fn diagnostics_appear_in_status_only_when_debug_is_set() {
    let mut app = app_with_rooms(Vec::new());

    app.display.debug = false;
    let plain = popup_status_lines(&app).join("\n");
    for marker in [
        "Diagnostics",
        "Startup:",
        "Room titles:",
        "Encode drops:",
        "Message layout:",
    ] {
        assert!(
            !plain.contains(marker),
            "{marker} must stay out of /status when debug is off"
        );
    }
    // The user-facing summary is unaffected.
    assert!(plain.contains("Axon server:"));
    assert!(plain.contains("Rooms loaded:"));

    app.display.debug = true;
    let debug = popup_status_lines(&app).join("\n");
    for marker in [
        "Diagnostics",
        "Startup:",
        "Room titles:",
        "Encode drops:",
        "Message layout:",
    ] {
        assert!(
            debug.contains(marker),
            "{marker} must appear when debug is on"
        );
    }
    assert!(
        debug.contains("Axon server:"),
        "and the summary is still there"
    );
}

#[test]
fn status_disambiguates_duplicate_matrix_ids_with_account_ids() {
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        account_with_id(first_id, "@alice:example.com", AccountState::Active),
        account_with_id(second_id, "@alice:example.com", AccountState::Deactivated),
    ]);

    let status = popup_status_lines(&app).join("\n");
    assert!(status.contains(&format!("@alice:example.com  [{first_id}]  (logged in")));
    assert!(status.contains(&format!("@alice:example.com  [{second_id}]  (logged out")));
}

#[test]
fn room_refresh_keeps_rooms_for_accounts_not_yet_listed() {
    // An empty/stale account list must not blank the whole room list.
    let mut unknown_room = room("!unknown:example.com", None, Some("Unknown"));
    unknown_room.account_id = Uuid::from_u128(9);
    let mut app = app_with_rooms(Vec::new());

    app.apply_room_refresh(vec![unknown_room]);

    assert_eq!(app.rooms.rooms.len(), 1);
}

#[test]
fn filtered_room_refresh_does_not_select_a_hidden_room() {
    let visible_account = Uuid::from_u128(1);
    let other_account = Uuid::from_u128(2);
    let mut other_room = room("!other:example.com", None, Some("Other"));
    other_room.account_id = other_account;
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![
        account_with_id(
            visible_account,
            "@visible:example.com",
            AccountState::Active,
        ),
        account_with_id(other_account, "@other:example.com", AccountState::Active),
    ]);
    app.accounts.selected = AccountSelection::Account(0);

    app.apply_room_refresh(vec![other_room]);

    assert_eq!(app.rooms.selected, None);
    assert!(app.selected_room().is_none());
}

#[test]
fn whereami_adds_dm_name_line_for_unnamed_room() {
    // An unnamed room (no name/alias) behaves as a DM.
    let dm = room("!dm:example.com", None, None);
    let mut app = app_with_rooms(vec![dm.clone()]);
    app.rooms.selected = Some(0);

    // No derived title yet → no DM name line.
    let before = crate::ui::popup_room_info_lines(&app);
    assert!(!before.iter().any(|line| line.starts_with("DM name:")));

    // Once a /members fetch resolves the partner, the line appears right after
    // "Name:" without removing any existing information.
    app.room_titles
        .insert(RoomKey::from(&dm), "jamie".to_owned());
    let after = crate::ui::popup_room_info_lines(&app);
    assert!(
        after.iter().any(|line| line == "DM name: jamie"),
        "lines: {after:?}"
    );
    assert!(after.iter().any(|line| line.starts_with("Matrix ID:")));
}

#[test]
fn dm_title_prefers_other_members_name() {
    let members = vec![
        MemberDto {
            user_id: "@me:example.com".to_owned(),
            display_name: Some("Me".to_owned()),
        },
        MemberDto {
            user_id: "@jamie:bostoncoop.net".to_owned(),
            display_name: Some("jamie".to_owned()),
        },
    ];
    assert_eq!(
        dm_title_from_members(Some("@me:example.com"), &members).as_deref(),
        Some("jamie")
    );
}

#[test]
fn dm_title_falls_back_to_localpart_without_display_name() {
    let members = vec![
        MemberDto {
            user_id: "@me:example.com".to_owned(),
            display_name: None,
        },
        MemberDto {
            user_id: "@jamie:bostoncoop.net".to_owned(),
            display_name: None,
        },
    ];
    assert_eq!(
        dm_title_from_members(Some("@me:example.com"), &members).as_deref(),
        Some("@jamie")
    );
}

#[test]
fn dm_title_summarizes_large_rooms_and_skips_note_to_self() {
    let mk = |uid: &str, name: &str| MemberDto {
        user_id: uid.to_owned(),
        display_name: Some(name.to_owned()),
    };
    // Only self → None, so the caller keeps the room id fallback.
    let solo = vec![mk("@me:example.com", "Me")];
    assert_eq!(dm_title_from_members(Some("@me:example.com"), &solo), None);

    // Four others → first three (sorted by user id) plus "+1".
    let many = vec![
        mk("@me:example.com", "Me"),
        mk("@a:x", "Al"),
        mk("@b:x", "Bo"),
        mk("@c:x", "Ci"),
        mk("@d:x", "Di"),
    ];
    assert_eq!(
        dm_title_from_members(Some("@me:example.com"), &many).as_deref(),
        Some("Al, Bo, Ci, +1")
    );
}

#[test]
fn own_sender_is_known_from_room_summary_before_first_send() {
    let account_id = Uuid::from_u128(1);
    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_id = account_id;
    room.account_user_id = Some("@me:example.com".to_owned());
    let mut app = app_with_rooms(vec![room]);

    app.seed_own_senders_from_rooms();

    assert_eq!(
        app.live.own_senders.get(&account_id).map(String::as_str),
        Some("@me:example.com")
    );
}

#[test]
fn room_summary_without_own_sender_still_loads() {
    let account_id = Uuid::from_u128(3);
    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_id = account_id;
    room.account_user_id = None;
    let mut app = app_with_rooms(vec![room]);

    app.seed_own_senders_from_rooms();

    assert!(!app.live.own_senders.contains_key(&account_id));
}

#[test]
fn own_message_color_applies_without_send_echo() {
    let account_id = Uuid::from_u128(2);
    let colors = TuiConfig::test_default().colors;
    let event = EventDto {
        account_id,
        sender: "@me:example.com".to_owned(),
        ..event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        )
    };
    let sender_labels = vec!["@me:example.com".to_owned()];
    let own_senders = HashMap::from([(account_id, "@me:example.com".to_owned())]);
    let lines = message_layout(
        &[&event],
        sender_labels.as_slice(),
        None,
        &colors,
        80,
        &HashMap::new(),
        &own_senders,
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Dense,
        TimeFormat::H24,
        false,
    )
    .lines;

    assert_eq!(lines[1].spans[2].style.fg, Some(colors.own_message_sender));
}

#[test]
fn selected_message_background_applies_only_to_first_line_when_enabled() {
    let colors = TuiConfig::test_default().colors;
    let event = event_with_id(
        "$message:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let lines = message_layout(
        &[&event],
        sender_labels.as_slice(),
        Some("$message:example.com"),
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
        true,
    )
    .lines;

    assert_eq!(lines[1].style.bg, Some(colors.selection_background));
    assert_eq!(lines[1].width(), 80);
    assert_eq!(lines[2].style.bg, None);
}
