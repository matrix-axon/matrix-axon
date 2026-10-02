use super::*;
use crate::api::{AxonClient, LiveFrame, RoomTag};
use crate::app::{Mode, RoomSort, RoomTargetResolution};
use crate::config::TuiConfig;
use ratatui_image::picker::Picker;
use uuid::Uuid;

fn room(account: Uuid, id: &str, name: &str, space: bool) -> RoomDto {
    serde_json::from_value(serde_json::json!({
        "account_id": account, "room_id": id, "name": name,
        "room_type": if space { Some("m.space") } else { None },
        "last_activity_ts": 0, "last_event_id": null,
    }))
    .unwrap()
}

fn app() -> App {
    let mut app = App::new(
        AxonClient::new("http://127.0.0.1:1".to_owned(), None),
        None,
        TuiConfig::test_default(),
        Picker::halfblocks(),
    );
    app.rooms.rooms = vec![
        room(Uuid::nil(), "!work:srv", "Work", true),
        room(Uuid::nil(), "!club:srv", "Club", true),
        room(Uuid::nil(), "!a:srv", "Alpha", false),
        room(Uuid::nil(), "!b:srv", "Beta", false),
        room(Uuid::nil(), "!c:srv", "Other", false),
    ];
    seed(&mut app, "!work:srv", &["!a:srv", "!b:srv"]);
    seed(&mut app, "!club:srv", &["!b:srv", "!not-joined:srv"]);
    app
}

fn key(id: &str) -> RoomKey {
    RoomKey {
        account_id: Uuid::nil(),
        room_id: id.to_owned(),
    }
}

fn seed(app: &mut App, id: &str, children: &[&str]) {
    app.spaces.children.insert(
        key(id),
        ChildrenState {
            children: Some(children.iter().map(|s| (*s).to_owned()).collect()),
            ..Default::default()
        },
    );
}

fn leaves(app: &App) -> Vec<String> {
    app.visible_room_indices()
        .iter()
        .map(|i| app.rooms.rooms[*i].room_id.clone())
        .collect()
}

fn change(spaces: &[&str], device_id: Uuid) -> PreferencesChangedDto {
    PreferencesChangedDto {
        key: "space_order".to_owned(),
        device_id,
        value: serde_json::json!({ "spaces": spaces.iter().map(|id| key(id).preference_entry()).collect::<Vec<_>>() }),
    }
}

#[tokio::test]
async fn launch_waits_for_order_membership_and_device_state_before_opening_top_room() {
    use crate::app::bootstrap::BootstrapOutcome;
    let mut app = app();
    let mut rooms = app.rooms.rooms.clone();
    // The globally most active room is Ungrouped and must not become the
    // launch room merely because the tree's membership has not landed yet.
    rooms[4].last_activity_ts = 100;
    app.rooms.rooms.clear();
    app.spaces.children.clear();
    // A reconnect can reveal rooms before the accounts stage advances to
    // Rooms. That automatic provisional choice must not suppress launch.
    app.apply_room_refresh(rooms.clone());
    assert_eq!(app.selected_room().unwrap().room_id, "!c:srv");
    app.bootstrap = BootstrapStage::Rooms;
    app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Ok(rooms)))
        .await;
    assert!(app.selected_room().is_none());
    assert!(app.space_launch_pending());
    assert_eq!(app.rooms.scroll, 0);
    app.apply_preferences_changed(change(&["!work:srv", "!club:srv"], Uuid::new_v4()));
    seed(&mut app, "!work:srv", &["!a:srv", "!b:srv"]);
    app.finish_space_launch(Instant::now());
    assert!(app.selected_room().is_none(), "markers have not landed");
    // The selected room's draft must hydrate before compose ownership is set.
    app.handle_bootstrap_outcome(BootstrapOutcome::DeviceState {
        markers: vec![(
            Uuid::nil(),
            Ok(serde_json::from_value(serde_json::json!({
                "entries": { "!a:srv": { "value": { "event_id": "$read", "origin_ts": 123 } } }
            }))
            .unwrap()),
        )],
        drafts: vec![(
            Uuid::nil(),
            Ok(serde_json::from_value(serde_json::json!({
                "entries": { "!a:srv": { "value": { "text": "saved Alpha draft" } } }
            }))
            .unwrap()),
        )],
    })
    .await;
    assert!(app.bootstrap.is_done());
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
    assert_eq!(
        app.rooms.selected,
        app.visible_room_indices().first().copied()
    );
    assert_eq!(app.input.buffer, "saved Alpha draft");
    assert_eq!(app.read_markers[&key("!a:srv")].event_id, "$read");
    // Later metadata cannot move a settled launch selection.
    seed(&mut app, "!club:srv", &["!b:srv"]);
    app.finish_space_launch(Instant::now());
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
}

#[tokio::test]
async fn launch_does_not_choose_a_later_group_before_the_first_group_is_known() {
    let mut app = app();
    app.spaces.children.remove(&key("!club:srv"));
    app.spaces.order_ready = true;
    app.begin_space_launch();
    app.bootstrap = BootstrapStage::Spaces;
    app.finish_space_launch(Instant::now());
    assert!(app.selected_room().is_none());
    // Successful empty membership permits the next group; a failed root also
    // permits best-effort progress instead of blocking startup indefinitely.
    seed(&mut app, "!club:srv", &[]);
    app.finish_space_launch(Instant::now());
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");

    app = self::app();
    app.spaces.order_ready = true;
    app.begin_space_launch();
    app.bootstrap = BootstrapStage::Spaces;
    app.spaces.children.clear();
    app.spaces.children.insert(
        key("!club:srv"),
        ChildrenState {
            error: Some("offline".to_owned()),
            ..Default::default()
        },
    );
    assert!(
        matches!(app.launch_selection(), LaunchSelection::Waiting(Some(root)) if root == key("!work:srv"))
    );
    let deadline = app.spaces.launch_deadline.unwrap();
    app.finish_space_launch(deadline);
    assert!(app.bootstrap.is_done());
    assert!(app.selected_room().is_some());
}

