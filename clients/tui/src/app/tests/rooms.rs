//! The room list: visibility filters, refresh reconciliation, cache pruning, and
//! the titles supplied by room summaries.

use super::support::*;
use crate::app::*;

#[test]
fn visible_room_indices_filters_dms_groups_unread_and_favorites() {
    // index 0: a DM; 1: a named group; 2: another DM.
    let mut dm1 = room("!dm1:example.com", None, None);
    dm1.is_direct = true;
    let group = room("!g:example.com", None, Some("Team"));
    let mut dm2 = room("!dm2:example.com", None, None);
    dm2.is_direct = true;
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

    // Favourite dm2; favorites shows only favourite rooms.
    dm2.tags.push(crate::api::RoomTag {
        name: crate::api::FAVOURITE_TAG.to_owned(),
        order: Some(0.5),
    });
    app.rooms.rooms[2] = dm2.clone();
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
fn name_filter_matches_calculated_room_title() {
    let dm = room("!dm:example.com", None, None);
    let group = room("!g:example.com", None, Some("Team"));
    let mut app = app_with_rooms(vec![dm.clone(), group]);
    app.rooms.selected = None;
    app.rooms.rooms[0].display_name = Some("Alice Example".to_owned());

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
    let mut dm = room("!dm:example.com", None, None);
    dm.is_direct = true;
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
            backup: Default::default(),
        },
        AccountDto {
            account_id: logged_out_id,
            user_id: "@bob:example.com".to_owned(),
            state: AccountState::Deactivated,
            device_id: None,
            verified: Some(false),
            backup: Default::default(),
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
            server_names: Vec::new(),
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
fn status_shows_honest_megolm_backup_snapshot() {
    let mut uploading = account_with_id(
        Uuid::from_u128(1),
        "@alice:example.com",
        AccountState::Active,
    );
    uploading.verified = Some(true);
    uploading.backup = BackupSnapshot {
        exists_on_server: Some(true),
        this_device_uploading: true,
        backup_state: BackupState::Enabled,
        recovery_state: RecoveryState::Enabled,
    };
    let mut none_on_server = account_with_id(
        Uuid::from_u128(2),
        "@bob:example.com",
        AccountState::Deactivated,
    );
    none_on_server.backup = BackupSnapshot {
        exists_on_server: Some(false),
        this_device_uploading: false,
        backup_state: BackupState::Unknown,
        recovery_state: RecoveryState::Incomplete,
    };
    let mut app = app_with_rooms(Vec::new());
    app.set_accounts(vec![uploading, none_on_server]);

    let status = popup_status_lines(&app).join("\n");
    assert!(status.contains("megolm backup: on homeserver; this device uploading; state enabled"));
    assert!(status.contains("4S secret storage: enabled"));
    assert!(status
        .contains("megolm backup: none on homeserver; this device not uploading; state unknown"));
    assert!(status.contains("4S secret storage: incomplete"));
    assert!(
        !status.to_ascii_lowercase().contains("keys recovered"),
        "recovery_state must not be presented as history-key import"
    );
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
fn whereami_shows_calculated_name_for_unnamed_group() {
    let mut group = room("!group:example.com", None, None);
    group.display_name = Some("Alice, Bob".to_owned());
    let mut app = app_with_rooms(vec![group]);
    app.rooms.selected = Some(0);
    let lines = crate::ui::popup_room_info_lines(&app);
    assert!(lines.iter().any(|line| line == "Name: Alice, Bob"));
    assert!(lines
        .iter()
        .any(|line| line == "Matrix ID: !group:example.com"));
    assert!(!lines.iter().any(|line| line.starts_with("DM name:")));
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
        &colors,
        80,
        &HashMap::new(),
        &own_senders,
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Dense,
        TimeFormat::H24,
    )
    .lines;

    assert_eq!(lines[1].spans[2].style.fg, Some(colors.own_message_sender));
}

#[test]
fn pin_room_sets_m_favourite_at_mid_range_order() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Ops"),
    )]);
    app.rooms.selected = Some(0);

    app.pin_room(None);

    let room = &app.rooms.rooms[0];
    assert!(room.is_favourite());
    assert_eq!(room.favourite_order(), Some(0.5));
    assert!(app.is_room_pinned(room));
}

#[test]
fn unpin_room_clears_m_favourite() {
    let mut favourite = room("!room:example.com", None, Some("Ops"));
    favourite.tags.push(crate::api::RoomTag {
        name: crate::api::FAVOURITE_TAG.to_owned(),
        order: Some(0.5),
    });
    let mut app = app_with_rooms(vec![favourite]);
    app.rooms.selected = Some(0);

    app.unpin_room(None);

    assert!(!app.rooms.rooms[0].is_favourite());
    assert!(!app.is_room_pinned(&app.rooms.rooms[0]));
}

#[test]
fn pin_migration_discards_local_pins_when_homeserver_has_favourites() {
    let mut favourite = room("!fav:example.com", None, Some("Fav"));
    favourite.tags.push(crate::api::RoomTag {
        name: crate::api::FAVOURITE_TAG.to_owned(),
        order: Some(0.25),
    });
    let local = room("!local:example.com", None, Some("Local"));
    let mut app = app_with_rooms(vec![favourite.clone(), local.clone()]);
    app.pinned_rooms = vec![RoomKey::from(&local)];
    app.set_accounts(vec![account_with_id(
        Uuid::nil(),
        "@alice:example.com",
        AccountState::Active,
    )]);

    app.maybe_start_pin_migration();

    assert!(app.pinned_rooms.is_empty());
    assert_eq!(app.pin_migration, PinMigration::Done);
}

#[test]
fn a_pin_this_session_does_not_count_as_homeserver_favourites() {
    let first = room("!a:example.com", None, Some("A"));
    let second = room("!b:example.com", None, Some("B"));
    let mut app = app_with_rooms(vec![first.clone(), second.clone()]);
    app.pinned_rooms = vec![RoomKey::from(&first), RoomKey::from(&second)];
    app.server_favourite_accounts = Some(std::collections::HashSet::new());
    app.set_accounts(vec![account_with_id(
        Uuid::nil(),
        "@alice:example.com",
        AccountState::Active,
    )]);
    app.rooms.selected = Some(0);
    app.pin_room(None);

    app.maybe_start_pin_migration();

    assert!(app.rooms.rooms[0].is_favourite());
    assert!(
        app.rooms.rooms[1].is_favourite() || app.pin_migration != PinMigration::Pending,
        "the leftover local pin must be uploaded, not discarded as a homeserver win"
    );
}

#[test]
fn pin_migration_drops_pins_for_rooms_no_longer_in_the_list() {
    let here = room("!here:example.com", None, Some("Here"));
    let mut app = app_with_rooms(vec![here.clone()]);
    app.pinned_rooms = vec![
        RoomKey::from(&here),
        RoomKey {
            account_id: Uuid::nil(),
            room_id: "!gone:example.com".to_owned(),
        },
    ];
    app.set_accounts(vec![account_with_id(
        Uuid::nil(),
        "@alice:example.com",
        AccountState::Active,
    )]);

    app.maybe_start_pin_migration();

    assert!(app.rooms.rooms[0].is_favourite());
    assert!(!app
        .pinned_rooms
        .iter()
        .any(|key| key.room_id == "!gone:example.com"));
}

