//! Pure extractors that lift the hot-column and crypto-provenance fields out
//! of event JSON and the SDK's [`EncryptionInfo`]. Kept separate from the I/O in
//! `engine.rs`/`redecrypt.rs` so they're trivially unit-testable. See ADR 0015.

use axon_store::EventCrypto;
use matrix_sdk::deserialized_responses::{
    AlgorithmInfo, EncryptionInfo, VerificationLevel, VerificationState,
};
use serde_json::Value;
use uuid::Uuid;

/// The plaintext `content.body` of a message event, if present and a string.
/// Populates `events.decrypted_body_text`. `None` for events with no textual
/// body (state events, redactions, UTDs).
pub(crate) fn body_text(content: &Value) -> Option<&str> {
    content.get("body")?.as_str()
}

/// The `m.relates_to` relation object from an event's `content`, captured
/// generically (replies, edits, annotations, `m.thread`). `None` if absent.
pub(crate) fn relates_to(content: &Value) -> Option<Value> {
    content.get("m.relates_to").cloned()
}

/// The `unsigned.prev_content` of a state event — the state content it
/// replaced (issue #31: lets a client distinguish an `m.room.member` real join
/// from a displayname/avatar change, both of which keep `membership: "join"`).
/// `None` when absent, e.g. message-like events and first-time state.
pub(crate) fn prev_content(raw_event: &Value) -> Option<Value> {
    raw_event.get("unsigned")?.get("prev_content").cloned()
}

/// For an `m.room.redaction` event, the `event_id` it redacts. Handles both the
/// top-level `redacts` (pre-v11 rooms) and `content.redacts` (room v11+). `None`
/// if neither is present or it isn't a string.
pub(crate) fn redacts(raw_event: &Value) -> Option<&str> {
    raw_event
        .get("redacts")
        .and_then(Value::as_str)
        .or_else(|| raw_event.get("content")?.get("redacts")?.as_str())
}

/// The crypto provenance of a decrypted event, extracted from the SDK's
/// [`EncryptionInfo`] into owned strings. Borrow into an [`EventCrypto`] via
/// [`CryptoMeta::as_event_crypto`] for persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CryptoMeta {
    session_id: Option<String>,
    curve25519_key: Option<String>,
    ed25519_key: Option<String>,
    forwarded: bool,
    forwarder_user_id: Option<String>,
    forwarder_device_id: Option<String>,
    device_id: Option<String>,
    verification_state: &'static str,
    sender_trust: &'static str,
}

impl CryptoMeta {
    /// Borrow the metadata into an [`EventCrypto`] ready for
    /// [`Store::upsert_event_crypto`](axon_store::Store::upsert_event_crypto).
    pub(crate) fn as_event_crypto<'a>(
        &'a self,
        account_id: Uuid,
        event_id: &'a str,
    ) -> EventCrypto<'a> {
        EventCrypto {
            account_id,
            event_id,
            session_id: self.session_id.as_deref(),
            curve25519_key: self.curve25519_key.as_deref(),
            ed25519_key: self.ed25519_key.as_deref(),
            forwarded: self.forwarded,
            forwarder_user_id: self.forwarder_user_id.as_deref(),
            forwarder_device_id: self.forwarder_device_id.as_deref(),
            device_id: self.device_id.as_deref(),
            verification_state: self.verification_state,
            sender_trust: Some(self.sender_trust),
        }
    }
}

/// Map the SDK's [`VerificationState`] to the four-valued M7c sender-trust
/// verdict recorded at decrypt time. This is the standard Matrix per-message
/// shield, collapsed to what a client needs to badge a sender:
///
/// * `verified` — the sending device is cross-signed by the sender's own master
///   key (it's really them).
/// * `verification_violation` — the sender's identity was already in conflict
///   when the event arrived (previously verified, identity since changed).
/// * `unknown` — the message couldn't be linked back to any device (deleted
///   device, key from an insecure source).
/// * `mismatched_sender` — the Megolm session belongs to a different user than
///   the event's `sender` (issue #59). Every message in an encrypted bridged
///   room arrives like this: the sender is a puppet, the session the bridge
///   bot's. The SDK withholds the session's device for these, so the
///   verification bundle's live device half is always empty for them.
/// * `unverified` — everything else unverified (unverified identity, unsigned
///   device).
///
/// Unlike the coarse [`crypto_meta`] `verification_state` (`verified`/
/// `unverified`), this preserves the *reason* a message is unverified.
pub(crate) fn sender_trust(state: &VerificationState) -> &'static str {
    match state {
        VerificationState::Verified => "verified",
        VerificationState::Unverified(VerificationLevel::VerificationViolation) => {
            "verification_violation"
        }
        VerificationState::Unverified(VerificationLevel::None(_)) => "unknown",
        VerificationState::Unverified(VerificationLevel::MismatchedSender) => "mismatched_sender",
        VerificationState::Unverified(_) => "unverified",
    }
}