#[tokio::test]
async fn explicit_startup_room_selection_wins_and_preserves_unhydrated_drafts() {
    use crate::app::bootstrap::BootstrapOutcome;
    let mut app = app();
    app.begin_space_launch();
    app.bootstrap = BootstrapStage::DeviceState;
    app.activate_sidebar_room(4);
    assert!(!app.space_launch_pending());
    assert!(app.compose_room.is_none());
    assert!(app.spaces.timeline.is_none(), "wait for read markers");
    app.handle_bootstrap_outcome(BootstrapOutcome::DeviceState {
        markers: Vec::new(),
        drafts: vec![(
            Uuid::nil(),
            Ok(serde_json::from_value(serde_json::json!({
                "entries": { "!c:srv": { "value": { "text": "saved Other draft" } } }
            }))
            .unwrap()),
        )],
    })
    .await;
    app.spaces.order_ready = true;
    app.finish_space_launch(Instant::now());
    assert_eq!(app.selected_room().unwrap().room_id, "!c:srv");
    assert_eq!(app.input.buffer, "saved Other draft");
}

#[test]
fn pinning_a_space_moves_it_to_the_shared_front_without_room_tag_writes() {
    let mut app = app();
    let hidden = RoomKey {
        account_id: Uuid::new_v4(),
        room_id: "!hidden:srv".to_owned(),
    };
    app.accounts.accounts = vec![crate::api::AccountDto {
        account_id: Uuid::nil(),
        user_id: "@me:srv".to_owned(),
        state: crate::api::AccountState::Active,
        device_id: None,
        verified: None,
        backup: Default::default(),
    }];
    app.accounts.selected = crate::app::AccountSelection::Account(0);
    app.rooms
        .rooms
        .push(room(hidden.account_id, "!hidden:srv", "Hidden", true));
    app.rooms
        .rooms
        .push(room(Uuid::nil(), "!third:srv", "Third", true));
    app.spaces.order = vec![
        key("!club:srv"),
        hidden.clone(),
        key("!work:srv"),
        key("!absent:srv"),
        key("!third:srv"),
    ];
    app.spaces.order_ready = true;
    app.spaces.focus = Some(key("!third:srv"));
    app.rooms.selected = Some(2);
    app.handle_space_list_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
    assert_eq!(
        app.spaces.order,
        [
            key("!third:srv"),
            key("!club:srv"),
            hidden,
            key("!work:srv"),
            key("!absent:srv")
        ]
    );
    assert!(app.spaces.order_dirty);
    assert!(app.rooms.rooms.iter().all(|room| room.tags.is_empty()));
    assert!(app.pinned_rooms.is_empty());
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
    let revision = app.spaces.order_revision;
    app.handle_space_list_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
    assert_eq!(
        app.spaces.order_revision, revision,
        "pinning first is idempotent"
    );
}

#[test]
fn left_collapses_and_opens_the_next_group_while_right_enters_a_header() {
    let mut app = app();
    app.mode = Mode::Compose;
    app.rooms.selected = Some(3);
    app.sync_draft_on_room_change();
    app.input.buffer = "Beta draft".to_owned();
    app.note_draft_activity();
    app.mode = Mode::RoomList;
    app.drafts.insert(key("!a:srv"), "Alpha draft".to_owned());
    app.spaces.collapsed.insert(key("!work:srv"));
    app.handle_space_list_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert!(app.spaces.collapsed.contains(&key("!club:srv")));
    assert!(!app.spaces.collapsed.contains(&key("!work:srv")));
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
    assert_eq!(app.drafts[&key("!b:srv")], "Beta draft");
    assert_eq!(app.input.buffer, "Alpha draft");
    assert!(!leaves(&app).contains(&"!b:srv".to_owned()));
    app.handle_space_list_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(app.selected_room().unwrap().room_id, "!c:srv");
    app.spaces.focus = Some(key("!club:srv"));
    app.handle_space_list_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(app.selected_room().unwrap().room_id, "!b:srv");
    assert_eq!(app.input.buffer, "Beta draft");
    assert!(!app.spaces.collapsed.contains(&key("!club:srv")));
    let selected = app.rooms.selected;
    app.handle_space_list_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(
        app.rooms.selected, selected,
        "expanded child navigation is idempotent"
    );
    assert!(
        !app.handle_space_list_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
        "Alt-Left still adjusts width"
    );
}

#[test]
fn deferred_group_navigation_is_canceled_and_empty_destinations_are_skipped() {
    let mut app = app();
    app.rooms.selected = Some(3);
    app.spaces.children.remove(&key("!work:srv"));
    app.handle_space_list_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(app.spaces.focus, Some(key("!work:srv")));
    assert!(app.spaces.navigation.is_some());
    assert_eq!(app.selected_room().unwrap().room_id, "!b:srv");
    app.handle_space_list_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    seed(&mut app, "!work:srv", &["!a:srv"]);
    app.finish_space_navigation();
    assert_eq!(app.selected_room().unwrap().room_id, "!b:srv");
    assert!(app.spaces.navigation.is_none());

    app.spaces.focus = None;
    app.spaces.children.remove(&key("!work:srv"));
    app.handle_space_list_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    seed(&mut app, "!work:srv", &[]);
    app.finish_space_navigation();
    assert_eq!(
        app.selected_room().unwrap().room_id,
        "!a:srv",
        "empty next group falls through to Ungrouped"
    );
}

#[test]
fn explicit_filter_collapse_hides_matches_and_no_following_room_keeps_header_focus() {
    let mut app = app();
    app.rooms.rooms.truncate(4);
    app.rooms.selected = Some(2);
    app.room_filter = RoomFilter::Name("alpha".to_owned());
    app.handle_space_list_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(
        app.spaces.filter_expanded.get(&key("!work:srv")),
        Some(&false)
    );
    assert!(!app.spaces.collapsed.contains(&key("!work:srv")));
    assert!(leaves(&app).is_empty());
    assert_eq!(app.spaces.focus, Some(key("!work:srv")));
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
    app.handle_space_list_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(leaves(&app), ["!a:srv"]);
    app.spaces.focus = Some(key("!work:srv"));
    app.toggle_focused_space();
    assert!(leaves(&app).is_empty());
    app.update_room_name_filter("alpha".to_owned());
    assert_eq!(
        leaves(&app),
        ["!a:srv"],
        "a new filter reveals matches again"
    );
}

#[test]
fn first_parent_is_root_ordered_and_collapsing_never_cross_lists() {
    let mut app = app();
    assert_eq!(leaves(&app), ["!b:srv", "!a:srv", "!c:srv"]);
    app.spaces.collapsed.insert(key("!club:srv"));
    assert_eq!(leaves(&app), ["!a:srv", "!c:srv"]);
    app.spaces.order = vec![key("!work:srv"), key("!club:srv")];
    assert_eq!(leaves(&app), ["!a:srv", "!b:srv", "!c:srv"]);
    assert!(app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Ungrouped { provisional: false })));
}

