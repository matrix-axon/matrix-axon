//! `POST /v1/rooms/previews` (#278): the request bounds and envelope. What a
//! preview is, edits, redactions and the scan limit, is pinned against the
//! store in `axon-store/tests/room_previews.rs`.
use super::*;

const URI: &str = "/v1/rooms/previews";

async fn post_previews(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    request(app, "POST", URI, Some(body), Some(&bearer())).await
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn previews_answer_in_request_order_once_per_room() {
    let store = store().await;
    let user = format!("@previews-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .unwrap()
        .account_id;
    let first = format!("!previews-a-{}:localhost", Uuid::new_v4());
    let second = format!("!previews-b-{}:localhost", Uuid::new_v4());
    let silent = format!("!previews-c-{}:localhost", Uuid::new_v4());
    let first_event = insert_message(&store, account_id, &first, 1_000, "hello").await;
    insert_message(&store, account_id, &second, 2_000, "there").await;
    let app = read_app(store);

    let room = |room_id: &str| json!({ "account_id": account_id, "room_id": room_id });
    let (status, body) = post_previews(
        &app,
        json!({ "rooms": [room(&second), room(&silent), room(&first), room(&second)] }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let data = body["data"].as_array().expect("data array");
    let rooms: Vec<&str> = data
        .iter()
        .map(|p| p["room_id"].as_str().unwrap())
        .collect();
    // The repeat is answered once; the room with nothing to show is absent.
    assert_eq!(rooms, vec![second.as_str(), first.as_str()]);
    assert_eq!(
        data[1],
        json!({
            "account_id": account_id,
            "room_id": first,
            "event_id": first_event,
            "sender": "@alice:localhost",
            "sender_display_name": null,
            "origin_ts": 1_000,
            "msgtype": "m.text",
            "body": "hello",
            "truncated": false,
        })
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn previews_reject_more_than_fifty_rooms_even_when_repeated() {
    let app = read_app(store().await);
    let account_id = Uuid::new_v4();
    let rooms = |n: usize| -> Vec<Value> {
        (0..n)
            .map(|_| json!({ "account_id": account_id, "room_id": "!same:localhost" }))
            .collect()
    };

    let (status, _) = post_previews(&app, json!({ "rooms": rooms(50) })).await;
    assert_eq!(status, StatusCode::OK);
    // Fifty-one copies of one room are still fifty-one rooms: the cap counts
    // what was sent, not what is left after dedupe.
    let (status, body) = post_previews(&app, json!({ "rooms": rooms(51) })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request", "{body}");
    let (status, body) = post_previews(&app, json!({ "rooms": [] })).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "data": [] })));
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn previews_reject_malformed_bodies() {
    let app = read_app(store().await);
    let account_id = Uuid::new_v4();

    for body in [
        json!({}),
        json!({ "rooms": [{ "account_id": "not-a-uuid", "room_id": "!r:localhost" }] }),
        json!({ "rooms": [{ "account_id": account_id }] }),
        json!({ "rooms": [], "extra": true }),
    ] {
        let (status, response) = post_previews(&app, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} → {response}");
        assert_eq!(response["error"]["code"], "bad_request", "{response}");
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn previews_cap_the_body_size_after_auth() {
    let app = read_app(store().await);
    let room_id = format!("!{}:localhost", "x".repeat(33 * 1024));
    let body = json!({ "rooms": [{ "account_id": Uuid::new_v4(), "room_id": room_id }] });

    let (status, _) = request(&app, "POST", URI, Some(body.clone()), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, response) = post_previews(&app, body).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{response}");
}
