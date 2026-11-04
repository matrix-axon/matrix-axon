//! Alert text and the provider JSON (ADR 0113, ADR 0129).
//!
//! Title and body are clipped before they are serialized. A payload that would
//! still exceed 4096 bytes is rebuilt as the activity alert. If that too would
//! exceed the limit, the wake is dropped.
//!
//! ADR 0129 adds two bodies that show nothing: the APNs badge-only push and
//! the FCM room-read message.

use sha2::{Digest, Sha256};

use super::message::{collapse_whitespace, PushMessage};

const CLIP: usize = 140;
const MAX_PAYLOAD_BYTES: usize = 4096;
pub(crate) const MESSAGE_CHANNEL_ID: &str = "messages-v2";
/// The data value that marks an FCM room-read message (ADR 0129). An older
/// client has no handler for it and drops a data message without `event_id`.
pub(crate) const ROOM_READ_KIND: &str = "room_read";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlertText {
    pub title: String,
    pub body: String,
}

/// The banner for one disclosure level.
///
/// Names are already resolved. A missing room name is `New message`. A missing
/// sender display is the caller's `@localpart`.
pub(crate) fn alert_text(
    disclosure: &str,
    room_name: Option<&str>,
    sender_display: &str,
    message: &PushMessage,
) -> AlertText {
    match disclosure {
        "sender_and_room" => AlertText {
            title: clip(room_name.unwrap_or("New message"), CLIP),
            body: clip(sender_display.trim(), CLIP),
        },
        "preview" => AlertText {
            title: clip(room_name.unwrap_or("New message"), CLIP),
            body: clip(&preview_body(sender_display, &message.body), CLIP),
        },
        _ => activity_text(),
    }
}

fn activity_text() -> AlertText {
    AlertText {
        title: "Axon".to_owned(),
        body: "New activity".to_owned(),
    }
}

/// `sender: body`, with the body clipped at 140 before the prefix.
///
/// This matches `messageNotificationBody` in the web client. The payload clips
/// the whole string again.
fn preview_body(sender: &str, body: &str) -> String {
    let text = collapse_whitespace(body);
    let who = sender.trim();
    let clipped = clip(&text, CLIP);
    if who.is_empty() {
        if clipped.is_empty() {
            "New message".to_owned()
        } else {
            clipped
        }
    } else if clipped.is_empty() {
        who.to_owned()
    } else {
        format!("{who}: {clipped}")
    }
}

pub(crate) fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// First 32 hex characters of the SHA-256 of the room id.
///
/// APNs allows 64 bytes for `apns-collapse-id`. A room id can be longer than that.
pub(crate) fn collapse_id(room_id: &str) -> String {
    let digest = Sha256::digest(room_id.as_bytes());
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The APNs alert. `badge` is the instance unread total for a registration
/// that opted in (ADR 0129), or `None` to leave the icon alone.
pub(crate) fn apns_body_for(
    text: &AlertText,
    account_id: &str,
    room_id: &str,
    message: &PushMessage,
    badge: Option<u64>,
) -> Vec<u8> {
    let mut payload = serde_json::json!({
        "aps": {
            "alert": { "title": text.title, "body": text.body }
        },
        "account_id": account_id,
        "room_id": room_id,
        "event_id": message.event_id,
    });
    if let Some(badge) = badge {
        payload["aps"]["badge"] = serde_json::Value::from(badge);
    }
    if let Some(thread_root_id) = &message.thread_root_id {
        payload["thread_root_id"] = serde_json::Value::String(thread_root_id.clone());
    }
    serde_json::to_vec(&payload).unwrap_or_default()
}

/// The APNs badge-only push (ADR 0129). Zero clears the icon.
///
/// No alert, no sound, and no `content-available`, so iOS applies the number
/// without running the app.
pub(crate) fn apns_badge_body(total: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "aps": { "badge": total } })).unwrap_or_default()
}

/// The FCM room-read message (ADR 0129): data only, normal priority.
///
/// There is no `notification` block, so the system shows nothing. There is no
/// `collapse_key`: FCM keeps at most four collapse keys per device, and a key
/// per room would drop most clears for a device that was offline. `tag` is the
/// room's collapse value, the tag the alert and the local toast posted under.
pub(crate) fn fcm_room_read_body(
    token: &str,
    account_id: &str,
    room_id: &str,
    collapse: &str,
) -> Vec<u8> {
    let payload = serde_json::json!({
        "message": {
            "token": token,
            "data": {
                "axon_kind": ROOM_READ_KIND,
                "account_id": account_id,
                "room_id": room_id,
                "tag": collapse,
            },
            "android": { "priority": "NORMAL" }
        }
    });
    serde_json::to_vec(&payload).unwrap_or_default()
}