#[test]
fn subspaces_remain_roots_and_unknown_children_are_not_timeline_targets() {
    let mut app = app();
    seed(
        &mut app,
        "!work:srv",
        &["!club:srv", "!a:srv", "!not-joined:srv"],
    );
    assert_eq!(
        app.sidebar_rows(None)
            .iter()
            .filter(|r| matches!(r, SidebarRow::Space { .. }))
            .count(),
        2
    );
    assert!(leaves(&app)
        .iter()
        .all(|id| id != "!club:srv" && id != "!not-joined:srv"));
}

#[test]
fn grouping_is_account_scoped_even_for_identical_room_ids() {
    let mut app = app();
    let account = Uuid::new_v4();
    app.rooms
        .rooms
        .push(room(account, "!a:srv", "Other account Alpha", false));
    let rows = app.sidebar_rows(None);
    let other = rows
        .iter()
        .find(|r| matches!(r, SidebarRow::Room { index: 5, .. }))
        .unwrap();
    assert!(matches!(
        other,
        SidebarRow::Room {
            indented: false,
            ..
        }
    ));
    assert!(app
        .sidebar_rows(Some(account))
        .iter()
        .all(|r| !matches!(r, SidebarRow::Space { .. } | SidebarRow::Ungrouped { .. })));
}

#[test]
fn favorites_sort_inside_each_group_and_fav_filter_flattens() {
    let mut app = app();
    app.rooms.rooms[2].tags.push(RoomTag {
        name: "m.favourite".to_owned(),
        order: Some(0.4),
    });
    app.rooms.rooms[4].tags.push(RoomTag {
        name: "m.favourite".to_owned(),
        order: Some(0.1),
    });
    app.room_sort = RoomSort::AlphaDesc;
    app.resort_rooms();
    assert_eq!(leaves(&app), ["!b:srv", "!a:srv", "!c:srv"]);
    app.spaces.collapsed.insert(key("!work:srv"));
    app.room_filter = RoomFilter::Favorites;
    assert_eq!(leaves(&app), ["!c:srv", "!a:srv"]);
    assert!(!app.space_tree_enabled());
    assert!(app.sidebar_rows(None).iter().all(|r| matches!(
        r,
        SidebarRow::Room {
            indented: false,
            ..
        }
    )));
}

#[test]
fn name_and_unread_reveal_children_without_overwriting_collapse_choices() {
    let mut app = app();
    app.spaces.collapsed.insert(key("!work:srv"));
    app.room_filter = RoomFilter::Name("alpha".to_owned());
    assert_eq!(leaves(&app), ["!a:srv"]);
    app.room_filter = RoomFilter::Unread;
    app.rooms.unread.insert(key("!a:srv"), 1);
    assert_eq!(leaves(&app), ["!a:srv"]);
    app.room_filter = RoomFilter::All;
    assert!(!leaves(&app).contains(&"!a:srv".to_owned()));
}

#[test]
fn header_focus_and_collapse_preserve_the_open_room_and_compose_owner() {
    let mut app = app();
    app.rooms.selected = Some(2);
    app.mode = Mode::RoomList;
    let compose = app.compose_room.clone();
    app.spaces.focus = Some(key("!work:srv"));
    assert!(app.handle_space_list_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
    assert_eq!(app.compose_room, compose);
    app.apply_room_refresh(app.rooms.rooms.clone());
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
    app.sync_room_selection_to_account_filter();
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
}

#[test]
fn numeric_targets_follow_leaf_numbers_and_named_targets_reveal_collapsed_children() {
    let mut app = app();
    app.spaces.collapsed.insert(key("!work:srv"));
    assert!(matches!(
        app.resolve_room_target("1"),
        RoomTargetResolution::Match(3)
    ));
    assert!(matches!(
        app.resolve_room_target("Alpha"),
        RoomTargetResolution::Match(2)
    ));
    assert!(matches!(
        app.resolve_room_target("Work"),
        RoomTargetResolution::Missing
    ));
    app.activate_sidebar_room(2);
    assert!(leaves(&app).contains(&"!a:srv".to_owned()));
    assert!(!app.spaces.collapsed.contains(&key("!work:srv")));
}

#[test]
fn header_arrows_skip_dividers_and_never_open_space_timelines() {
    let mut app = app();
    app.rooms.selected = Some(2);
    app.spaces.focus = Some(key("!club:srv"));
    app.handle_space_list_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.selected_room().unwrap().room_id, "!b:srv");
    assert_eq!(app.spaces.focus, None);
    app.handle_space_list_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.spaces.focus, Some(key("!club:srv")));
    assert_eq!(app.selected_room().unwrap().room_id, "!b:srv");
}

#[test]
fn pending_or_failed_membership_keeps_rooms_accessible_and_labels_the_remainder() {
    let mut app = app();
    app.spaces.children.clear();
    assert_eq!(leaves(&app).len(), 3);
    assert!(app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Ungrouped { provisional: true })));
    app.spaces
        .children
        .entry(key("!work:srv"))
        .or_default()
        .error = Some("timeout".to_owned());
    assert_eq!(leaves(&app).len(), 3);
}

#[test]
fn invalidation_during_get_discards_the_response_and_requires_a_followup() {
    let mut app = app();
    app.spaces
        .children
        .get_mut(&key("!work:srv"))
        .unwrap()
        .inflight = Some(7);
    app.invalidate_space(&key("!work:srv"));
    app.apply_space_outcome(SpaceOutcome::Children {
        key: key("!work:srv"),
        request: 7,
        result: Ok(Vec::new()),
    });
    let state = &app.spaces.children[&key("!work:srv")];
    assert!(state.dirty);
    assert_eq!(state.children.as_ref().unwrap().len(), 2);
    assert!(state.inflight.is_none());
}

#[test]
fn removed_space_results_do_not_resurrect_the_cache() {
    let mut app = app();
    app.rooms.rooms.remove(0);
    app.reconcile_spaces();
    app.apply_space_outcome(SpaceOutcome::Children {
        key: key("!work:srv"),
        request: 7,
        result: Ok(Vec::new()),
    });
    assert!(!app.spaces.children.contains_key(&key("!work:srv")));
}

#[test]
fn successful_empty_children_prune_old_membership() {
    let mut app = app();
    app.spaces
        .children
        .get_mut(&key("!work:srv"))
        .unwrap()
        .inflight = Some(7);
    app.apply_space_outcome(SpaceOutcome::Children {
        key: key("!work:srv"),
        request: 7,
        result: Ok(Vec::new()),
    });
    let row = app
        .sidebar_rows(None)
        .into_iter()
        .find(|r| matches!(r, SidebarRow::Room { index: 2, .. }))
        .unwrap();
    assert!(matches!(
        row,
        SidebarRow::Room {
            indented: false,
            ..
        }
    ));
}