#[test]
fn pin_migration_uploads_known_accounts_without_waiting_on_others() {
    let mine = Uuid::from_u128(1);
    let other = Uuid::from_u128(2);
    let mut visible = room("!mine:example.com", None, Some("Mine"));
    visible.account_id = mine;
    let mut app = app_with_rooms(vec![visible.clone()]);
    app.pinned_rooms = vec![
        RoomKey::from(&visible),
        RoomKey {
            account_id: other,
            room_id: "!other:example.com".to_owned(),
        },
    ];
    app.set_accounts(vec![
        account_with_id(mine, "@me:example.com", AccountState::Active),
        account_with_id(other, "@other:example.com", AccountState::Active),
    ]);

    app.maybe_start_pin_migration();

    assert!(app.rooms.rooms[0].is_favourite());
    assert_eq!(app.pinned_rooms.len(), 1);
    assert_eq!(app.pinned_rooms[0].account_id, other);
}

#[test]
fn pin_migration_does_not_start_while_a_tag_write_is_in_flight() {
    let room = room("!room:example.com", None, Some("Ops"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.pinned_rooms = vec![RoomKey::from(&room)];
    app.tag_write_busy = true;
    app.set_accounts(vec![account_with_id(
        Uuid::nil(),
        "@alice:example.com",
        AccountState::Active,
    )]);

    app.maybe_start_pin_migration();

    assert_eq!(app.pin_migration, PinMigration::Pending);
    assert_eq!(app.pinned_rooms.len(), 1);
    assert!(!app.rooms.rooms[0].is_favourite());
}

#[tokio::test]
async fn a_failed_pin_keeps_the_local_pin() {
    let room = room("!room:example.com", None, Some("Ops"));
    let key = RoomKey::from(&room);
    let mut app = app_with_rooms(vec![room]);
    app.client = AxonClient::new("http://127.0.0.1:1".to_owned(), None);
    app.pinned_rooms = vec![key.clone()];
    app.rooms.selected = Some(0);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_tag_write_sender(tx);

    app.pin_room(None);

    assert!(
        app.pinned_rooms.contains(&key),
        "must not drop the local pin before the PUT"
    );
    let outcome = rx.recv().await.expect("pin write outcome");
    assert!(outcome.result.is_err());
    app.handle_tag_write_outcome(outcome).await;

    assert!(app.pinned_rooms.contains(&key));
    assert!(!app.rooms.rooms[0].is_favourite());
}

#[test]
fn account_data_changed_patches_tags_and_resorts() {
    let older = room("!a:example.com", None, Some("A"));
    let newer = room("!b:example.com", None, Some("B"));
    let mut app = app_with_rooms(vec![newer.clone(), older.clone()]);
    app.rooms.rooms[0].last_activity_ts = 9;
    app.rooms.rooms[1].last_activity_ts = 1;

    app.apply_account_data_changed(
        Uuid::nil(),
        crate::api::AccountDataChangedDto {
            room_id: Some("!a:example.com".to_owned()),
            event_type: "m.tag".to_owned(),
            content: serde_json::json!({ "tags": { "m.favourite": { "order": 0.1 } } }),
        },
    );

    assert!(app.rooms.rooms[0].is_favourite());
    assert_eq!(app.rooms.rooms[0].room_id, "!a:example.com");
}

#[test]
fn account_data_changed_patches_is_direct() {
    let mut room = room("!dm:example.com", None, Some("Alice"));
    room.is_direct = false;
    let mut app = app_with_rooms(vec![room]);

    app.apply_account_data_changed(
        Uuid::nil(),
        crate::api::AccountDataChangedDto {
            room_id: None,
            event_type: "m.direct".to_owned(),
            content: serde_json::json!({ "@alice:example.com": ["!dm:example.com"] }),
        },
    );

    assert!(app.rooms.rooms[0].is_direct);
    assert!(is_likely_dm(&app.rooms.rooms[0]));
}

#[test]
fn opening_a_room_with_older_history_names_the_ways_to_reach_it() {
    let room = room("!busy:example.com", None, Some("Busy"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);

    // A full page carries a cursor: the status must say how to page back.
    app.apply_timeline_page(
        &room,
        TimelinePage {
            events: Vec::new(),
            next_cursor: Some("older".to_owned()),
        },
        None,
    );
    let text = app.status.text(false);
    assert!(text.contains("PageUp"), "status: {text}");
    assert!(text.contains("/top"), "status: {text}");

    // A short page is the whole room: no hint, and no cursor kept.
    app.apply_timeline_page(
        &room,
        TimelinePage {
            events: Vec::new(),
            next_cursor: None,
        },
        None,
    );
    let text = app.status.text(false);
    assert!(!text.contains("PageUp"), "status: {text}");
    assert!(!text.contains("/top"), "status: {text}");
    assert!(!app
        .messages
        .history_cursors
        .contains_key(&RoomKey::from(&room)));
}

fn display_name_frame(room: &RoomDto, title: &str) -> crate::api::LiveFrame {
    crate::api::LiveFrame::RoomDisplayName {
        account_id: room.account_id,
        payload: crate::api::RoomDisplayNameChangedDto {
            room_id: room.room_id.clone(),
            display_name: title.to_owned(),
        },
    }
}

#[test]
fn live_calculated_title_updates_sort_filter_and_preserves_selection() {
    let mut first = room("!a:x", None, None);
    first.display_name = Some("Alpha".to_owned());
    let mut second = room("!b:x", None, None);
    second.display_name = Some("Bravo".to_owned());
    let mut app = app_with_rooms(vec![first.clone(), second]);
    app.rooms.selected = Some(0);
    app.room_sort = RoomSort::AlphaAsc;
    assert_eq!(
        app.handle_live_frame(display_name_frame(&first, "Zulu")),
        LiveFrameAction::None
    );
    assert_eq!(app.rooms.rooms[0].title(), "Bravo");
    assert_eq!(app.selected_room().unwrap().room_id, first.room_id);
    app.rooms.selected = None;
    app.room_filter = RoomFilter::Name("zul".to_owned());
    assert_eq!(app.visible_room_indices(), vec![1]);
}

#[test]
fn live_calculated_titles_are_account_scoped_and_do_not_override_state() {
    let mut first = room("!same:x", None, Some("Named"));
    first.account_id = Uuid::from_u128(1);
    let mut second = first.clone();
    second.account_id = Uuid::from_u128(2);
    second.name = None;
    second.canonical_alias = Some("#alias:x".to_owned());
    let mut app = app_with_rooms(vec![first.clone(), second]);
    app.handle_live_frame(display_name_frame(&first, "Calculated"));
    assert_eq!(app.rooms.rooms[0].title(), "Named");
    assert_eq!(
        app.rooms.rooms[0].display_name.as_deref(),
        Some("Calculated")
    );
    assert_eq!(app.rooms.rooms[1].display_name, None);
    assert_eq!(app.rooms.rooms[1].title(), "#alias:x");
}

#[test]
fn equal_live_title_survives_an_older_room_response() {
    let mut listed = room("!room:x", None, None);
    listed.display_name = Some("Current".to_owned());
    let mut app = app_with_rooms(vec![listed.clone()]);
    let started = std::time::Instant::now();
    app.handle_live_frame(display_name_frame(&listed, "Current"));
    listed.display_name = Some("Stale".to_owned());
    app.apply_room_refresh(crate::api::RoomListSnapshot {
        rooms: vec![listed],
        started,
        account_filter: None,
    });
    assert_eq!(app.rooms.rooms[0].title(), "Current");
    assert_eq!(app.room_title_updates.len(), 1);
}

#[test]
fn unknown_title_frame_survives_initial_load_without_triggering_a_fetch() {
    let listed = room("!new:x", None, None);
    let mut app = app_with_rooms(Vec::new());
    let started = std::time::Instant::now();
    assert_eq!(
        app.handle_live_frame(display_name_frame(&listed, "Alice, Bob")),
        LiveFrameAction::None
    );
    app.apply_room_refresh(crate::api::RoomListSnapshot {
        rooms: vec![listed],
        started,
        account_filter: None,
    });
    assert_eq!(app.rooms.rooms[0].title(), "Alice, Bob");
}

#[test]
fn response_requested_after_a_frame_wins_and_releases_pending_values() {
    let mut listed = room("!room:x", None, None);
    let mut app = app_with_rooms(Vec::new());
    app.handle_live_frame(display_name_frame(&listed, "Earlier"));
    listed.display_name = Some("Later".to_owned());
    app.apply_room_refresh(vec![listed]);
    assert_eq!(app.rooms.rooms[0].title(), "Later");
    assert!(app.room_title_updates.is_empty());
}

#[test]
fn an_older_concurrent_response_cannot_regress_a_newer_snapshot() {
    let mut listed = room("!room:x", None, None);
    let mut app = app_with_rooms(Vec::new());
    let started = std::time::Instant::now();
    listed.display_name = Some("Latest".to_owned());
    app.apply_room_refresh(vec![listed.clone()]);
    listed.display_name = Some("Old".to_owned());
    app.apply_room_refresh(crate::api::RoomListSnapshot {
        rooms: vec![listed],
        started,
        account_filter: None,
    });
    assert_eq!(app.rooms.rooms[0].title(), "Latest");
}

#[test]
fn unknown_title_backlog_is_bounded_and_requests_one_overflow_refresh() {
    let mut app = app_with_rooms(Vec::new());
    let mut refreshes = 0;
    for i in 0..8300 {
        let listed = room(&format!("!{i}:x"), None, None);
        if app.handle_live_frame(display_name_frame(&listed, "Title"))
            == LiveFrameAction::RefreshRooms
        {
            refreshes += 1;
        }
    }
    assert_eq!(refreshes, 1);
    assert_eq!(app.room_title_updates.len(), 8192);
    app.apply_room_refresh(Vec::new());
    assert!(app.room_title_updates.is_empty());
}

#[test]
fn state_title_updates_are_immediate_and_survive_stale_responses() {
    let mut listed = room("!room:x", None, None);
    listed.display_name = Some("Alice, Bob".to_owned());
    let mut app = app_with_rooms(vec![listed.clone()]);
    let started = std::time::Instant::now();
    for (kind, content, expected) in [
        (
            "m.room.name",
            serde_json::json!({"name": "New name"}),
            "New name",
        ),
        (
            "m.room.canonical_alias",
            serde_json::json!({"alias": "#new:x"}),
            "New name",
        ),
        ("m.room.name", serde_json::json!({}), "#new:x"),
        (
            "m.room.canonical_alias",
            serde_json::json!({}),
            "Alice, Bob",
        ),
    ] {
        let mut event = event_with_state_key("$state:x", kind, Some(""), None, content);
        event.room_id = listed.room_id.clone();
        event.account_id = listed.account_id;
        app.handle_live_frame(crate::api::LiveFrame::Timeline(Box::new(event)));
        assert_eq!(app.rooms.rooms[0].title(), expected);
    }
    listed.name = Some("Old state".to_owned());
    listed.canonical_alias = Some("#old:x".to_owned());
    app.apply_room_refresh(crate::api::RoomListSnapshot {
        rooms: vec![listed],
        started,
        account_filter: None,
    });
    assert_eq!(app.rooms.rooms[0].title(), "Alice, Bob");
}

#[tokio::test]
async fn loading_and_scrolling_unnamed_rooms_do_not_schedule_title_member_reads() {
    let mut app = app_with_rooms(Vec::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_members_sender(tx);
    app.apply_room_refresh(
        (0..500)
            .map(|i| room(&format!("!{i}:x"), None, None))
            .collect::<Vec<_>>(),
    );
    for scroll in [0, 100, 250, 499] {
        app.rooms.scroll = scroll;
        crate::ui::prepare(&mut app, ratatui::layout::Rect::new(0, 0, 100, 30));
        let _ = app.visible_room_indices();
    }
    assert!(app.members_refresh_after.is_empty());
    assert!(rx.try_recv().is_err());
    // Legitimate roster hydration still resolves timeline senders but cannot change a title.
    let listed = app.rooms.rooms[0].clone();
    app.apply_members_outcome(timeline::MembersOutcome {
        room_key: RoomKey::from(&listed),
        members: Some(vec![MemberDto {
            user_id: "@alice:x".to_owned(),
            membership: "join".to_owned(),
            display_name: Some("Alice".to_owned()),
        }]),
    });
    assert_eq!(app.rooms.rooms[0].title(), listed.room_id);
    assert_eq!(
        app.rooms.display_names[&RoomKey::from(&listed)]["@alice:x"],
        "Alice"
    );
}

#[test]
fn inactive_account_title_updates_are_pruned_and_ignored() {
    let listed = room("!room:x", None, None);
    let mut app = app_with_rooms(Vec::new());
    app.handle_live_frame(display_name_frame(&listed, "Pending"));
    assert_eq!(app.room_title_updates.len(), 1);
    app.set_accounts(vec![account_with_id(
        listed.account_id,
        "@me:x",
        AccountState::Deactivated,
    )]);
    assert!(app.room_title_updates.is_empty());
    assert_eq!(
        app.handle_live_frame(display_name_frame(&listed, "Ignored")),
        LiveFrameAction::None
    );
    assert!(app.room_title_updates.is_empty());
}

fn app_with_full_title_backlog() -> App {
    let mut app = app_with_rooms(Vec::new());
    // Keep tests within startup so revealing a room does not fetch its timeline.
    app.bootstrap = BootstrapStage::DeviceState;
    for i in 0..8192 {
        let listed = room(&format!("!pending{i}:x"), None, None);
        assert_eq!(
            app.handle_live_frame(display_name_frame(&listed, "Pending")),
            LiveFrameAction::None
        );
    }
    app
}

#[tokio::test]
async fn title_overflow_after_recovery_start_queues_a_covering_read() {
    let mut app = app_with_full_title_backlog();
    let mut dropped = room("!dropped:x", None, None);
    assert_eq!(
        app.handle_live_frame(display_name_frame(&dropped, "First drop")),
        LiveFrameAction::RefreshRooms
    );
    app.request_rooms_refresh();
    let started = std::time::Instant::now();
    assert_eq!(
        app.handle_live_frame(display_name_frame(&dropped, "Latest drop")),
        LiveFrameAction::None
    );
    // The pending map is still full, so neither value could be retained.
    assert_eq!(app.room_title_updates.len(), 8192);
    dropped.display_name = Some("First drop".to_owned());
    app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Ok(crate::api::RoomListSnapshot {
        rooms: vec![dropped.clone()],
        started,
        account_filter: None,
    })))
    .await;
    assert_eq!(app.rooms.rooms[0].title(), "First drop");
    assert!(
        app.rooms_fetch_inflight,
        "a read covering the later drop must follow the first recovery"
    );
    assert!(
        !app.rooms_fetch_again,
        "only one follow-up should be dispatched"
    );
    dropped.display_name = Some("Latest drop".to_owned());
    app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Ok(vec![dropped].into())))
        .await;
    assert_eq!(app.rooms.rooms[0].title(), "Latest drop");
    assert!(!app.rooms_fetch_inflight);
    assert!(!app.rooms_fetch_again);
    assert!(app.title_update_overflow.is_none());
}