pub(crate) fn fcm_body_for(
    text: &AlertText,
    account_id: &str,
    room_id: &str,
    message: &PushMessage,
    token: &str,
    collapse: &str,
) -> Vec<u8> {
    let mut data = serde_json::Map::new();
    data.insert(
        "account_id".to_owned(),
        serde_json::Value::String(account_id.to_owned()),
    );
    data.insert(
        "room_id".to_owned(),
        serde_json::Value::String(room_id.to_owned()),
    );
    data.insert(
        "event_id".to_owned(),
        serde_json::Value::String(message.event_id.clone()),
    );
    if let Some(thread_root_id) = &message.thread_root_id {
        data.insert(
            "thread_root_id".to_owned(),
            serde_json::Value::String(thread_root_id.clone()),
        );
    }
    let payload = serde_json::json!({
        "message": {
            "token": token,
            "notification": { "title": text.title, "body": text.body },
            "data": data,
            "android": {
                "priority": "HIGH",
                "notification": {
                    "channel_id": MESSAGE_CHANNEL_ID,
                    "tag": collapse,
                }
            }
        }
    });
    serde_json::to_vec(&payload).unwrap_or_default()
}

/// The bodies a send will use, or `None` when even the activity alert is too big.
pub(crate) struct OutboundRequest<'a> {
    pub provider: &'a str,
    pub disclosure: &'a str,
    pub room_name: Option<&'a str>,
    pub sender_display: &'a str,
    pub account_id: &'a str,
    pub room_id: &'a str,
    pub message: &'a PushMessage,
    pub fcm_token: &'a str,
    /// The APNs `aps.badge`, when the registration opted in and the total
    /// was read. FCM ignores it.
    pub badge: Option<u64>,
}

pub(crate) fn fit_outbound(request: &OutboundRequest<'_>) -> Option<(Vec<u8>, String)> {
    let collapse = collapse_id(request.room_id);
    let text = alert_text(
        request.disclosure,
        request.room_name,
        request.sender_display,
        request.message,
    );
    let body = provider_body(request, &text, &collapse);
    if body.len() <= MAX_PAYLOAD_BYTES {
        return Some((body, collapse));
    }
    let text = activity_text();
    let body = provider_body(request, &text, &collapse);
    if body.len() <= MAX_PAYLOAD_BYTES {
        return Some((body, collapse));
    }
    None
}