#[test]
fn preference_frames_accept_instance_scope_and_ignore_own_device() {
    let mut app = app();
    app.device_id = Uuid::new_v4();
    let own = app.device_id;
    app.handle_live_frame(LiveFrame::Preferences(change(&["!work:srv"], own)));
    assert!(app.spaces.order.is_empty());
    app.handle_live_frame(LiveFrame::Preferences(change(
        &["!work:srv"],
        Uuid::new_v4(),
    )));
    assert_eq!(app.spaces.order, [key("!work:srv")]);
    assert_eq!(app.ordered_space_indices(None), [0, 1]);
}

#[test]
fn move_before_hydration_does_not_overwrite_the_server() {
    let mut app = app();
    app.spaces.focus = Some(key("!club:srv"));
    app.move_focused_space(1);
    assert!(!app.spaces.order_dirty);
    assert!(app.status.text(false).contains("still loading"));
}

#[test]
fn filtered_reorder_preserves_other_accounts_and_absent_keys() {
    let mut app = app();
    let hidden = RoomKey {
        account_id: Uuid::new_v4(),
        room_id: "!other:srv".to_owned(),
    };
    app.spaces.order = vec![
        key("!club:srv"),
        hidden.clone(),
        key("!work:srv"),
        key("!absent:srv"),
    ];
    app.spaces.order_ready = true;
    app.spaces.focus = Some(key("!club:srv"));
    app.move_focused_space(1);
    assert_eq!(
        app.spaces.order,
        [
            key("!work:srv"),
            hidden,
            key("!club:srv"),
            key("!absent:srv")
        ]
    );
    assert!(app.spaces.order_dirty);
}

#[test]
fn move_during_put_stays_pending_after_the_older_put_finishes() {
    let mut app = app();
    app.rooms
        .rooms
        .push(room(Uuid::nil(), "!club-only:srv", "Club only", false));
    seed(&mut app, "!club:srv", &["!b:srv", "!club-only:srv"]);
    app.spaces.order_ready = true;
    app.spaces.focus = Some(key("!club:srv"));
    app.move_focused_space(1);
    let revision = app.spaces.order_revision;
    app.move_focused_space(-1);
    app.apply_space_outcome(SpaceOutcome::OrderWrite {
        revision,
        result: Ok(()),
    });
    assert!(app.spaces.order_dirty);
    app.apply_space_outcome(SpaceOutcome::OrderWrite {
        revision: app.spaces.order_revision,
        result: Ok(()),
    });
    assert!(!app.spaces.order_dirty);
}

#[test]
fn failed_put_retains_local_order_and_blocks_older_reads() {
    let mut app = app();
    app.spaces.order_ready = true;
    app.spaces.focus = Some(key("!club:srv"));
    app.move_focused_space(1);
    let order = app.spaces.order.clone();
    app.apply_space_outcome(SpaceOutcome::OrderWrite {
        revision: app.spaces.order_revision,
        result: Err(ApiError::Request("timeout".to_owned())),
    });
    assert_eq!(app.spaces.order, order);
    assert!(app.spaces.order_retry_at.is_some());
    app.apply_space_outcome(SpaceOutcome::OrderRead {
        revision: app.spaces.order_revision,
        result: Ok(PreferenceDto {
            value: serde_json::json!({ "spaces": [] }),
        }),
    });
    assert_eq!(app.spaces.order, order);
    assert!(app.spaces.order_dirty);
}

#[test]
fn sibling_write_during_local_save_requires_a_trailing_authoritative_read() {
    let mut app = app();
    app.spaces.order_ready = true;
    app.spaces.order_read_again = false;
    app.spaces.focus = Some(key("!club:srv"));
    app.move_focused_space(1);
    app.apply_preferences_changed(change(&["!club:srv"], Uuid::new_v4()));
    assert!(app.spaces.order_read_again);
    app.apply_space_outcome(SpaceOutcome::OrderWrite {
        revision: app.spaces.order_revision,
        result: Ok(()),
    });
    let desired = vec![key("!club:srv")];
    app.apply_space_outcome(SpaceOutcome::OrderRead { revision: app.spaces.order_revision,
            result: Ok(PreferenceDto { value: serde_json::json!({ "spaces": desired.iter().map(RoomKey::preference_entry).collect::<Vec<_>>() }) }) });
    assert_eq!(app.spaces.order, desired);
}

#[test]
fn newer_frame_or_local_move_makes_a_pending_get_stale() {
    let mut app = app();
    let revision = app.spaces.order_revision;
    app.apply_preferences_changed(change(&["!work:srv"], Uuid::new_v4()));
    app.apply_space_outcome(SpaceOutcome::OrderRead {
        revision,
        result: Ok(PreferenceDto {
            value: serde_json::json!({ "spaces": [] }),
        }),
    });
    assert_eq!(app.spaces.order, [key("!work:srv")]);
}

#[test]
fn unset_preference_is_ready_but_never_uploads_an_empty_order() {
    let mut app = app();
    app.apply_space_outcome(SpaceOutcome::OrderRead {
        revision: 0,
        result: Err(ApiError::Status {
            status: reqwest::StatusCode::NOT_FOUND,
            message: "unset".to_owned(),
        }),
    });
    assert!(app.spaces.order_ready);
    assert!(!app.spaces.order_dirty);
}

#[test]
fn preference_parser_rejects_shape_errors_and_deduplicates_keys() {
    assert!(parse_order(serde_json::json!({ "spaces": ["bad"] })).is_err());
    assert!(parse_order(serde_json::json!({ "spaces": [], "extra": 1 })).is_err());
    let entry = key("!a:srv").preference_entry();
    assert_eq!(
        parse_order(serde_json::json!({ "spaces": [entry, entry] })).unwrap(),
        [key("!a:srv")]
    );
}

