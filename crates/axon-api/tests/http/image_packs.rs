use super::*;

async fn inline_event(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    event_id: &str,
    kind: &str,
    content: Option<Value>,
) {
    let redacts = content.as_ref().and_then(|c| c["redacts"].as_str());
    store
        .upsert_event(&NewEvent {
            event_id,
            room_id,
            account_id,
            sender: "@inline:localhost",
            origin_ts: 1,
            event_type: kind,
            content: content.clone(),
            raw_event: json!({"type":kind}),
            megolm_session_id: None,
            redacts,
            relates_to: content
                .as_ref()
                .and_then(|c| c.get("m.relates_to").cloned()),
            decrypted_body_text: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn inline_images_survive_pack_removal_and_revoke_before_conditional_responses() {
    let store = store().await;
    let a = store
        .upsert_account(
            &format!("@inline-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let b = store
        .upsert_account(
            &format!("@inline-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = format!("!inline-{}:localhost", Uuid::new_v4());
    joined(&store, a.account_id, &a.user_id, &room).await;
    let mxc = "mxc://example.org/inline-message";
    let content = json!({"msgtype":"m.text", "body":"hi :wave:", "format":"org.matrix.custom.html", "formatted_body":format!("hi <img data-mx-emoticon src='{mxc}' alt=':wave:' height='24'>")});
    personal(&store, a.account_id, json!({"images":{"wave":{"url":mxc}}})).await;
    inline_event(
        &store,
        a.account_id,
        &room,
        "$inline-message",
        "m.room.message",
        Some(content.clone()),
    )
    .await;
    personal(&store, a.account_id, json!({"images":{}})).await;
    let proxy = Arc::new(ConfiguredMediaProxy::ok(b"GIF89ainline"));
    let app = media_app(store.clone(), proxy.clone());
    let path = format!("/v1/media/{}/example.org/inline-message", a.account_id);
    let thumb = format!("{path}/thumbnail?width=32&height=32");
    for uri in [&path, &thumb] {
        let (status, headers, _) = get_media_bytes(&app, uri, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/gif");
    }
    assert_eq!(
        get_media_bytes(
            &app,
            &format!("/v1/media/{}/example.org/inline-message", b.account_id),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    state(
        &store,
        a.account_id,
        &room,
        "m.room.member",
        &a.user_id,
        json!({"membership":"leave"}),
        2,
    )
    .await;
    assert_eq!(
        get_media_bytes(&app, &path, Some("*")).await.0,
        StatusCode::NOT_FOUND
    );
    state(
        &store,
        a.account_id,
        &room,
        "m.room.member",
        &a.user_id,
        json!({"membership":"join"}),
        3,
    )
    .await;
    inline_event(
        &store,
        a.account_id,
        &room,
        "$inline-redact",
        "m.room.redaction",
        Some(json!({"redacts":"$inline-message"})),
    )
    .await;
    for uri in [&path, &thumb] {
        assert_eq!(
            get_media_bytes(&app, uri, Some("*")).await.0,
            StatusCode::NOT_FOUND
        );
    }

    // Re-decryption populates the projection atomically, with no pack present.
    inline_event(
        &store,
        a.account_id,
        &room,
        "$inline-utd",
        "m.room.encrypted",
        None,
    )
    .await;
    store
        .update_decrypted_event(
            a.account_id,
            "$inline-utd",
            &content,
            "m.room.message",
            Some("hi :wave:"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(get_media_bytes(&app, &path, None).await.0, StatusCode::OK);
    let unsafe_app = media_app(
        store.clone(),
        Arc::new(ConfiguredMediaProxy::ok(b"<svg onload='x'>")),
    );
    assert_eq!(
        get_media_bytes(&unsafe_app, &path, None).await.1["content-type"],
        "application/octet-stream"
    );
    inline_event(
        &store,
        a.account_id,
        &room,
        "$inline-edit-target",
        "m.room.message",
        Some(json!({"msgtype":"m.text", "body":"old"})),
    )
    .await;
    inline_event(&store, a.account_id, &room, "$inline-edit", "m.room.message", Some(json!({
        "m.relates_to":{"rel_type":"m.replace", "event_id":"$inline-edit-target"},
        "m.new_content":{"msgtype":"m.text", "body":":edit:", "format":"org.matrix.custom.html", "formatted_body":"<img src='mxc://example.org/inline-edit'>"}
    }))).await;
    let edit_path = format!("/v1/media/{}/example.org/inline-edit", a.account_id);
    assert_eq!(
        get_media_bytes(&app, &edit_path, None).await.0,
        StatusCode::OK
    );
    inline_event(
        &store,
        a.account_id,
        &room,
        "$inline-edit-target-redaction",
        "m.room.redaction",
        Some(json!({"redacts":"$inline-edit-target"})),
    )
    .await;
    assert_eq!(
        get_media_bytes(&app, &edit_path, Some("*")).await.0,
        StatusCode::NOT_FOUND
    );
    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = ANY($1)")
        .bind(vec![a.account_id, b.account_id])
        .execute(store.pool())
        .await
        .unwrap();
}

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
            "iswydt": {"url": "mxc://example.org/iswydt", "info": {"mimetype": "image/png"}},
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
    for id in ["adam", "allthethings", "iswydt", "new", "avatar", "sticker"] {
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
    for id in ["old", "foreign", "unknown"] {
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
        json!({}),
        4,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["partial"], false);
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
    let (status, stickers) = get(&app, &format!("{uri}?usage=sticker")).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = stickers["data"]["packs"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["images"].as_array().unwrap())
        .map(|i| i["shortcode"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["wave", "both", "sticker", "hidden"]);
    assert!(stickers["data"]["packs"][1]["images"][1]["sticker"]
        .as_bool()
        .unwrap());
    assert_eq!(
        get(&app, &format!("{uri}?usage=invalid")).await.0,
        StatusCode::BAD_REQUEST
    );
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
            StatusCode::NOT_MODIFIED
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
    for key in ["a", "b", "c"] {
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
    assert_eq!(packs.len(), 2);
    assert_eq!(packs[1]["partial"], true);
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
            json!({"images": {"wave": {"url":"mxc://example.org/wave"}}}),
            4,
        )
        .await;
    }
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], true);
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 64);
    store.delete_account_row(a.account_id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_packs_budget_counts_only_contributing_sources() {
    let store = store().await;
    let a = store
        .upsert_account(
            &format!("@pack-budget-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!budget:localhost";
    let ancestor = "!budget-space:localhost";
    joined(&store, a.account_id, &a.user_id, room).await;
    joined(&store, a.account_id, &a.user_id, ancestor).await;
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.create",
        "",
        json!({"type":"m.space"}),
        1,
    )
    .await;
    parent(&store, a.account_id, room, ancestor, 2).await;
    let app = read_app(store.clone());
    let uri = format!("/v1/accounts/{}/rooms/{room}/emoji-packs", a.account_id);
    let images = (0..500)
        .map(|i| {
            (
                format!("image{i:04}"),
                json!({"url":"mxc://example.org/wave"}),
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
            json!({"images":images}),
            3,
        )
        .await;
    }
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "c-clear",
        json!({}),
        3,
    )
    .await;
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "d-sticker",
        json!({"images":{"sticker":{"url":"mxc://example.org/sticker","usage":["sticker"]}}}),
        3,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], false, "{body}");
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 2);
    // One image beyond the budget is real truncation, even in an ancestor.
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.image_pack",
        "ancestor",
        json!({"images":{"wave":{"url":"mxc://example.org/ancestor"}}}),
        4,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], true);
    // Invalid and empty rows do not spend the 64 returned-room-pack slots.
    for key in ["a", "b"] {
        state(
            &store,
            a.account_id,
            room,
            "m.room.image_pack",
            key,
            json!({}),
            5,
        )
        .await;
    }
    for i in 0..60 {
        state(
            &store,
            a.account_id,
            room,
            "m.room.image_pack",
            &format!("clear{i:02}"),
            json!({}),
            5,
        )
        .await;
    }
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "e-invalid",
        json!({"images":false}),
        5,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], true);
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["data"]["packs"][0]["room_id"], ancestor);
    // Malformed metadata keeps the usable image and marks its pack partial.
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.image_pack",
        "ancestor",
        json!({"images":{"wave":{"url":"mxc://example.org/ancestor","info":{"w":4294967296_u64}}}}),
        6,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"][0]["partial"], true);
    assert!(body["data"]["packs"][0]["images"][0]["info"].is_null());
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "e-invalid",
        json!({}),
        7,
    )
    .await;
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.image_pack",
        "ancestor",
        json!({"images":{"wave":{"url":"mxc://example.org/ancestor"}}}),
        7,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], false, "{body}");
    // The independent candidate-scan bound remains explicit rather than
    // silently claiming completeness for sources outside the snapshot slice.
    state(
        &store,
        a.account_id,
        room,
        "m.room.image_pack",
        "f-clear",
        json!({}),
        6,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], true);
    store.delete_account_row(a.account_id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_preview_continues_past_invalid_candidate_pages() {
    let store = store().await;
    let account = store
        .upsert_account(
            &format!("@pack-pages-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!pack-pages:localhost";
    joined(&store, account.account_id, &account.user_id, room).await;
    for i in 0..70 {
        state(
            &store,
            account.account_id,
            room,
            "im.ponies.room_emotes",
            &format!("a{i:03}"),
            json!({"images":{"bad\nname":{"url":"mxc://example.org/paged"}}}),
            2,
        )
        .await;
    }
    state(
        &store,
        account.account_id,
        room,
        "im.ponies.room_emotes",
        "z-valid",
        json!({"images":{"wave":{"url":"mxc://example.org/paged"}}}),
        3,
    )
    .await;
    let app = media_app(
        store.clone(),
        Arc::new(ConfiguredMediaProxy::ok(b"GIF89apack")),
    );
    let path = format!("/v1/media/{}/example.org/paged", account.account_id);
    assert_eq!(get_media_bytes(&app, &path, None).await.0, StatusCode::OK);
    // Without the final usable pack, exhaustion still denies access.
    state(
        &store,
        account.account_id,
        room,
        "im.ponies.room_emotes",
        "z-valid",
        json!({}),
        4,
    )
    .await;
    assert_eq!(
        get_media_bytes(&app, &path, Some("*")).await.0,
        StatusCode::NOT_FOUND
    );
    store.delete_account_row(account.account_id).await.unwrap();
}

async fn space(store: &Store, account: &axon_store::Account, room: &str) {
    joined(store, account.account_id, &account.user_id, room).await;
    state(
        store,
        account.account_id,
        room,
        "m.room.create",
        "",
        json!({"type":"m.space"}),
        1,
    )
    .await;
    state(store, account.account_id, room, "m.room.image_pack", "main",
        json!({"images":{"shared":{"url":format!("mxc://example.org/{}", &room[1..room.find(':').unwrap()]),"usage":["emoticon","sticker"]}}}), 2).await;
}

async fn parent(store: &Store, account: Uuid, child: &str, parent: &str, ts: i64) {
    state(
        store,
        account,
        child,
        "m.space.parent",
        parent,
        json!({"canonical":true,"via":["localhost"]}),
        ts,
    )
    .await;
    state(
        store,
        account,
        parent,
        "m.space.child",
        child,
        json!({"via":["localhost"]}),
        ts,
    )
    .await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_packs_canonical_ancestors_order_usage_and_removal() {
    let store = store().await;
    let a = store
        .upsert_account(
            &format!("@inherit-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!child:localhost";
    joined(&store, a.account_id, &a.user_id, room).await;
    personal(&store, a.account_id, json!({"images":{"personal":{"url":"mxc://example.org/personal","usage":["emoticon","sticker"]}}})).await;
    state(&store,a.account_id,room,"m.room.image_pack","main",
        json!({"images":{"local":{"url":"mxc://example.org/local","usage":["emoticon","sticker"]}}}),2).await;
    // Room IDs deliberately sort in the opposite order to inheritance depth.
    for id in [
        "!z-parent:localhost",
        "!a-grandparent:localhost",
        "!zz-other:localhost",
    ] {
        space(&store, &a, id).await;
    }
    parent(&store, a.account_id, room, "!z-parent:localhost", 3).await;
    parent(
        &store,
        a.account_id,
        "!z-parent:localhost",
        "!a-grandparent:localhost",
        3,
    )
    .await;
    // Multiple valid canonical claims use the lexicographically lowest ID.
    parent(&store, a.account_id, room, "!zz-other:localhost", 3).await;
    let app = media_app(store.clone(), Arc::new(ConfiguredMediaProxy::ok(b"image")));
    let uri = format!("/v1/accounts/{}/rooms/{room}/emoji-packs", a.account_id);
    for suffix in ["", "?usage=sticker"] {
        let (status, body) = get(&app, &format!("{uri}{suffix}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["partial"], false);
        let packs = body["data"]["packs"].as_array().unwrap();
        assert_eq!(
            packs
                .iter()
                .map(|p| p["room_id"].clone())
                .collect::<Vec<_>>(),
            vec![
                Value::Null,
                json!(room),
                json!("!z-parent:localhost"),
                json!("!a-grandparent:localhost")
            ]
        );
        assert!(packs
            .iter()
            .all(|p| p["images"].as_array().unwrap().len() == 1));
    }
    assert!(store
        .image_pack_authorizes_media(a.account_id, "mxc://example.org/z-parent")
        .await
        .unwrap());
    // A cycle stops before repeating packs and reports bounded traversal.
    parent(
        &store,
        a.account_id,
        "!a-grandparent:localhost",
        "!z-parent:localhost",
        4,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["partial"], true);
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 4);
    // Clearing the parent's claim immediately removes the grandparent.
    state(
        &store,
        a.account_id,
        "!z-parent:localhost",
        "m.space.parent",
        "!a-grandparent:localhost",
        json!({}),
        5,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 3);
    assert_eq!(body["data"]["partial"], false);
    // Departure removes an ancestor even if its cached packs and links remain.
    state(
        &store,
        a.account_id,
        "!z-parent:localhost",
        "m.room.member",
        &a.user_id,
        json!({"membership":"leave"}),
        6,
    )
    .await;
    state(
        &store,
        a.account_id,
        room,
        "m.space.parent",
        "!zz-other:localhost",
        json!({}),
        6,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 2);
    assert!(!store
        .image_pack_authorizes_media(a.account_id, "mxc://example.org/z-parent")
        .await
        .unwrap());
    store.delete_account_row(a.account_id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_packs_parent_claim_validation() {
    let store = store().await;
    let a = store
        .upsert_account(
            &format!("@claims-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!claims:localhost";
    let ancestor = "!ancestor:localhost";
    joined(&store, a.account_id, &a.user_id, room).await;
    space(&store, &a, ancestor).await;
    state(
        &store,
        a.account_id,
        ancestor,
        "m.space.child",
        room,
        json!({"via":["localhost"]}),
        2,
    )
    .await;
    let app = media_app(store.clone(), Arc::new(ConfiguredMediaProxy::ok(b"image")));
    let uri = format!("/v1/accounts/{}/rooms/{room}/emoji-packs", a.account_id);
    for (index, content) in [
        json!({"via":["localhost"]}),
        json!({"via":["localhost"],"canonical":false}),
        json!({"via":["localhost"],"canonical":"true"}),
        json!({"via":[],"canonical":true}),
        json!({"via":"localhost","canonical":true}),
        json!({"via":[42],"canonical":true}),
        json!({"via":[""],"canonical":true}),
        // Valid claim, but neither reciprocal nor authorized.
        json!({"via":["localhost"],"canonical":true}),
    ]
    .into_iter()
    .enumerate()
    {
        if index == 7 {
            state(
                &store,
                a.account_id,
                ancestor,
                "m.space.child",
                room,
                json!({}),
                9,
            )
            .await;
        }
        state(
            &store,
            a.account_id,
            room,
            "m.space.parent",
            ancestor,
            content,
            10 + index as i64,
        )
        .await;
        let (_, body) = get(&app, &uri).await;
        assert_eq!(body["data"]["packs"], json!([]), "case {index}: {body}");
    }
    // The sender's current membership and power can validate a one-way claim.
    joined(&store, a.account_id, "@pack-owner:localhost", ancestor).await;
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.power_levels",
        "",
        json!({"users":{"@pack-owner:localhost":50}}),
        20,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 1, "{body}");
    for (index, levels) in [
        json!({"users":{"@pack-owner:localhost":49}}),
        json!({"users":{"@pack-owner:localhost":"100"}}),
        json!({"users":{"@pack-owner:localhost":100},"events":{"m.space.child":101}}),
        json!({"users":{"@pack-owner:localhost":1e100}}),
    ]
    .into_iter()
    .enumerate()
    {
        state(
            &store,
            a.account_id,
            ancestor,
            "m.room.power_levels",
            "",
            levels,
            21 + index as i64,
        )
        .await;
        let (_, body) = get(&app, &uri).await;
        assert_eq!(body["data"]["packs"], json!([]), "case {index}: {body}");
    }
    // State-event overrides and user defaults have the same precedence as Matrix.
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.power_levels",
        "",
        json!({"users_default":40,"state_default":100,"events":{"m.space.child":40}}),
        26,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 1);
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.member",
        "@pack-owner:localhost",
        json!({"membership":"leave"}),
        27,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"], json!([]));
    // A reciprocal link validates the claim regardless of the sender's power.
    parent(&store, a.account_id, room, ancestor, 30).await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 1);
    state(
        &store,
        a.account_id,
        ancestor,
        "m.space.child",
        room,
        json!({"via":[]}),
        31,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"], json!([]));
    parent(&store, a.account_id, room, ancestor, 32).await;
    // Even retained content cannot revive a redacted parent claim.
    store
        .upsert_room_state_with_redaction(
            &RoomStateUpsert {
                account_id: a.account_id,
                room_id: room,
                event_type: "m.space.parent",
                state_key: ancestor,
                event_id: &format!("$pack-32-{ancestor}"),
                sender: "@pack-owner:localhost",
                origin_ts: 32,
                content: Some(json!({"canonical":true,"via":["localhost"]})),
            },
            None,
            RoomStateRedaction::Redacted {
                event_id: Some("$redact-parent"),
            },
        )
        .await
        .unwrap();
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"], json!([]));
    parent(&store, a.account_id, room, ancestor, 33).await;

    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.create",
        "",
        json!({"type":"ordinary"}),
        33,
    )
    .await;
    let (_, body) = get(&app, &uri).await;
    assert_eq!(body["data"]["packs"], json!([]));
    store.delete_account_row(a.account_id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_packs_ancestor_depth_budget_and_account_isolation() {
    let store = store().await;
    let a = store
        .upsert_account(
            &format!("@depth-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let b = store
        .upsert_account(
            &format!("@depth-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!depth:localhost";
    for account in [&a, &b] {
        joined(&store, account.account_id, &account.user_id, room).await;
    }
    let mut previous = room.to_owned();
    for index in 0..17 {
        let id = format!("!depth-{index}:localhost");
        space(&store, &a, &id).await;
        parent(&store, a.account_id, &previous, &id, 2).await;
        previous = id;
    }
    let app = media_app(store.clone(), Arc::new(ConfiguredMediaProxy::ok(b"image")));
    let (_, body) = get(
        &app,
        &format!("/v1/accounts/{}/rooms/{room}/emoji-packs", a.account_id),
    )
    .await;
    assert_eq!(body["data"]["partial"], true, "{body}");
    assert_eq!(body["data"]["packs"].as_array().unwrap().len(), 16);
    let (_, body) = get(
        &app,
        &format!("/v1/accounts/{}/rooms/{room}/emoji-packs", b.account_id),
    )
    .await;
    assert_eq!(body["data"]["packs"], json!([]));
    assert_eq!(body["data"]["partial"], false);
    store.delete_account_row(a.account_id).await.unwrap();
    store.delete_account_row(b.account_id).await.unwrap();
}

#[derive(Clone)]
struct PackLogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for PackLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn logged_packs(app: &axum::Router, uri: &str, level: tracing::Level) -> (Value, String) {
    use tracing::instrument::WithSubscriber;

    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = PackLogWriter(output.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(level)
        .with_writer(move || writer.clone())
        .finish();
    let (status, body) = get(app, uri).with_subscriber(subscriber).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    (body, logs)
}

fn inheritance_log(logs: &str) -> &str {
    logs.lines()
        .find(|line| line.contains("axon_store::image_packs: image pack inheritance stopped"))
        .expect("inheritance diagnostic")
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn emoji_packs_debug_explains_inheritance_without_changing_results() {
    let store = store().await;
    let a = store
        .upsert_account(
            &format!("@pack-logs-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let room = "!logging-child:localhost";
    let ancestor = "!logging-parent:localhost";
    joined(&store, a.account_id, &a.user_id, room).await;
    space(&store, &a, ancestor).await;
    let app = media_app(store.clone(), Arc::new(ConfiguredMediaProxy::ok(b"image")));
    let uri = format!("/v1/accounts/{}/rooms/{room}/emoji-packs", a.account_id);
    let (debug_body, logs) = logged_packs(&app, &uri, tracing::Level::DEBUG).await;
    assert!(inheritance_log(&logs).contains("stop_reason=\"no_parent_link\""));
    let (info_body, logs) = logged_packs(&app, &uri, tracing::Level::INFO).await;
    assert_eq!(debug_body, info_body);
    assert!(!logs.contains("image pack inheritance stopped"));
    for (index, content, reason) in [
        (0, json!({"via":["localhost"]}), "no_canonical_parent"),
        (1, json!({"canonical":true,"via":[]}), "invalid_parent_link"),
        (
            2,
            json!({"canonical":true,"via":["localhost"]}),
            "unverifiable_parent",
        ),
    ] {
        state(
            &store,
            a.account_id,
            room,
            "m.space.parent",
            ancestor,
            content,
            10 + index,
        )
        .await;
        let (_, logs) = logged_packs(&app, &uri, tracing::Level::DEBUG).await;
        assert!(
            inheritance_log(&logs).contains(&format!("stop_reason=\"{reason}\"")),
            "{logs}"
        );
    }
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.member",
        &a.user_id,
        json!({"membership":"leave"}),
        20,
    )
    .await;
    let (_, logs) = logged_packs(&app, &uri, tracing::Level::DEBUG).await;
    assert!(inheritance_log(&logs).contains("stop_reason=\"parent_not_joined\""));
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.member",
        &a.user_id,
        json!({"membership":"join"}),
        21,
    )
    .await;
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.create",
        "",
        json!({}),
        22,
    )
    .await;
    let (_, logs) = logged_packs(&app, &uri, tracing::Level::DEBUG).await;
    assert!(inheritance_log(&logs).contains("stop_reason=\"parent_not_space\""));
    state(
        &store,
        a.account_id,
        ancestor,
        "m.room.create",
        "",
        json!({"type":"m.space"}),
        23,
    )
    .await;
    parent(&store, a.account_id, room, ancestor, 24).await;
    state(&store,a.account_id,ancestor,"m.room.image_pack","main",json!({
        "pack":{"display_name":"do-not-log-pack-name"},
        "images":{
            "do-not-log-shortcode":{"url":"mxc://example.org/do-not-log-image-url","body":"do-not-log-body","usage":["sticker"]},
            "both":{"url":"mxc://example.org/both"}
        }
    }),25).await;
    let (debug_body, logs) = logged_packs(&app, &uri, tracing::Level::DEBUG).await;
    assert!(inheritance_log(&logs).contains("ancestor_count=1"));
    assert!(inheritance_log(&logs).contains(&format!("stop_room_id=\"{ancestor}\"")));
    let summary = logs
        .lines()
        .find(|line| line.contains("image pack discovery completed"))
        .unwrap();
    for field in [
        "pack_count=1",
        "image_count=1",
        "filtered_images=1",
        "elapsed_ms=",
    ] {
        assert!(summary.contains(field), "{summary}");
    }
    assert!(
        !logs.contains("do-not-log-"),
        "pack content must not reach logs"
    );
    let (info_body, _) = logged_packs(&app, &uri, tracing::Level::INFO).await;
    assert_eq!(debug_body, info_body);
    parent(&store, a.account_id, ancestor, ancestor, 26).await;
    let (body, logs) = logged_packs(&app, &uri, tracing::Level::DEBUG).await;
    assert_eq!(body["data"]["partial"], true);
    assert!(inheritance_log(&logs).contains("stop_reason=\"cycle\""));
    state(
        &store,
        a.account_id,
        ancestor,
        "m.space.parent",
        ancestor,
        json!({}),
        27,
    )
    .await;
    let mut previous = ancestor.to_owned();
    for index in 0..16 {
        let next = format!("!logging-depth-{index}:localhost");
        space(&store, &a, &next).await;
        parent(&store, a.account_id, &previous, &next, 28).await;
        previous = next;
    }
    let (body, logs) = logged_packs(&app, &uri, tracing::Level::DEBUG).await;
    assert_eq!(body["data"]["partial"], true);
    assert!(inheritance_log(&logs).contains("stop_reason=\"depth_limit\""));
    assert!(inheritance_log(&logs).contains("ancestor_count=16"));
    store.delete_account_row(a.account_id).await.unwrap();
}