/// Extract [`CryptoMeta`] from the SDK's [`EncryptionInfo`]. For a Megolm event
/// this yields the session id and the megolm creator's identity keys (also the
/// sending device's keys); for the Olm fallback only the curve25519 key.
pub(crate) fn crypto_meta(info: &EncryptionInfo) -> CryptoMeta {
    let (session_id, curve25519_key, ed25519_key) = match &info.algorithm_info {
        AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key,
            sender_claimed_keys,
            session_id,
        } => {
            let ed25519 = sender_claimed_keys
                .iter()
                .find(|(alg, _)| alg.as_str() == "ed25519")
                .map(|(_, key)| key.clone());
            (session_id.clone(), Some(curve25519_key.clone()), ed25519)
        }
        AlgorithmInfo::OlmV1Curve25519AesSha2 {
            curve25519_public_key_base64,
        } => (None, Some(curve25519_public_key_base64.clone()), None),
    };

    let verification_state = match info.verification_state {
        VerificationState::Verified => "verified",
        VerificationState::Unverified(_) => "unverified",
    };

    CryptoMeta {
        sender_trust: sender_trust(&info.verification_state),
        session_id,
        curve25519_key,
        ed25519_key,
        forwarded: info.forwarder.is_some(),
        forwarder_user_id: info.forwarder.as_ref().map(|f| f.user_id.to_string()),
        forwarder_device_id: info.forwarder.as_ref().map(|f| f.device_id.to_string()),
        device_id: info.sender_device.as_ref().map(|d| d.to_string()),
        verification_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn body_text_reads_message_body() {
        let content = json!({ "msgtype": "m.text", "body": "hi there" });
        assert_eq!(body_text(&content), Some("hi there"));
    }

    #[test]
    fn body_text_absent_is_none() {
        assert_eq!(body_text(&json!({ "membership": "join" })), None);
        // Present but non-string must not be coerced — the column is text.
        assert_eq!(body_text(&json!({ "body": 42 })), None);
    }

    #[test]
    fn relates_to_captures_relation() {
        let content = json!({
            "body": "edit",
            "m.relates_to": { "rel_type": "m.replace", "event_id": "$orig" }
        });
        let rel = relates_to(&content).expect("relation");
        assert_eq!(rel["rel_type"], "m.replace");
        assert_eq!(rel["event_id"], "$orig");
    }

    #[test]
    fn relates_to_absent_is_none() {
        assert_eq!(relates_to(&json!({ "body": "plain" })), None);
    }

    #[test]
    fn prev_content_reads_unsigned() {
        // A displayname change: membership stays "join", only displayname moves —
        // this is exactly what distinguishes it from a real join (issue #31).
        let raw_event = json!({
            "type": "m.room.member",
            "state_key": "@alice:example.org",
            "content": { "membership": "join", "displayname": "Alice B" },
            "unsigned": {
                "prev_content": { "membership": "join", "displayname": "Alice" }
            }
        });
        let prev = prev_content(&raw_event).expect("prev_content");
        assert_eq!(prev["membership"], "join");
        assert_eq!(prev["displayname"], "Alice");
    }

    #[test]
    fn prev_content_absent_is_none() {
        assert_eq!(prev_content(&json!({ "type": "m.room.message" })), None);
        assert_eq!(
            prev_content(&json!({ "type": "m.room.member", "unsigned": {} })),
            None
        );
    }

    #[test]
    fn redacts_reads_top_level() {
        let ev = json!({ "type": "m.room.redaction", "redacts": "$target" });
        assert_eq!(redacts(&ev), Some("$target"));
    }

    #[test]
    fn redacts_reads_content_for_v11() {
        let ev = json!({
            "type": "m.room.redaction",
            "content": { "redacts": "$target", "reason": "spam" }
        });
        assert_eq!(redacts(&ev), Some("$target"));
    }

    #[test]
    fn redacts_absent_is_none() {
        assert_eq!(redacts(&json!({ "type": "m.room.message" })), None);
    }

    #[test]
    fn sender_trust_maps_every_verification_level() {
        use matrix_sdk::deserialized_responses::DeviceLinkProblem;

        assert_eq!(sender_trust(&VerificationState::Verified), "verified");
        assert_eq!(
            sender_trust(&VerificationState::Unverified(
                VerificationLevel::VerificationViolation
            )),
            "verification_violation"
        );
        // "couldn't link to a device" → unknown, regardless of the link problem.
        assert_eq!(
            sender_trust(&VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::MissingDevice
            ))),
            "unknown"
        );
        assert_eq!(
            sender_trust(&VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::InsecureSource
            ))),
            "unknown"
        );
        // A session owned by someone other than the sender keeps its own verdict
        // (issue #59) rather than reading as an ordinary unverified device.
        assert_eq!(
            sender_trust(&VerificationState::Unverified(
                VerificationLevel::MismatchedSender
            )),
            "mismatched_sender"
        );
        // Everything else unverified collapses to the plain verdict.
        for level in [
            VerificationLevel::UnverifiedIdentity,
            VerificationLevel::UnsignedDevice,
        ] {
            assert_eq!(
                sender_trust(&VerificationState::Unverified(level)),
                "unverified"
            );
        }
    }
}