#[tokio::test]
async fn stale_title_overflow_response_queues_recovery_and_rearms_overflow() {
    let stale_started = std::time::Instant::now();
    let mut app = app_with_full_title_backlog();
    // A different room-list read has already applied a later snapshot.
    app.room_snapshot_started = Some(std::time::Instant::now());
    let dropped = room("!dropped:x", None, None);
    assert_eq!(
        app.handle_live_frame(display_name_frame(&dropped, "Dropped")),
        LiveFrameAction::RefreshRooms
    );
    app.request_rooms_refresh();
    app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Ok(crate::api::RoomListSnapshot {
        rooms: Vec::new(),
        started: stale_started,
        account_filter: None,
    })))
    .await;
    assert!(
        app.rooms_fetch_inflight,
        "skipping a stale response must not strand overflow recovery"
    );
    assert!(!app.rooms_fetch_again);
    assert!(app.title_update_overflow.is_none());
    // Skipping did not free the backlog. A further drop can request recovery
    // again and is coalesced behind the newly dispatched read.
    assert_eq!(
        app.handle_live_frame(display_name_frame(&dropped, "Later drop")),
        LiveFrameAction::RefreshRooms
    );
    app.request_rooms_refresh();
    assert!(app.rooms_fetch_again);
}

#[tokio::test]
async fn failed_title_overflow_recovery_allows_a_later_frame_to_retry() {
    let mut app = app_with_full_title_backlog();
    let dropped = room("!dropped:x", None, None);
    assert_eq!(
        app.handle_live_frame(display_name_frame(&dropped, "Dropped")),
        LiveFrameAction::RefreshRooms
    );
    app.request_rooms_refresh();
    app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Err(crate::api::ApiError::Request(
        "offline".to_owned(),
    ))))
    .await;
    assert!(!app.rooms_fetch_inflight);
    assert_eq!(
        app.handle_live_frame(display_name_frame(&dropped, "Retry")),
        LiveFrameAction::RefreshRooms
    );
}