#[tokio::test]
async fn viewport_fetches_are_bounded_and_successful_empty_reads_stop_repeating() {
    let mut app = app();
    app.rooms.rooms = (0..100)
        .map(|i| room(Uuid::nil(), &format!("!s{i}:srv"), &format!("{i:03}"), true))
        .collect();
    app.spaces.children.clear();
    let (tx, _rx) = mpsc::unbounded_channel();
    app.spaces.tx = Some(tx);
    app.rooms.page_size = 3;
    app.sweep_spaces(Instant::now());
    assert_eq!(
        app.spaces
            .children
            .values()
            .filter(|s| s.inflight.is_some())
            .count(),
        WORKERS
    );
    assert_eq!(app.spaces.workers.available_permits(), 0);
    assert!(app.spaces.children.len() <= WORKERS + 1);
    for state in app.spaces.children.values_mut() {
        if let Some(abort) = state.abort.take() {
            abort.abort();
        }
    }
    app.spaces.children.clear();
    let root = RoomKey::from(&app.rooms.rooms[0]);
    app.spaces.children.insert(
        root.clone(),
        ChildrenState {
            children: Some(Vec::new()),
            ..Default::default()
        },
    );
    app.sweep_spaces(Instant::now());
    assert!(app.spaces.children[&root].inflight.is_none());
}

fn render(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::{backend::TestBackend, Terminal};
    app.bootstrap = crate::app::bootstrap::BootstrapStage::Done;
    app.show_input_help = false;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::prepare(app, frame.area());
            crate::ui::draw(frame, app);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

fn room_text(app: &App, buffer: &ratatui::buffer::Buffer) -> String {
    let area = app.frame.areas.rooms.unwrap();
    let mut text = String::new();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
    }
    text
}

#[test]
fn rendered_tree_has_headers_indentation_leaf_numbers_and_collapse_markers() {
    let mut app = app();
    app.mode = Mode::RoomList;
    app.rooms.selected = Some(2);
    let buffer = render(&mut app, 140, 30);
    let text = room_text(&app, &buffer);
    assert!(text.contains("[-] Club"));
    assert!(text.contains("[-] Work"));
    assert!(text.contains("  >2 Alpha"));
    assert!(text.contains("Ungrouped"));
    assert!(text.contains("3 Other"));
    app.spaces.focus = Some(key("!work:srv"));
    app.toggle_focused_space();
    let buffer = render(&mut app, 140, 30);
    let text = room_text(&app, &buffer);
    assert!(text.contains(">[+] Work"));
    assert!(!text.contains("Alpha"));
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
}

