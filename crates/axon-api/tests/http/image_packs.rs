use super::*;

async fn state(
    store: &Store,
    account: Uuid,
    room: &str,
    kind: &str,
    key: &str,
    content: Value,
    ts: i64,
) {
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id: account,
            room_id: room,
            event_type: kind,
            state_key: key,
            event_id: &format!("$pack-{ts}-{key}"),
            sender: "@pack-owner:localhost",
            origin_ts: ts,
            content: Some(content),
        })
        .await
        .unwrap();
}

async fn joined(store: &Store, account: Uuid, user: &str, room: &str) {
    state(
        store,
        account,
        room,
        "m.room.member",
        user,
        json!({"membership": "join"}),
        1,
    )
    .await;
}

async fn personal(store: &Store, account: Uuid, content: Value) {
    store
        .upsert_account_data(&AccountDataUpsert {
            account_id: account,
            room_id: None,
            event_type: "im.ponies.user_emotes",
            content,
        })
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_packs_discovery_preview_isolation_and_removal() {
    let store = store().await;
    let a = store
        .upsert_account(
            &format!("@packs-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let b = store
        .upsert_account(
            &format!("@packs-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!packs:localhost";
    joined(&store, a.account_id, &a.user_id, room).await;
    joined(&store, b.account_id, &b.user_id, room).await;
    // Production uses this personal-pack shape, including per-image usage.
    personal(&store, a.account_id, json!({
        "pack": {"display_name": "Adam", "avatar_url": "mxc://example.org/avatar"},
        "images": {
            "adam": {"body": "adam", "url": "mxc://example.org/adam", "usage": ["sticker", "emoticon"]},
            "allthethings": {"url": "mxc://example.org/allthethings"},
            "iswydt": {"url": "mxc://example.org/iswydt", "info": {"mimetype": "text/html"}},
            "sticker": {"url": "mxc://example.org/sticker", "usage": ["sticker"]}
        }
    })).await;
    personal(
        &store,
        b.account_id,
        json!({"images": {"foreign": {"url": "mxc://example.org/foreign"}}}),
    )
    .await;
    state(
        &store,
        a.account_id,
        room,
        "im.ponies.room_emotes",
        "main",
        json!({"images": {"old": {"url": "mxc://example.org/old"}}}),
        2,
    )
    .await;
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "main",
        json!({"images": {"new": {"url": "mxc://example.org/new"}}}),
        3,
    )
    .await;
    let proxy = Arc::new(ConfiguredMediaProxy::ok(b"\x89PNG\r\n\x1a\npack-image"));
    let app = media_app(store.clone(), proxy.clone());
    let uri = format!("/v1/accounts/{}/rooms/{room}/emoji-packs", a.account_id);
    let (status, body) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["partial"], false);
    let packs = body["data"]["packs"].as_array().unwrap();
    assert_eq!(packs.len(), 2);
    assert_eq!(packs[0]["room_id"], Value::Null);
    assert_eq!(packs[0]["event_id"], Value::Null);
    assert_eq!(packs[0]["display_name"], "Adam");
    assert_eq!(
        packs[0]["images"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["shortcode"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["adam", "allthethings", "iswydt"]
    );
    assert_eq!(packs[1]["event_type"], "m.room.image_pack");
    assert_eq!(packs[1]["images"][0]["shortcode"], "new");
    // None of these images has ever been sent as a reaction or attachment.
    for id in ["adam", "allthethings", "iswydt", "new", "avatar"] {
        let path = format!("/v1/media/{}/example.org/{id}", a.account_id);
        for uri in [path.clone(), format!("{path}/thumbnail?width=32&height=32")] {
            let (status, headers, bytes) = get_media_bytes(&app, &uri, None).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(headers["content-type"], "image/png");
            assert_eq!(headers["x-content-type-options"], "nosniff");
            assert_eq!(bytes, b"\x89PNG\r\n\x1a\npack-image");
        }
    }
    let calls = proxy.calls().len();
    for id in ["old", "foreign", "sticker", "unknown"] {
        let path = format!("/v1/media/{}/example.org/{id}", a.account_id);
        assert_eq!(
            get_media_bytes(&app, &path, Some("*")).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get_media_bytes(
                &app,
                &format!("{path}/thumbnail?width=32&height=32"),
                Some("*")
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(proxy.calls().len(), calls);
    // SVG/HTML from a pack cannot acquire an active inline MIME type.
    let unsafe_app = media_app(
        store.clone(),
        Arc::new(ConfiguredMediaProxy::ok(b"<svg onload='x()'/>")),
    );
    let path = format!("/v1/media/{}/example.org/iswydt", a.account_id);
    let (_, headers, _) = get_media_bytes(&unsafe_app, &path, None).await;
    assert_eq!(headers["content-type"], "application/octet-stream");

    // Clearing stable state must not resurrect the legacy tuple it replaced.
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "main",
        json!({"images": {}}),
        4,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"][1]["images"], json!([]));
    for id in ["new", "old"] {
        assert_eq!(
            get_media_bytes(
                &app,
                &format!("/v1/media/{}/example.org/{id}", a.account_id),
                Some("*")
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    // Removing account data revokes authorization even for a matching ETag.
    personal(&store, a.account_id, json!({"images": {}})).await;
    assert_eq!(
        get_media_bytes(&app, &path, Some("*")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(
            &app,
            &format!("/v1/accounts/{}/rooms/{room}/emoji-packs", Uuid::new_v4())
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(
            &app,
            &format!(
                "/v1/accounts/{}/rooms/!unknown:localhost/emoji-packs",
                a.account_id
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "leave-test",
        json!({"images": {"leave": {"url": "mxc://example.org/leave"}}}),
        5,
    )
    .await;
    let leave_path = format!("/v1/media/{}/example.org/leave", a.account_id);
    assert_eq!(
        get_media_bytes(&app, &leave_path, None).await.0,
        StatusCode::OK
    );
    state(
        &store,
        a.account_id,
        room,
        "m.room.member",
        &a.user_id,
        json!({"membership": "leave"}),
        6,
    )
    .await;
    assert_eq!(get(&app, &uri).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        get_media_bytes(&app, &leave_path, Some("*")).await.0,
        StatusCode::NOT_FOUND
    );
    store.delete_account_row(a.account_id).await.unwrap();
    store.delete_account_row(b.account_id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_packs_mixed_sources_keep_pack_identity_and_usage() {
    let store = store().await;
    let account = store
        .upsert_account(
            &format!("@pack-variations-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!pack-variations:localhost";
    joined(&store, account.account_id, &account.user_id, room).await;
    joined(
        &store,
        account.account_id,
        &account.user_id,
        "!other:localhost",
    )
    .await;
    personal(
        &store,
        account.account_id,
        json!({
            "images": {"wave": {"url": "mxc://example.org/personal-wave"}}
        }),
    )
    .await;
    // The empty state key is a normal pack identity, not a singleton restriction.
    state(
        &store,
        account.account_id,
        room,
        "im.ponies.room_emotes",
        "",
        json!({
            "pack": {"display_name": "Legacy stickers", "usage": ["sticker"]},
            "images": {
                "wave": {"url": "mxc://example.org/legacy-wave", "usage": ["emoticon"]},
                "sticker": {"url": "mxc://example.org/sticker-only"},
                "both": {"url": "mxc://example.org/both", "usage": []}
            }
        }),
        2,
    )
    .await;
    state(&store, account.account_id, room, "m.room.image_pack", "Extras / work", json!({
        "pack": {"usage": ["emoticon"]},
        "images": {
            "wave": {"url": "mxc://example.org/stable-wave", "body": "A wave", "info": {"mimetype": "image/gif", "is_animated": true}},
            "hidden": {"url": "mxc://example.org/hidden-sticker", "usage": ["sticker"]}
        }
    }), 3).await;
    state(
        &store,
        account.account_id,
        "!other:localhost",
        "m.room.image_pack",
        "",
        json!({
            "images": {"elsewhere": {"url": "mxc://example.org/elsewhere"}}
        }),
        4,
    )
    .await;

    let proxy = Arc::new(ConfiguredMediaProxy::ok(b"GIF89apack-image"));
    let app = media_app(store.clone(), proxy);
    let uri = format!(
        "/v1/accounts/{}/rooms/{room}/emoji-packs",
        account.account_id
    );
    let (status, body) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["partial"], false);
    let packs = body["data"]["packs"].as_array().unwrap();
    assert_eq!(packs.len(), 3);
    assert_eq!(packs[0]["room_id"], Value::Null);
    assert_eq!(packs[0]["display_name"], Value::Null);
    assert_eq!(packs[1]["event_type"], "im.ponies.room_emotes");
    assert_eq!(packs[1]["state_key"], "");
    assert_eq!(packs[2]["event_type"], "m.room.image_pack");
    assert_eq!(packs[2]["state_key"], "Extras / work");
    let waves: Vec<_> = packs
        .iter()
        .flat_map(|pack| pack["images"].as_array().unwrap())
        .filter(|image| image["shortcode"] == "wave")
        .map(|image| image["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        waves,
        [
            "mxc://example.org/personal-wave",
            "mxc://example.org/legacy-wave",
            "mxc://example.org/stable-wave"
        ]
    );
    assert_eq!(packs[1]["images"].as_array().unwrap().len(), 2);
    assert_eq!(packs[2]["images"].as_array().unwrap().len(), 1);
    assert_eq!(packs[2]["images"][0]["body"], "A wave");
    // Every source can preview unused images, including GIFs, with safe MIME.
    for id in ["personal-wave", "legacy-wave", "stable-wave", "both"] {
        let path = format!("/v1/media/{}/example.org/{id}", account.account_id);
        let (status, headers, _) = get_media_bytes(&app, &path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/gif");
    }
    for id in ["sticker-only", "hidden-sticker"] {
        let path = format!("/v1/media/{}/example.org/{id}", account.account_id);
        assert_eq!(
            get_media_bytes(&app, &path, Some("*")).await.0,
            StatusCode::NOT_FOUND
        );
    }
    store.delete_account_row(account.account_id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_packs_bounds_and_redaction() {
    let store = store().await;
    let a = store
        .upsert_account(
            &format!("@pack-bounds-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!bounds:localhost";
    joined(&store, a.account_id, &a.user_id, room).await;
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "oversized",
        json!({"images": {}, "padding": "x".repeat(131072)}),
        2,
    )
    .await;
    let app = read_app(store.clone());
    let uri = format!("/v1/accounts/{}/rooms/{room}/emoji-packs", a.account_id);
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], true);
    assert_eq!(body["data"]["packs"], json!([]));
    let images = (0..513)
        .map(|i| {
            (
                format!("image{i:04}"),
                json!({"url": "mxc://example.org/bounded"}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    for key in ["a", "b"] {
        state(
            &store,
            a.account_id,
            room,
            "m.room.image_pack",
            key,
            json!({"images": images}),
            3,
        )
        .await;
    }
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], true);
    let packs = body["data"]["packs"].as_array().unwrap();
    assert_eq!(
        packs
            .iter()
            .map(|p| p["images"].as_array().unwrap().len())
            .sum::<usize>(),
        1000
    );
    // Redaction evidence takes precedence even if content was retained.
    store
        .upsert_room_state_with_redaction(
            &RoomStateUpsert {
                account_id: a.account_id,
                room_id: room,
                event_type: "m.room.image_pack",
                state_key: "a",
                event_id: "$pack-3-a",
                sender: "@pack-owner:localhost",
                origin_ts: 3,
                content: Some(
                    json!({"images": {"still-present": {"url": "mxc://example.org/redacted"}}}),
                ),
            },
            None,
            RoomStateRedaction::Redacted {
                event_id: Some("$redaction"),
            },
        )
        .await
        .unwrap();
    assert!(!store
        .image_pack_authorizes_media(a.account_id, "mxc://example.org/redacted")
        .await
        .unwrap());
    for i in 0..65 {
        state(
            &store,
            a.account_id,
            room,
            "im.ponies.room_emotes",
            &format!("pack{i:02}"),
            json!({"images": {}}),
            4,
        )
        .await;
    }
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], true);
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 64);
    store.delete_account_row(a.account_id).await.unwrap();
}