fn provider_body(request: &OutboundRequest<'_>, text: &AlertText, collapse: &str) -> Vec<u8> {
    if request.provider == "fcm" {
        fcm_body_for(
            text,
            request.account_id,
            request.room_id,
            request.message,
            request.fcm_token,
            collapse,
        )
    } else {
        apns_body_for(
            text,
            request.account_id,
            request.room_id,
            request.message,
            request.badge,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::message::PushClass;

    fn message(body: &str) -> PushMessage {
        PushMessage {
            event_id: "$event".to_owned(),
            sender: "@bob:example".to_owned(),
            body: body.to_owned(),
            thread_root_id: None,
            class: PushClass::Notify,
        }
    }

    #[test]
    fn disclosure_chooses_the_banner_and_clips_it() {
        let message = message(&"a".repeat(200));
        let activity = alert_text("activity", Some("Room"), "@bob", &message);
        assert_eq!(activity.title, "Axon");
        assert_eq!(activity.body, "New activity");

        let named = alert_text("sender_and_room", None, "@bob", &message);
        assert_eq!(named.title, "New message");
        assert_eq!(named.body, "@bob");

        let preview = alert_text("preview", Some("Room"), "@bob", &message);
        assert_eq!(preview.title, "Room");
        assert_eq!(preview.body.chars().count(), 140);
        assert!(preview.body.starts_with("@bob: "));
        assert!(preview.body.ends_with('…'));
    }

    #[test]
    fn the_collapse_id_is_32_hex_characters() {
        let id = collapse_id("!room:example");
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(collapse_id("!room:example"), id);
    }

    #[test]
    fn a_thread_root_is_omitted_until_the_message_has_one() {
        let message = message("hi");
        let text = activity_text();
        let apns = apns_body_for(&text, "account", "!room", &message, None);
        let parsed: serde_json::Value = serde_json::from_slice(&apns).unwrap();
        assert!(parsed.get("thread_root_id").is_none());
        assert_eq!(parsed["account_id"], "account");
        assert_eq!(parsed["event_id"], "$event");

        let mut threaded = message;
        threaded.thread_root_id = Some("$root".to_owned());
        let apns = apns_body_for(&text, "account", "!room", &threaded, None);
        let parsed: serde_json::Value = serde_json::from_slice(&apns).unwrap();
        assert_eq!(parsed["thread_root_id"], "$root");

        let fcm = fcm_body_for(&text, "account", "!room", &threaded, "device-token", "abc");
        let parsed: serde_json::Value = serde_json::from_slice(&fcm).unwrap();
        assert_eq!(parsed["message"]["data"]["thread_root_id"], "$root");
        assert_eq!(parsed["message"]["android"]["priority"], "HIGH");
        assert_eq!(
            parsed["message"]["android"]["notification"]["channel_id"],
            MESSAGE_CHANNEL_ID
        );
        assert_eq!(parsed["message"]["android"]["notification"]["tag"], "abc");
        assert!(parsed["message"].get("collapse_key").is_none());
    }

    #[test]
    fn an_oversized_preview_falls_back_to_activity_and_a_huge_token_is_dropped() {
        let message = message("hello");
        let (body, _) = fit_outbound(&request("apns", "preview", &message, "token")).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["aps"]["alert"]["body"], "@bob: hello");

        let huge_token = "t".repeat(5000);
        assert!(fit_outbound(&request("fcm", "activity", &message, &huge_token)).is_none());
        // The same token is not part of an APNs body, so that wake still fits.
        assert!(fit_outbound(&request("apns", "activity", &message, &huge_token)).is_some());
    }

    #[test]
    fn an_alert_carries_a_badge_only_when_one_is_given() {
        let message = message("hi");
        let text = activity_text();
        let plain: serde_json::Value =
            serde_json::from_slice(&apns_body_for(&text, "a", "!r", &message, None)).unwrap();
        assert!(plain["aps"].get("badge").is_none());

        let badged: serde_json::Value =
            serde_json::from_slice(&apns_body_for(&text, "a", "!r", &message, Some(7))).unwrap();
        assert_eq!(badged["aps"]["badge"], 7);
        assert_eq!(badged["aps"]["alert"]["body"], "New activity");

        let mut fcm = request("fcm", "activity", &message, "token");
        fcm.badge = Some(7);
        let (body, _) = fit_outbound(&fcm).unwrap();
        assert!(!String::from_utf8(body).unwrap().contains("badge"));
    }

    #[test]
    fn the_badge_only_push_has_no_alert_and_zero_is_sent() {
        let parsed: serde_json::Value = serde_json::from_slice(&apns_badge_body(0)).unwrap();
        assert_eq!(parsed, serde_json::json!({ "aps": { "badge": 0 } }));
    }

    #[test]
    fn the_room_read_message_is_data_only_and_normal_priority() {
        let collapse = collapse_id("!room:example");
        let body = fcm_room_read_body("device-token", "account", "!room:example", &collapse);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let message = &parsed["message"];
        assert!(message.get("notification").is_none());
        assert!(message.get("collapse_key").is_none());
        assert!(message["android"].get("notification").is_none());
        assert_eq!(message["android"]["priority"], "NORMAL");
        assert_eq!(message["data"]["axon_kind"], ROOM_READ_KIND);
        assert_eq!(message["data"]["room_id"], "!room:example");
        assert_eq!(message["data"]["account_id"], "account");
        assert_eq!(message["data"]["tag"], collapse.as_str());
        // An older Android client routes a data message by `event_id`. Leaving
        // it out is what makes this harmless before the client half ships.
        assert!(message["data"].get("event_id").is_none());
    }

    fn request<'a>(
        provider: &'a str,
        disclosure: &'a str,
        message: &'a PushMessage,
        token: &'a str,
    ) -> OutboundRequest<'a> {
        OutboundRequest {
            provider,
            disclosure,
            room_name: Some("Room"),
            sender_display: "@bob",
            account_id: "account",
            room_id: "!room:example",
            message,
            fcm_token: token,
            badge: None,
        }
    }
}