#[tokio::test]
async fn scrolled_children_keep_their_space_name_visible() {
    let mut app = app();
    // Reproduce an inherited scroll offset that starts at the first child,
    // even though there is enough room to show both spaces.
    app.rooms.selected = Some(3);
    app.rooms.scroll = 1;
    let buffer = render(&mut app, 140, 30);
    let text = room_text(&app, &buffer);
    assert!(text.contains("[-] Club"));
    assert!(text.contains("[-] Work"));

    // A large group must retain its label even when the actual header is
    // well above the viewport, without scrolling the selected room away.
    app.rooms.rooms.extend((0..30).map(|i| {
        room(
            Uuid::nil(),
            &format!("!child{i}:srv"),
            &format!("Child{i:02}"),
            false,
        )
    }));
    let children: Vec<_> = (0..30).map(|i| format!("!child{i}:srv")).collect();
    seed(
        &mut app,
        "!work:srv",
        &children.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    app.rooms.selected = Some(app.rooms.rooms.len() - 1);
    app.rooms.scroll = 0;
    for width in [80, 140] {
        let buffer = render(&mut app, width, 12);
        let text = room_text(&app, &buffer);
        assert!(text.contains("[-] Work"), "width={width}");
        assert!(text.contains("Child29"), "width={width}");
        assert_eq!(text.matches("[-] Work").count(), 1);
    }
    assert!(app.rooms.scroll > LOOKAHEAD);
    let (tx, _rx) = mpsc::unbounded_channel();
    app.spaces.tx = Some(tx);
    app.invalidate_space(&key("!work:srv"));
    app.sweep_spaces(Instant::now());
    assert!(app.spaces.children[&key("!work:srv")].inflight.is_some());

    app.spaces.focus = Some(key("!work:srv"));
    let buffer = render(&mut app, 140, 12);
    let text = room_text(&app, &buffer);
    assert_eq!(text.matches("[-] Work").count(), 1);
    assert!(app.frame.sidebar_header.is_none());
    app.set_room_filter(RoomFilter::Favorites);
    render(&mut app, 140, 12);
    assert!(app.frame.sidebar_header.is_none());
}

#[test]
fn tree_geometry_handles_empty_filters_tiny_terminals_and_narrow_room_rows() {
    let mut app = app();
    app.rooms.selected = Some(4);
    for (width, height) in [(1, 1), (2, 2), (40, 6), (80, 9), (240, 60)] {
        render(&mut app, width, height);
        assert!(app.rooms.scroll <= app.frame.sidebar.len());
        assert!(app.frame.sidebar_end <= app.frame.sidebar.len());
        if let Some(selected) = app.rooms.selected {
            // Where a room's full height consumes the available pane, a
            // contextual heading must not displace it.
            if let Some(area) = app.frame.areas.rooms {
                let row_height = if app.frame.areas.rooms_wide { 1 } else { 2 };
                if usize::from(area.height.saturating_sub(2)) <= row_height {
                    assert!(app.frame.sidebar_header.is_none());
                    assert!(app.frame.sidebar[app.rooms.scroll..app.frame.sidebar_end]
                        .iter()
                        .any(|row| row.index() == Some(selected)));
                }
            }
        }
    }
    app.rooms.selected = None;
    app.room_filter = RoomFilter::Dms;
    render(&mut app, 140, 30);
    assert!(app.frame.sidebar.is_empty());
}

#[test]
fn page_navigation_counts_terminal_lines_including_headers_and_dividers() {
    let mut app = app();
    app.rooms.selected = Some(2);
    render(&mut app, 80, 9);
    app.spaces.focus = Some(key("!club:srv"));
    // Five room-pane lines: Club(1), Beta(2), Work(1), Alpha starts at 4.
    app.rooms.page_size = 4;
    app.handle_space_list_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
}

fn live_message(id: &str, body: &str) -> EventDto {
    serde_json::from_value(serde_json::json!({
        "account_id": Uuid::nil(), "room_id": "!a:srv", "event_id": id,
        "sender": "@alice:srv", "type": "m.room.message", "body": body,
        "origin_ts": 2, "arrival_order": 2, "redacted": false,
        "content": { "msgtype": "m.text", "body": body }
    }))
    .unwrap()
}

fn pending_timeline(app: &mut App, request: u64) {
    let task = tokio::spawn(std::future::pending::<()>());
    app.spaces.timeline = Some(PendingTimeline {
        request,
        key: key("!a:srv"),
        abort: task.abort_handle(),
        live: Vec::new(),
        overflow: false,
    });
    app.rooms.selected = Some(2);
}

fn timeline_result(app: &App, request: u64, events: Vec<EventDto>) -> SpaceOutcome {
    SpaceOutcome::Timeline(Box::new(SidebarTimelineOutcome {
        room: app.rooms.rooms[2].clone(),
        request,
        page: Ok(TimelinePage {
            events,
            next_cursor: None,
        }),
        members: Ok(Vec::new()),
    }))
}

#[tokio::test]
async fn background_snapshot_replays_new_messages_and_edits_observed_in_flight() {
    let mut app = app();
    let old = live_message("$old", "original");
    pending_timeline(&mut app, 7);
    app.messages.events.insert(key("!a:srv"), vec![old.clone()]);
    app.handle_live_frame(LiveFrame::Timeline(Box::new(live_message("$new", "live"))));
    let mut edit = live_message("$edit", "* updated");
    edit.relates_to = Some(serde_json::json!({ "rel_type": "m.replace", "event_id": "$old" }));
    edit.content =
        Some(serde_json::json!({ "m.new_content": { "msgtype": "m.text", "body": "updated" } }));
    app.handle_live_frame(LiveFrame::Timeline(Box::new(edit)));
    let outcome = timeline_result(&app, 7, vec![old]);
    app.apply_space_outcome(outcome);
    let events = &app.messages.events[&key("!a:srv")];
    assert!(events.iter().any(|e| e.event_id == "$new"));
    assert_eq!(
        events
            .iter()
            .find(|e| e.event_id == "$old")
            .unwrap()
            .body
            .as_deref(),
        Some("updated")
    );
}

#[tokio::test]
async fn stale_background_snapshot_cannot_replace_a_newer_request_or_a_historical_jump() {
    let mut app = app();
    pending_timeline(&mut app, 8);
    app.messages
        .events
        .insert(key("!a:srv"), vec![live_message("$new", "new")]);
    let outcome = timeline_result(&app, 7, Vec::new());
    app.apply_space_outcome(outcome);
    assert_eq!(app.messages.events[&key("!a:srv")].len(), 1);
    assert_eq!(app.spaces.timeline.as_ref().unwrap().request, 8);
    app.last_jump_ts = Some(123);
    let outcome = timeline_result(&app, 8, Vec::new());
    app.apply_space_outcome(outcome);
    assert_eq!(app.messages.events[&key("!a:srv")].len(), 1);
}

#[tokio::test]
async fn live_replay_buffer_is_bounded_and_overflow_never_installs_an_old_snapshot() {
    let mut app = app();
    pending_timeline(&mut app, 7);
    let event = live_message("$new", "live");
    for _ in 0..1001 {
        app.queue_sidebar_live_event(&event);
    }
    assert_eq!(app.spaces.timeline.as_ref().unwrap().live.len(), 1000);
    assert!(app.spaces.timeline.as_ref().unwrap().overflow);
    app.messages.events.insert(key("!a:srv"), vec![event]);
    let outcome = timeline_result(&app, 7, Vec::new());
    app.apply_space_outcome(outcome);
    assert_eq!(app.messages.events[&key("!a:srv")].len(), 1);
}

#[test]
fn removed_relationships_do_not_own_rooms_and_confirmed_empty_roots_are_hidden() {
    let mut app = app();
    app.spaces
        .children
        .get_mut(&key("!work:srv"))
        .unwrap()
        .inflight = Some(7);
    let removed =
        serde_json::from_value(serde_json::json!({"room_id": "!a:srv", "via": []})).unwrap();
    let missing_via = serde_json::from_value(serde_json::json!({"room_id": "!b:srv"})).unwrap();
    app.apply_space_outcome(SpaceOutcome::Children {
        key: key("!work:srv"),
        request: 7,
        result: Ok(vec![removed, missing_via]),
    });
    assert!(!app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Space { index: 0, .. })));
    assert!(app.sidebar_rows(None).iter().any(|r| matches!(
        r,
        SidebarRow::Room {
            index: 2,
            indented: false,
            ..
        }
    )));
    // Only joined, navigable leaves count, not unjoined children or subspaces.
    seed(&mut app, "!work:srv", &["!absent:srv", "!club:srv"]);
    assert!(!app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Space { index: 0, .. })));
    app.spaces
        .children
        .get_mut(&key("!work:srv"))
        .unwrap()
        .error = Some("offline".to_owned());
    assert!(app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Space { index: 0, .. })));
    app.spaces.children.remove(&key("!work:srv"));
    assert!(app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Space { index: 0, .. })));
}

#[test]
fn filtering_and_collapse_do_not_turn_nonempty_membership_into_empty_membership() {
    let mut app = app();
    app.spaces.collapsed.insert(key("!work:srv"));
    assert!(app.sidebar_rows(None).iter().any(|r| matches!(
        r,
        SidebarRow::Space {
            index: 0,
            expanded: false
        }
    )));
    app.set_room_filter(RoomFilter::Name("work".to_owned()));
    assert!(app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Space { index: 0, .. })));
}

#[tokio::test]
async fn hidden_room_navigation_starts_at_its_group_and_wraps_at_the_ends() {
    let mut app = app();
    app.spaces.collapsed.insert(key("!work:srv"));
    app.rooms.selected = Some(2);
    app.switch_relative_room(1).await;
    assert_eq!(app.selected_room().unwrap().room_id, "!c:srv");
    app.rooms.selected = Some(2);
    app.switch_relative_room(-1).await;
    assert_eq!(app.selected_room().unwrap().room_id, "!b:srv");
    app.spaces.collapsed.insert(key("!club:srv"));
    app.spaces.collapsed.remove(&key("!work:srv"));
    app.rooms.selected = Some(3);
    app.switch_relative_room(1).await;
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
    app.rooms.selected = Some(3);
    app.switch_relative_room(-1).await;
    assert_eq!(app.selected_room().unwrap().room_id, "!c:srv");
}

#[test]
fn preparing_a_hidden_active_room_does_not_grant_header_command_focus() {
    let mut app = app();
    app.mode = Mode::RoomList;
    app.rooms.selected = Some(2);
    app.spaces.collapsed.insert(key("!work:srv"));
    render(&mut app, 120, 20);
    assert!(app.spaces.focus.is_none());
    assert!(!app.handle_space_list_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));
    assert!(!app.handle_space_list_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    app.handle_space_list_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.selected_room().unwrap().room_id, "!b:srv");
}