mod pending_membership {
    //! A leave or forget axon answered with a `504` (ADR 0121 decision 5,
    //! issue #743): pending, not failed, and settled by the room list.

    use std::time::{Duration, Instant};

    use super::*;
    use crate::app::room_actions::{
        RoomActionFailure, RoomActionKind, PENDING_MEMBERSHIP_CHECK_DELAYS,
    };

    fn timed_out(verb: &str) -> RoomActionFailure {
        RoomActionFailure {
            message: format!("HTTP 504 Gateway Timeout: {verb} timed out after 30s"),
            timed_out: true,
        }
    }

    fn action(kind: RoomActionKind, slow: &RoomDto) -> PendingRoomAction {
        PendingRoomAction {
            kind,
            key: RoomKey::from(slow),
            room_title: slow.title().to_owned(),
            user_id: None,
            reason: None,
            server_names: Vec::new(),
        }
    }

    fn slow_room() -> RoomDto {
        room("!slow:example.com", None, Some("Slow"))
    }

    /// Another room on the slow room's account, so a read without the slow
    /// room is not an empty one.
    fn other_room() -> RoomDto {
        room("!other:example.com", None, Some("Other"))
    }

    /// A room-list read started now, for `account_filter`.
    fn read(rooms: Vec<RoomDto>, account_filter: Option<Uuid>) -> crate::api::RoomListSnapshot {
        crate::api::RoomListSnapshot {
            rooms,
            started: Instant::now(),
            account_filter,
        }
    }

    fn is_pending(app: &App, key: &RoomKey, kind: RoomActionKind) -> bool {
        app.pending_memberships.contains_key(&(key.clone(), kind))
    }

    /// An app listing the slow room, selected, whose leave just timed out.
    async fn app_with_pending(kind: RoomActionKind) -> (App, RoomKey) {
        let slow = slow_room();
        let mut app = app_with_rooms(vec![slow.clone()]);
        app.rooms.selected = Some(0);
        app.handle_room_action_outcome(RoomActionOutcome {
            action: action(kind, &slow),
            result: Err(timed_out(kind_verb(kind))),
        })
        .await;
        (app, RoomKey::from(&slow))
    }

