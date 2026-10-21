//! Which timeline events can wake a phone (ADR 0113).
//!
//! The count rise is the user's push rules. This module only decides whether
//! one event is an eligible `m.room.message`, and which push-action class it
//! carries. It does not read the ruleset again.

use serde_json::Value;

/// How the live path classified this event's push actions.
///
/// `Unknown` is `None` from the SDK: the actions were not computed. It is not
/// a ruleset that declined to notify. Match [`Self::Notify`] on `Action::Notify`
/// only. `Action::should_notify` also accepts an in-app-only action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushClass {
    Notify,
    Silent,
    Unknown,
}

/// One eligible message carried on an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushMessage {
    pub event_id: String,
    pub sender: String,
    pub body: String,
    pub thread_root_id: Option<String>,
    pub class: PushClass,
}

/// Collapse whitespace the way the web client's preview does, then trim.
pub(crate) fn collapse_whitespace(text: &str) -> String {
    let mut out = String::new();
    let mut pending = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending = true;
            continue;
        }
        if pending && !out.is_empty() {
            out.push(' ');
        }
        pending = false;
        out.push(ch);
    }
    out
}

/// `None` when the event must not wake anyone.
///
/// A still-encrypted event, an edit, the account's own message, and a blank
/// body are dropped. `m.notice` is not special-cased. A thread reply is kept,
/// and its root is recorded so the tap can open that thread.
pub(crate) fn consider_event(
    raw: &Value,
    own_user_id: &str,
    class: PushClass,
) -> Option<PushMessage> {
    if raw.get("type").and_then(Value::as_str) != Some("m.room.message") {
        return None;
    }
    let content = raw.get("content")?.as_object()?;
    let sender = raw.get("sender").and_then(Value::as_str)?;
    if sender == own_user_id {
        return None;
    }
    let event_id = raw.get("event_id").and_then(Value::as_str)?;
    if event_id.is_empty() {
        return None;
    }
    let relates = content.get("m.relates_to").and_then(Value::as_object);
    let rel_type = relates
        .and_then(|rel| rel.get("rel_type"))
        .and_then(Value::as_str);
    if rel_type == Some("m.replace") {
        return None;
    }
    let thread_root_id = (rel_type == Some("m.thread"))
        .then(|| {
            relates
                .and_then(|rel| rel.get("event_id"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .flatten();
    let body = content
        .get("body")
        .and_then(Value::as_str)
        .map(collapse_whitespace)
        .unwrap_or_default();
    if body.is_empty() {
        return None;
    }
    Some(PushMessage {
        event_id: event_id.to_owned(),
        sender: sender.to_owned(),
        body,
        thread_root_id,
        class,
    })
}

/// `@localpart` of a Matrix user id. A value that is not an id is returned unchanged.
pub(crate) fn at_localpart(user_id: &str) -> String {
    let Some(rest) = user_id.strip_prefix('@') else {
        return user_id.to_owned();
    };
    let local = rest.split_once(':').map(|(local, _)| local).unwrap_or(rest);
    format!("@{local}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(body: &str) -> Value {
        json!({
            "type": "m.room.message",
            "event_id": "$event",
            "sender": "@bob:example",
            "content": { "msgtype": "m.text", "body": body }
        })
    }

    #[test]
    fn an_ordinary_message_is_eligible() {
        let message =
            consider_event(&message("hello"), "@alice:example", PushClass::Notify).unwrap();
        assert_eq!(message.event_id, "$event");
        assert_eq!(message.body, "hello");
        assert_eq!(message.class, PushClass::Notify);
        assert_eq!(message.thread_root_id, None);
    }

    #[test]
    fn eligibility_drops_the_cases_that_must_not_wake() {
        assert!(consider_event(&message("hello"), "@bob:example", PushClass::Notify).is_none());
        let mut edit = message("edited");
        edit["content"]["m.relates_to"] = json!({"rel_type": "m.replace", "event_id": "$old"});
        assert!(consider_event(&edit, "@alice:example", PushClass::Notify).is_none());
        assert!(consider_event(&message("   \n\t"), "@alice:example", PushClass::Notify).is_none());
        let encrypted = json!({
            "type": "m.room.encrypted",
            "event_id": "$utd",
            "sender": "@bob:example",
            "content": { "algorithm": "m.megolm.v1.aes-sha2" }
        });
        assert!(consider_event(&encrypted, "@alice:example", PushClass::Notify).is_none());
        let redaction = json!({
            "type": "m.room.redaction",
            "event_id": "$redact",
            "sender": "@bob:example",
            "content": {}
        });
        assert!(consider_event(&redaction, "@alice:example", PushClass::Notify).is_none());
    }

    #[test]
    fn a_notice_and_a_thread_reply_stay_eligible() {
        let mut notice = message("psst");
        notice["content"]["msgtype"] = json!("m.notice");
        assert!(consider_event(&notice, "@alice:example", PushClass::Notify).is_some());

        let mut thread = message("reply");
        thread["content"]["m.relates_to"] = json!({"rel_type": "m.thread", "event_id": "$root"});
        let message = consider_event(&thread, "@alice:example", PushClass::Silent).unwrap();
        assert_eq!(message.thread_root_id.as_deref(), Some("$root"));
        assert_eq!(message.class, PushClass::Silent);
        assert_eq!(message.body, "reply");
    }

    #[test]
    fn whitespace_collapses_and_localpart_keeps_the_at_sign() {
        assert_eq!(collapse_whitespace("  a \n b\t"), "a b");
        assert_eq!(at_localpart("@bob:example"), "@bob");
        assert_eq!(at_localpart("bob"), "bob");
    }
}