#[test]
fn filter_choices_preserve_both_previously_open_and_previously_closed_groups() {
    let mut app = app();
    app.spaces.collapsed.insert(key("!work:srv"));
    app.set_room_filter(RoomFilter::Name("alpha".to_owned()));
    app.spaces.focus = Some(key("!work:srv"));
    app.set_space_expanded(&key("!work:srv"), true);
    assert!(app.spaces.collapsed.contains(&key("!work:srv")));
    app.set_room_filter(RoomFilter::All);
    assert!(!app.space_is_expanded(&key("!work:srv")));
    app.set_room_filter(RoomFilter::Unread);
    app.set_space_expanded(&key("!club:srv"), false);
    assert!(!app.spaces.collapsed.contains(&key("!club:srv")));
    app.set_room_filter(RoomFilter::All);
    assert!(app.space_is_expanded(&key("!club:srv")));
}

#[tokio::test]
async fn replayed_membership_departures_schedule_coalesced_room_refreshes() {
    let mut app = app();
    app.rooms.rooms[2].account_user_id = Some("@alice:srv".to_owned());
    pending_timeline(&mut app, 7);
    let mut departure = live_message("$leave", "");
    departure.event_type = "m.room.member".to_owned();
    departure.state_key = Some("@alice:srv".to_owned());
    departure.content = Some(serde_json::json!({"membership": "leave"}));
    app.queue_sidebar_live_event(&departure);
    app.queue_sidebar_live_event(&departure);
    let outcome = timeline_result(&app, 7, Vec::new());
    app.apply_space_outcome(outcome);
    assert!(app.rooms_fetch_inflight);
    assert!(app.rooms_fetch_again);
}

#[tokio::test]
async fn offscreen_invalidated_membership_settles_and_empty_roots_can_reappear() {
    let mut app = app();
    app.rooms.rooms.extend((0..30).map(|i| {
        room(
            Uuid::nil(),
            &format!("!s{i}:srv"),
            &format!("A{i:02}"),
            true,
        )
    }));
    for i in 0..30 {
        seed(&mut app, &format!("!s{i}:srv"), &[]);
    }
    // A large earlier group keeps Work beyond the viewport/lookahead.
    let mut club_children = vec!["!b:srv".to_owned()];
    for i in 0..30 {
        let id = format!("!club-child{i}:srv");
        app.rooms
            .rooms
            .push(room(Uuid::nil(), &id, "Club child", false));
        club_children.push(id);
    }
    app.spaces
        .children
        .get_mut(&key("!club:srv"))
        .unwrap()
        .children = Some(club_children);
    seed(&mut app, "!work:srv", &[]);
    assert!(!app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Space { index: 0, .. })));
    app.invalidate_space(&key("!work:srv"));
    app.rooms.page_size = 1;
    let work_position = app
        .sidebar_rows(None)
        .iter()
        .position(|row| matches!(row, SidebarRow::Space { index: 0, .. }))
        .unwrap();
    assert!(work_position > app.rooms.page_size + LOOKAHEAD);
    let (tx, _rx) = mpsc::unbounded_channel();
    app.spaces.tx = Some(tx);
    app.spaces.order_ready = true;
    app.spaces.order_read_again = false;
    app.sweep_spaces(Instant::now());
    let request = app.spaces.children[&key("!work:srv")].inflight.unwrap();
    assert!(app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Ungrouped { provisional: true })));
    let child =
        serde_json::from_value(serde_json::json!({"room_id": "!a:srv", "via": ["srv"]})).unwrap();
    app.apply_space_outcome(SpaceOutcome::Children {
        key: key("!work:srv"),
        request,
        result: Ok(vec![child]),
    });
    assert!(app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Space { index: 0, .. })));
    assert!(!app
        .sidebar_rows(None)
        .iter()
        .any(|r| matches!(r, SidebarRow::Ungrouped { provisional: true })));
    // Invalidation of an offscreen populated root also gets pool capacity.
    app.invalidate_space(&key("!work:srv"));
    app.sweep_spaces(Instant::now());
    assert!(app.spaces.children[&key("!work:srv")].inflight.is_some());
}

#[tokio::test]
async fn removing_a_pending_destination_cancels_navigation_and_cannot_recreate_cache() {
    let mut app = app();
    app.spaces.focus = Some(key("!work:srv"));
    app.spaces.navigation = Some(SpaceNavigation {
        key: key("!work:srv"),
        advance_if_empty: true,
    });
    app.rooms.rooms.remove(0);
    app.reconcile_spaces();
    let (tx, _rx) = mpsc::unbounded_channel();
    app.spaces.tx = Some(tx);
    app.sweep_spaces(Instant::now());
    assert!(app.spaces.navigation.is_none());
    assert!(!app.spaces.children.contains_key(&key("!work:srv")));
    // Even a stale intent supplied after reconciliation cannot recreate it.
    app.spaces.focus = Some(key("!work:srv"));
    app.spaces.navigation = Some(SpaceNavigation {
        key: key("!work:srv"),
        advance_if_empty: false,
    });
    app.sweep_spaces(Instant::now());
    assert!(app.spaces.navigation.is_none());
    assert!(!app.spaces.children.contains_key(&key("!work:srv")));
}

// Measure the draw-adjacent flat projection, where ADR 0093 permits a linear
// index scan but not per-room key/String allocations. Counting is thread-local
// and enabled only around the measured call, so parallel tests do not interfere.
mod allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! { static COUNT: Cell<Option<usize>> = const { Cell::new(None) }; }
    struct CountingSystem;
    #[global_allocator]
    static ALLOCATOR: CountingSystem = CountingSystem;

    fn record() {
        let _ = COUNT.try_with(|count| {
            if let Some(value) = count.get() {
                count.set(Some(value + 1));
            }
        });
    }

    // SAFETY: Every operation delegates unchanged to System; counters neither
    // allocate nor inspect or alter the allocated memory.
    unsafe impl GlobalAlloc for CountingSystem {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record();
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            record();
            unsafe { System.realloc(ptr, layout, size) }
        }
    }

    pub(super) fn measure<T>(call: impl FnOnce() -> T) -> (T, usize) {
        COUNT.with(|count| count.set(Some(0)));
        let result = call();
        let count = COUNT.with(|count| count.replace(None).unwrap());
        (result, count)
    }
}