    fn kind_verb(kind: RoomActionKind) -> &'static str {
        match kind {
            RoomActionKind::Forget => "forget",
            _ => "leave",
        }
    }

    /// Run every check of the schedule and the grace after it.
    fn run_schedule(app: &mut App, from: Instant) -> Instant {
        let mut now = from;
        for delay in PENDING_MEMBERSHIP_CHECK_DELAYS {
            now += delay;
            app.poll_pending_memberships(now);
        }
        now += Duration::from_secs(60);
        app.poll_pending_memberships(now);
        now
    }

    #[tokio::test]
    async fn a_504_from_leave_is_pending_not_a_failure() {
        let (app, key) = app_with_pending(RoomActionKind::Leave).await;

        let status = app.status.text(false);
        assert!(status.contains("hasn't finished leaving Slow"), "{status}");
        assert!(!status.contains("failed"), "{status}");
        assert!(app
            .pending_memberships
            .contains_key(&(key, RoomActionKind::Leave)));
        // The room stays: only the room list can say the leave went through.
        assert_eq!(app.rooms.rooms.len(), 1);
    }

    #[tokio::test]
    async fn a_502_from_leave_is_still_a_failure() {
        let slow = slow_room();
        let mut app = app_with_rooms(vec![slow.clone()]);
        app.handle_room_action_outcome(RoomActionOutcome {
            action: action(RoomActionKind::Leave, &slow),
            result: Err(RoomActionFailure {
                message: "HTTP 502 Bad Gateway: M_UNKNOWN".to_owned(),
                timed_out: false,
            }),
        })
        .await;

        assert_eq!(
            app.status.text(false),
            "/leave failed in Slow: HTTP 502 Bad Gateway: M_UNKNOWN"
        );
        assert!(app.pending_memberships.is_empty());
    }

    #[tokio::test]
    async fn a_504_from_invite_is_still_a_failure() {
        let slow = slow_room();
        let mut app = app_with_rooms(vec![slow.clone()]);
        let mut invite = action(RoomActionKind::Invite, &slow);
        invite.user_id = Some("@bob:example.com".to_owned());
        app.handle_room_action_outcome(RoomActionOutcome {
            action: invite,
            result: Err(timed_out("invite")),
        })
        .await;

        assert!(app.status.text(false).starts_with("/invite failed in Slow"));
        assert!(app.pending_memberships.is_empty());
    }

    #[tokio::test]
    async fn holds_back_a_second_leave_until_the_first_check() {
        let (mut app, _) = app_with_pending(RoomActionKind::Leave).await;

        app.start_leave_room();
        assert_eq!(app.mode, Mode::Compose, "no confirmation, nothing sent");
        assert!(app.status.text(false).contains("no need to leave again"));

        app.poll_pending_memberships(Instant::now() + PENDING_MEMBERSHIP_CHECK_DELAYS[0]);
        app.start_leave_room();
        assert!(
            matches!(app.mode, Mode::ConfirmRoomAction { .. }),
            "a deliberate retry goes through after the first check"
        );
    }

    #[tokio::test]
    async fn a_check_rereads_the_room_list_and_a_missing_room_settles_it() {
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;
        assert!(!app.rooms_fetch_inflight);

        app.poll_pending_memberships(Instant::now() + PENDING_MEMBERSHIP_CHECK_DELAYS[0]);
        assert!(app.rooms_fetch_inflight, "the check asks for the room list");

        app.apply_room_refresh(vec![other_room()]);
        assert!(!is_pending(&app, &key, RoomActionKind::Leave));
        assert_eq!(app.status.text(false), "left Slow");
    }

    #[tokio::test]
    async fn a_list_read_for_another_account_settles_nothing() {
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;
        let elsewhere = Uuid::from_u128(7);
        let mut theirs = other_room();
        theirs.account_id = elsewhere;
        // The filter the read went out with decides, not the one now in force:
        // the user cleared it while that read was in flight.
        app.account_filter = None;

        app.apply_room_refresh(read(vec![theirs], Some(elsewhere)));

        assert!(is_pending(&app, &key, RoomActionKind::Leave));
    }

    #[tokio::test]
    async fn a_read_for_every_account_settles_whatever_the_filter_is_now() {
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;
        app.account_filter = Some(Uuid::from_u128(7));

        app.apply_room_refresh(read(vec![other_room()], None));

        assert!(!is_pending(&app, &key, RoomActionKind::Leave));
    }

    #[tokio::test]
    async fn an_empty_room_list_settles_nothing() {
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;

        app.apply_room_refresh(Vec::new());

        assert!(is_pending(&app, &key, RoomActionKind::Leave));
    }

    #[tokio::test]
    async fn a_read_started_before_the_504_settles_nothing() {
        let started = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("a monotonic clock a second past its origin");
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;

        app.apply_room_refresh(crate::api::RoomListSnapshot {
            rooms: vec![other_room()],
            started,
            account_filter: None,
        });

        assert!(is_pending(&app, &key, RoomActionKind::Leave));
    }

    #[tokio::test]
    async fn checks_follow_the_schedule_not_the_tick_that_ran_them() {
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;
        let since = app.pending_memberships[&(key.clone(), RoomActionKind::Leave)].since;

        // The first check runs 5 s late.
        app.poll_pending_memberships(
            since + PENDING_MEMBERSHIP_CHECK_DELAYS[0] + Duration::from_secs(5),
        );

        assert_eq!(
            app.pending_memberships[&(key, RoomActionKind::Leave)].next_check,
            since + PENDING_MEMBERSHIP_CHECK_DELAYS[0] + PENDING_MEMBERSHIP_CHECK_DELAYS[1]
        );
    }

    #[tokio::test]
    async fn a_leave_still_listed_after_the_schedule_may_have_failed() {
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;
        app.apply_room_refresh(vec![slow_room()]);

        run_schedule(&mut app, Instant::now());

        assert!(!app
            .pending_memberships
            .contains_key(&(key, RoomActionKind::Leave)));
        let status = app.status.text(false);
        assert!(status.contains("Slow is still listed"), "{status}");
    }

    #[tokio::test]
    async fn a_window_with_no_read_of_the_room_ends_silently() {
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;
        let elsewhere = Uuid::from_u128(7);
        let mut theirs = other_room();
        theirs.account_id = elsewhere;
        // Every read went out for another account, so none could list Slow.
        app.apply_room_refresh(read(vec![theirs], Some(elsewhere)));
        app.status = Status::from("unrelated".to_owned());

        run_schedule(&mut app, Instant::now());

        assert!(!is_pending(&app, &key, RoomActionKind::Leave));
        assert_eq!(app.status.text(false), "unrelated");
    }

    #[tokio::test]
    async fn holds_back_a_forget_while_the_leave_is_held_back() {
        let (mut app, _) = app_with_pending(RoomActionKind::Leave).await;

        app.start_forget_room(None);
        assert_eq!(app.mode, Mode::Compose, "no confirmation, nothing sent");
        assert!(
            app.status
                .text(false)
                .contains("only be forgotten once it is left"),
            "{}",
            app.status.text(false)
        );

        app.poll_pending_memberships(Instant::now() + PENDING_MEMBERSHIP_CHECK_DELAYS[0]);
        app.start_forget_room(None);
        assert!(
            matches!(app.mode, Mode::ConfirmRoomAction { .. }),
            "past the first check, a forget is the user's call"
        );
    }

    #[tokio::test]
    async fn a_pending_forget_waits_the_schedule_out_without_reading_the_list() {
        let (mut app, key) = app_with_pending(RoomActionKind::Forget).await;
        assert!(app
            .status
            .text(false)
            .contains("hasn't finished forgetting Slow"));

        // A forget changes nothing the list shows, so a refresh settles nothing.
        app.apply_room_refresh(Vec::new());
        assert!(app
            .pending_memberships
            .contains_key(&(key.clone(), RoomActionKind::Forget)));

        app.rooms_fetch_inflight = false;
        run_schedule(&mut app, Instant::now());
        assert!(!app.rooms_fetch_inflight);
        assert!(!app
            .pending_memberships
            .contains_key(&(key, RoomActionKind::Forget)));
    }

    #[tokio::test]
    async fn a_forget_timing_out_keeps_a_pending_leave() {
        let (mut app, key) = app_with_pending(RoomActionKind::Leave).await;
        let slow = slow_room();
        app.handle_room_action_outcome(RoomActionOutcome {
            action: action(RoomActionKind::Forget, &slow),
            result: Err(timed_out("forget")),
        })
        .await;

        assert!(app
            .pending_memberships
            .contains_key(&(key.clone(), RoomActionKind::Leave)));
        assert!(app
            .pending_memberships
            .contains_key(&(key, RoomActionKind::Forget)));
    }

    #[tokio::test]
    async fn an_answer_to_a_later_forget_ends_a_pending_leave() {
        let slow = slow_room();
        // A lagging list still shows the room, so only the answer itself can
        // end the pending leave.
        let base_url = spawn_api_stub(vec![rooms_response_body(std::slice::from_ref(&slow))]).await;
        let mut app = App::new(
            AxonClient::new(base_url, None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );
        app.rooms.rooms = vec![slow.clone()];
        app.handle_room_action_outcome(RoomActionOutcome {
            action: action(RoomActionKind::Leave, &slow),
            result: Err(timed_out("leave")),
        })
        .await;

        app.handle_room_action_outcome(RoomActionOutcome {
            action: action(RoomActionKind::Forget, &slow),
            result: Ok(()),
        })
        .await;

        assert!(app.pending_memberships.is_empty());
        assert_eq!(app.status.text(false), "forgot Slow");
    }
}

mod join {
    //! `/join` picks an account, then a `200` or a listed `504` opens the room.
    //! A room missing from that first list is watched for a few seconds.
    //! An unanswered `504` does not become a pending membership.

    use std::time::{Duration, Instant};

    use super::*;
    use crate::app::room_actions::{RoomActionFailure, RoomActionKind, JOIN_OPEN_READ_DELAYS};

    fn join_action(account_id: Uuid, typed: &str, room_id: &str) -> PendingRoomAction {
        PendingRoomAction {
            kind: RoomActionKind::Join,
            key: RoomKey {
                account_id,
                room_id: room_id.to_owned(),
            },
            room_title: typed.to_owned(),
            user_id: None,
            reason: None,
            server_names: Vec::new(),
        }
    }

    fn failure(message: &str, timed_out: bool) -> RoomActionFailure {
        RoomActionFailure {
            message: message.to_owned(),
            timed_out,
        }
    }

    fn join_response(room_id: &str) -> String {
        serde_json::json!({ "data": { "room_id": room_id } }).to_string()
    }

    async fn app_for(responses: Vec<String>) -> App {
        let base_url = spawn_api_stub(responses).await;
        let mut app = App::new(
            AxonClient::new(base_url, None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );
        app.show_input_help = false;
        app.status = Status::Info(String::new());
        app
    }

    async fn app_joining(
        responses: Vec<String>,
    ) -> (App, tokio::sync::mpsc::UnboundedReceiver<RoomActionOutcome>) {
        let mut app = app_for(responses).await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_room_action_sender(tx);
        (app, rx)
    }

    #[tokio::test]
    async fn join_with_all_and_two_accounts_asks_for_an_account() {
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            account_with_id(
                Uuid::from_u128(1),
                "@alice:example.com",
                AccountState::Active,
            ),
            account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
        ]);

        app.handle_command(crate::command::parse("/join #room:example.com"))
            .await;