#[test]
fn flat_projection_does_not_allocate_keys_or_rows_for_each_room() {
    let mut app = app();
    app.rooms.rooms = (0..2000)
        .map(|i| room(Uuid::nil(), &format!("!r{i}:srv"), "Room", false))
        .collect();
    let (visible, allocations) = allocations::measure(|| app.visible_room_indices());
    assert_eq!(visible, (0..2000).collect::<Vec<_>>());
    assert!(
        allocations <= 16,
        "2000-room flat projection made {allocations} allocations"
    );
}

#[test]
fn moves_skip_hidden_empty_roots_without_discarding_their_saved_rank() {
    let mut app = app();
    app.rooms
        .rooms
        .push(room(Uuid::nil(), "!empty:srv", "Empty", true));
    seed(&mut app, "!empty:srv", &[]);
    app.spaces.order = vec![key("!club:srv"), key("!empty:srv"), key("!work:srv")];
    app.spaces.order_ready = true;
    app.spaces.focus = Some(key("!club:srv"));
    app.move_focused_space(1);
    assert_eq!(
        app.spaces.order,
        [key("!work:srv"), key("!empty:srv"), key("!club:srv")]
    );
}

#[test]
fn refreshing_an_empty_root_explains_its_visibility_until_membership_settles() {
    let mut app = app();
    seed(&mut app, "!work:srv", &[]);
    app.invalidate_space(&key("!work:srv"));
    let buffer = render(&mut app, 120, 20);
    assert!(room_text(&app, &buffer).contains("Work (loading…)"));
    let state = app.spaces.children.get_mut(&key("!work:srv")).unwrap();
    state.dirty = false;
    state.inflight = Some(7);
    app.apply_space_outcome(SpaceOutcome::Children {
        key: key("!work:srv"),
        request: 7,
        result: Ok(Vec::new()),
    });
    let buffer = render(&mut app, 120, 20);
    assert!(!room_text(&app, &buffer).contains("Work"));
}

fn select_other_account(app: &mut App) {
    let other = Uuid::new_v4();
    app.accounts.accounts = [other, Uuid::nil()]
        .into_iter()
        .map(|account_id| crate::api::AccountDto {
            account_id,
            user_id: "@me:srv".to_owned(),
            state: crate::api::AccountState::Active,
            device_id: None,
            verified: None,
            backup: Default::default(),
        })
        .collect();
    app.accounts.selected = crate::app::AccountSelection::Account(0);
}

#[tokio::test]
async fn historical_search_jump_reveals_parent_and_cancels_sidebar_read() {
    use crate::app::search_flow::{SearchJumpAction, SearchOutcome};
    let mut app = app();
    app.bootstrap = BootstrapStage::Done;
    let (tx, _rx) = mpsc::unbounded_channel();
    app.spaces.tx = Some(tx);
    app.activate_sidebar_room(2);
    assert!(app.spaces.timeline.is_some());
    app.spaces.collapsed.insert(key("!work:srv"));
    app.spaces.focus = Some(key("!club:srv"));
    select_other_account(&mut app);
    let hit: EventDto = serde_json::from_value(serde_json::json!({
        "account_id": Uuid::nil(), "room_id": "!a:srv", "event_id": "$hit:srv",
        "sender": "@me:srv", "type": "m.room.message", "origin_ts": 1,
        "content": {"msgtype": "m.text", "body": "hit"}, "body": "hit",
        "redacted": false, "arrival_order": 1
    }))
    .unwrap();
    app.handle_search_outcome(SearchOutcome::Jump {
        hit: hit.clone(),
        action: SearchJumpAction::View,
        room_refresh: None,
        result: Ok(TimelinePage {
            events: vec![hit],
            next_cursor: None,
        }),
        thread_load: None,
    });
    assert_eq!(app.selected_room().unwrap().room_id, "!a:srv");
    assert_eq!(app.messages.selection.as_deref(), Some("$hit:srv"));
    assert!(app.spaces.focus.is_none());
    assert_eq!(app.active_account_filter(), Some(Uuid::nil()));
    assert!(!app.spaces.collapsed.contains(&key("!work:srv")));
    assert!(app.spaces.timeline.is_none());
}

#[tokio::test]
async fn unread_thread_jump_reveals_already_selected_collapsed_room() {
    use crate::app::UnreadThread;
    let mut app = app();
    app.rooms.selected = Some(2);
    app.spaces.collapsed.insert(key("!work:srv"));
    app.spaces.focus = Some(key("!club:srv"));
    select_other_account(&mut app);
    app.unread_threads.insert(
        key("!a:srv"),
        HashMap::from([(
            "$root:srv".to_owned(),
            UnreadThread {
                root_event_id: "$root:srv".to_owned(),
                unread_count: 1,
                latest_event_id: "$reply:srv".to_owned(),
                latest_ts: 2,
                latest_sender: "@me:srv".to_owned(),
                latest_body: "reply".to_owned(),
                counted: HashSet::from(["$reply:srv".to_owned()]),
                recent: vec![],
            },
        )]),
    );
    app.open_selected_unread_thread().await;
    assert_eq!(app.rooms.selected, Some(2));
    assert!(app.spaces.focus.is_none());
    assert_eq!(app.active_account_filter(), Some(Uuid::nil()));
    assert!(!app.spaces.collapsed.contains(&key("!work:srv")));
}

#[test]
fn flapping_reconnects_coalesce_refresh_and_preserve_failed_read_backoff() {
    let mut app = app();
    let now = Instant::now();
    let retry = now + Duration::from_secs(30);
    app.spaces
        .children
        .get_mut(&key("!work:srv"))
        .unwrap()
        .retry_at = Some(retry);
    app.spaces.order_retry_at = Some(retry);
    app.spaces.order_read_again = false;
    app.request_space_reconnect_refresh(now);
    app.request_space_reconnect_refresh(now + Duration::from_millis(500));
    app.sweep_space_reconnect_refresh(now + Duration::from_secs(1));
    assert!(!app.rooms_fetch_inflight);
    assert!(!app.spaces.children[&key("!work:srv")].dirty);
    app.sweep_space_reconnect_refresh(now + Duration::from_millis(1500));
    assert!(app.rooms_fetch_inflight);
    assert!(app.spaces.children[&key("!work:srv")].dirty);
    assert_eq!(app.spaces.children[&key("!work:srv")].retry_at, Some(retry));
    assert_eq!(app.spaces.order_retry_at, Some(retry));
    assert!(app.spaces.order_read_again);
    app.sweep_space_reconnect_refresh(now + Duration::from_secs(2));
    assert!(!app.rooms_fetch_again);
}