        assert_eq!(
            app.status.text(false),
            "choose an account with /account before using /join"
        );
        assert!(!app.room_action_busy);
        assert!(matches!(app.accounts.selected, AccountSelection::All));
    }

    #[tokio::test]
    async fn join_without_an_account_asks_for_login() {
        let mut app = app_with_rooms(Vec::new());

        app.handle_command(crate::command::parse("/join #room:example.com"))
            .await;

        assert_eq!(app.status.text(false), "log in before using /join");
        assert!(!app.room_action_busy);
    }

    #[tokio::test]
    async fn join_with_one_account_uses_that_account() {
        let account_id = Uuid::from_u128(4);
        let (mut app, mut rx) = app_joining(vec![join_response("!resolved:example.com")]).await;
        app.set_accounts(vec![account_with_id(
            account_id,
            "@alice:example.com",
            AccountState::Active,
        )]);

        app.handle_command(crate::command::parse("/join !room:example.com"))
            .await;

        let outcome = rx.recv().await.expect("join outcome");
        assert_eq!(outcome.action.key.account_id, account_id);
        assert_eq!(outcome.action.key.room_id, "!resolved:example.com");
        assert_eq!(outcome.action.server_names, vec!["example.com".to_owned()]);
        assert!(outcome.result.is_ok());
        assert!(app.room_action_busy);
        assert_eq!(app.status.text(false), "joining !room:example.com...");
    }

    #[tokio::test]
    async fn join_with_the_account_filter_uses_that_account() {
        let alice = Uuid::from_u128(1);
        let bob = Uuid::from_u128(2);
        let (mut app, mut rx) = app_joining(vec![join_response("!resolved:example.com")]).await;
        app.set_accounts(vec![
            account_with_id(alice, "@alice:example.com", AccountState::Active),
            account_with_id(bob, "@bob:example.com", AccountState::Active),
        ]);
        app.accounts.selected = AccountSelection::Account(1);

        app.handle_command(crate::command::parse("/join #room:example.com"))
            .await;

        let outcome = rx.recv().await.expect("join outcome");
        assert_eq!(outcome.action.key.account_id, bob);
        assert!(outcome.action.server_names.is_empty());
        assert_eq!(outcome.action.key.room_id, "!resolved:example.com");
        assert!(outcome.result.is_ok());
    }

    #[tokio::test]
    async fn join_with_only_a_launch_filter_uses_that_account() {
        let account_id = Uuid::from_u128(9);
        let (mut app, mut rx) = app_joining(vec![join_response("!resolved:example.com")]).await;
        app.account_filter = Some(account_id);

        app.handle_command(crate::command::parse("/join #room:example.com"))
            .await;

        let outcome = rx.recv().await.expect("join outcome");
        assert_eq!(outcome.action.key.account_id, account_id);
        assert!(outcome.result.is_ok());
    }

    #[tokio::test]
    async fn join_success_refreshes_and_selects_when_the_room_is_listed() {
        let other = room("!other:example.com", None, Some("Other"));
        let ops = room("!ops:example.com", Some("#ops:example.com"), Some("Ops"));
        let mut app = app_for(vec![
            rooms_response_body(&[other.clone(), ops.clone()]),
            empty_timeline_response_body(),
            empty_members_response_body(),
        ])
        .await;
        app.rooms.rooms = vec![other];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(ops.account_id, "#ops:example.com", &ops.room_id),
            result: Ok(()),
        })
        .await;

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!ops:example.com")
        );
        assert_eq!(app.status.text(false), "joined Ops");
        assert!(app.pending_memberships.is_empty());
        assert!(app.pending_join_open.is_none());
    }

    #[tokio::test]
    async fn join_success_keeps_the_selection_when_the_room_is_not_listed() {
        let other = room("!other:example.com", None, Some("Other"));
        let mut app = app_for(vec![rooms_response_body(std::slice::from_ref(&other))]).await;
        app.rooms.rooms = vec![other];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(Uuid::nil(), "#ops:example.com", "!ops:example.com"),
            result: Ok(()),
        })
        .await;

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!other:example.com")
        );
        assert_eq!(
            app.status.text(false),
            "joined #ops:example.com; it isn't in the room list yet"
        );
        assert!(app.pending_memberships.is_empty());
        let watch = app.pending_join_open.as_ref().expect("join watch");
        assert_eq!(watch.target, "!ops:example.com");
        assert!(!watch.ready);
        assert_eq!(watch.reads_remaining, JOIN_OPEN_READ_DELAYS.len() as u8);
    }

    #[tokio::test]
    async fn a_504_from_join_does_not_decide_or_enter_pending_membership() {
        let other = room("!other:example.com", None, Some("Other"));
        let mut app = app_for(vec![rooms_response_body(std::slice::from_ref(&other))]).await;
        app.rooms.rooms = vec![other];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(Uuid::nil(), "#ops:example.com", "#ops:example.com"),
            result: Err(failure(
                "HTTP 504 Gateway Timeout: join timed out after 30s",
                true,
            )),
        })
        .await;

        let status = app.status.text(false);
        assert_eq!(
            status,
            "the homeserver hasn't answered for #ops:example.com"
        );
        assert!(!status.contains("failed"), "{status}");
        assert!(!status.contains("joined"), "{status}");
        assert!(app.pending_memberships.is_empty());
        let watch = app.pending_join_open.as_ref().expect("join watch");
        assert_eq!(watch.target, "#ops:example.com");
        assert!(!watch.ready);
        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!other:example.com")
        );
    }

    #[tokio::test]
    async fn a_504_from_join_selects_when_the_room_list_contains_the_alias() {
        let ops = room("!ops:example.com", Some("#ops:example.com"), Some("Ops"));
        let mut app = app_for(vec![
            rooms_response_body(std::slice::from_ref(&ops)),
            empty_timeline_response_body(),
            empty_members_response_body(),
        ])
        .await;

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(ops.account_id, "#ops:example.com", "#ops:example.com"),
            result: Err(failure(
                "HTTP 504 Gateway Timeout: join timed out after 30s",
                true,
            )),
        })
        .await;

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!ops:example.com")
        );
        assert_eq!(app.status.text(false), "joined Ops");
        assert!(app.pending_memberships.is_empty());
        assert!(app.pending_join_open.is_none());
    }

    #[tokio::test]
    async fn a_504_with_a_failed_refresh_ignores_the_room_already_on_screen() {
        let listed = room("!slow:example.com", Some("#slow:example.com"), Some("Slow"));
        let mut app = app_for(vec!["not-json".to_owned()]).await;
        app.rooms.rooms = vec![listed.clone()];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(listed.account_id, "#slow:example.com", "#slow:example.com"),
            result: Err(failure(
                "HTTP 504 Gateway Timeout: join timed out after 30s",
                true,
            )),
        })
        .await;

        let status = app.status.text(false);
        assert_eq!(
            status,
            "the homeserver hasn't answered for #slow:example.com"
        );
        assert!(!status.contains("failed"), "{status}");
        assert!(!status.contains("joined"), "{status}");
        assert!(app.pending_memberships.is_empty());
        assert!(
            app.pending_join_open.is_some(),
            "a failed refresh does not prove the join, but a later list can"
        );
        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!slow:example.com")
        );
    }

    #[tokio::test]
    async fn a_502_from_join_reports_failure() {
        let other = room("!other:example.com", None, Some("Other"));
        let mut app = app_with_rooms(vec![other]);
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(Uuid::nil(), "#ops:example.com", "#ops:example.com"),
            result: Err(failure("HTTP 502 Bad Gateway: M_UNKNOWN", false)),
        })
        .await;

        assert_eq!(
            app.status.text(false),
            "/join failed for #ops:example.com: HTTP 502 Bad Gateway: M_UNKNOWN"
        );
        assert!(app.pending_memberships.is_empty());
        assert!(app.pending_join_open.is_none());
        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!other:example.com")
        );
    }

    #[tokio::test]
    async fn a_client_timeout_from_join_reports_failure() {
        let mut app = app_with_rooms(Vec::new());

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(Uuid::nil(), "#ops:example.com", "#ops:example.com"),
            result: Err(failure("request timed out", false)),
        })
        .await;

        assert_eq!(
            app.status.text(false),
            "/join failed for #ops:example.com: request timed out"
        );
        assert!(app.pending_memberships.is_empty());
        assert!(app.pending_join_open.is_none());
    }

    fn snapshot(rooms: Vec<crate::api::RoomDto>, started: Instant) -> crate::api::RoomListSnapshot {
        crate::api::RoomListSnapshot {
            rooms,
            started,
            account_filter: None,
        }
    }

    #[tokio::test]
    async fn join_success_opens_when_a_later_list_contains_the_room() {
        let other = room("!other:example.com", None, Some("Other"));
        let ops = room("!ops:example.com", Some("#ops:example.com"), Some("Ops"));
        let mut app = app_for(vec![
            rooms_response_body(std::slice::from_ref(&other)),
            empty_timeline_response_body(),
            empty_members_response_body(),
        ])
        .await;
        app.rooms.rooms = vec![other.clone()];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(ops.account_id, "#ops:example.com", &ops.room_id),
            result: Ok(()),
        })
        .await;
        let since = app.pending_join_open.as_ref().expect("watch").since;
        app.apply_room_refresh(snapshot(vec![other, ops], since + Duration::from_millis(1)));
        app.poll_pending_join_open(Instant::now()).await;

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!ops:example.com")
        );
        assert_eq!(app.status.text(false), "joined Ops");
        assert!(app.pending_join_open.is_none());
        assert!(app.pending_memberships.is_empty());
    }

    #[tokio::test]
    async fn a_504_opens_when_a_later_list_contains_the_alias() {
        let other = room("!other:example.com", None, Some("Other"));
        let ops = room("!ops:example.com", Some("#ops:example.com"), Some("Ops"));
        let mut app = app_for(vec![
            rooms_response_body(std::slice::from_ref(&other)),
            empty_timeline_response_body(),
            empty_members_response_body(),
        ])
        .await;
        app.rooms.rooms = vec![other.clone()];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(ops.account_id, "#ops:example.com", "#ops:example.com"),
            result: Err(failure(
                "HTTP 504 Gateway Timeout: join timed out after 30s",
                true,
            )),
        })
        .await;
        let since = app.pending_join_open.as_ref().expect("watch").since;
        app.apply_room_refresh(snapshot(vec![other, ops], since + Duration::from_millis(1)));
        app.poll_pending_join_open(Instant::now()).await;

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!ops:example.com")
        );
        assert_eq!(app.status.text(false), "joined Ops");
        assert!(app.pending_join_open.is_none());
        assert!(app.pending_memberships.is_empty());
    }

    #[tokio::test]
    async fn a_list_from_before_the_join_does_not_open_the_room() {
        let other = room("!other:example.com", None, Some("Other"));
        let ops = room("!ops:example.com", Some("#ops:example.com"), Some("Ops"));
        let mut app = app_for(vec![rooms_response_body(std::slice::from_ref(&other))]).await;
        app.rooms.rooms = vec![other.clone()];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(ops.account_id, "#ops:example.com", &ops.room_id),
            result: Ok(()),
        })
        .await;
        app.pending_join_open.as_mut().expect("watch").since =
            Instant::now() + Duration::from_secs(3600);
        app.apply_room_refresh(snapshot(vec![other, ops], Instant::now()));
        app.poll_pending_join_open(Instant::now()).await;

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!other:example.com")
        );
        assert!(!app.pending_join_open.as_ref().expect("watch").ready);
        assert_eq!(
            app.status.text(false),
            "joined #ops:example.com; it isn't in the room list yet"
        );
    }

    #[tokio::test]
    async fn a_list_for_another_account_does_not_open_the_join() {
        let other = room("!other:example.com", None, Some("Other"));
        let ops = room("!ops:example.com", Some("#ops:example.com"), Some("Ops"));
        let mut app = app_for(vec![rooms_response_body(std::slice::from_ref(&other))]).await;
        app.rooms.rooms = vec![other.clone()];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(ops.account_id, "#ops:example.com", &ops.room_id),
            result: Ok(()),
        })
        .await;
        let since = app.pending_join_open.as_ref().expect("watch").since;
        app.apply_room_refresh(crate::api::RoomListSnapshot {
            rooms: vec![other, ops],
            started: since + Duration::from_millis(1),
            account_filter: Some(Uuid::from_u128(7)),
        });
        app.poll_pending_join_open(Instant::now()).await;

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!other:example.com")
        );
        assert!(!app.pending_join_open.as_ref().expect("watch").ready);
    }

    #[tokio::test]
    async fn the_join_watch_stops_after_its_reads_miss_the_room() {
        let other = room("!other:example.com", None, Some("Other"));
        let mut app = app_for(vec![rooms_response_body(std::slice::from_ref(&other))]).await;
        app.rooms.rooms = vec![other.clone()];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(Uuid::nil(), "#ops:example.com", "!ops:example.com"),
            result: Ok(()),
        })
        .await;

        let first = app.pending_join_open.as_ref().expect("watch").next_read_at;
        app.poll_pending_join_open(first).await;
        assert!(app.rooms_fetch_inflight);
        assert_eq!(
            app.pending_join_open
                .as_ref()
                .expect("watch")
                .reads_remaining,
            1
        );
        app.rooms_fetch_inflight = false;
        app.apply_room_refresh(snapshot(
            vec![other.clone()],
            first + Duration::from_millis(1),
        ));
        assert!(app.pending_join_open.is_some(), "one delayed read is left");

        let second = app.pending_join_open.as_ref().expect("watch").next_read_at;
        app.poll_pending_join_open(second).await;
        let last = app
            .pending_join_open
            .as_ref()
            .expect("watch")
            .last_read_started
            .expect("the last read was requested");
        assert_eq!(
            app.pending_join_open
                .as_ref()
                .expect("watch")
                .reads_remaining,
            0
        );
        app.rooms_fetch_inflight = false;
        app.apply_room_refresh(snapshot(vec![other], last + Duration::from_millis(1)));

        assert!(app.pending_join_open.is_none());
        assert_eq!(
            app.status.text(false),
            "joined #ops:example.com; it isn't in the room list yet"
        );
        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!other:example.com")
        );
    }

    #[tokio::test]
    async fn choosing_another_room_cancels_the_join_watch() {
        let other = room("!other:example.com", None, Some("Other"));
        let elsewhere = room("!elsewhere:example.com", None, Some("Elsewhere"));
        let ops = room("!ops:example.com", Some("#ops:example.com"), Some("Ops"));
        let mut app = app_for(vec![rooms_response_body(&[
            other.clone(),
            elsewhere.clone(),
        ])])
        .await;
        app.rooms.rooms = vec![other.clone(), elsewhere.clone()];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(ops.account_id, "#ops:example.com", &ops.room_id),
            result: Ok(()),
        })
        .await;
        app.rooms.selected = app
            .rooms
            .rooms
            .iter()
            .position(|room| room.room_id == "!elsewhere:example.com");
        let since = app.pending_join_open.as_ref().expect("watch").since;
        app.apply_room_refresh(snapshot(
            vec![other, elsewhere, ops],
            since + Duration::from_millis(1),
        ));
        app.poll_pending_join_open(Instant::now()).await;

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!elsewhere:example.com")
        );
        assert!(app.pending_join_open.is_none());
        assert_eq!(
            app.status.text(false),
            "joined #ops:example.com; it isn't in the room list yet"
        );
    }

    #[tokio::test]
    async fn another_room_action_cancels_the_join_watch() {
        let other = room("!other:example.com", None, Some("Other"));
        let mut app = app_for(vec![rooms_response_body(std::slice::from_ref(&other))]).await;
        app.rooms.rooms = vec![other];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(Uuid::nil(), "#ops:example.com", "!ops:example.com"),
            result: Ok(()),
        })
        .await;
        assert!(app.pending_join_open.is_some());

        app.handle_command(crate::command::parse("/invite @bob:example.com"))
            .await;

        assert!(app.pending_join_open.is_none());
        assert!(app.pending_memberships.is_empty());
    }

    #[tokio::test]
    async fn a_failed_follow_up_read_keeps_the_unanswered_status() {
        let other = room("!other:example.com", None, Some("Other"));
        let mut app = app_for(vec![rooms_response_body(std::slice::from_ref(&other))]).await;
        app.rooms.rooms = vec![other];
        app.rooms.selected = Some(0);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: join_action(Uuid::nil(), "#ops:example.com", "#ops:example.com"),
            result: Err(failure(
                "HTTP 504 Gateway Timeout: join timed out after 30s",
                true,
            )),
        })
        .await;
        {
            let watch = app.pending_join_open.as_mut().expect("watch");
            watch.reads_remaining = 0;
            watch.last_read_started = Some(Instant::now());
        }

        app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Err(crate::api::ApiError::Request(
            "nope".to_owned(),
        ))))
        .await;

        let status = app.status.text(false);
        assert_eq!(
            status,
            "the homeserver hasn't answered for #ops:example.com"
        );
        assert!(!status.contains("failed"), "{status}");
        assert!(app.pending_join_open.is_none());
        assert!(app.pending_memberships.is_empty());
    }
}
