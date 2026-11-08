//! Room summaries of one selected unjoined room (ADR 0128 decisions 5, 10,
//! and 11).
//!
//! A [`axon_core::RoomDetailJob::Summary`] job asks the store whether the
//! room still qualifies and its summary is due, picks the endpoint the
//! account's homeserver advertises in `/versions`, and sends one
//! `GET /_matrix/client/v1/room_summary/{roomId}` (or MSC3266's unstable
//! `im.nheko.summary` path) with `via` from the linking `m.space.child`
//! events. The answer is validated field by field exactly like a hierarchy
//! entry. Logs name outcomes and withheld field names, never names, topics,
//! aliases, bodies, or tokens.
use std::time::Duration;

use axon_store::{SummaryBegin, SummaryLimits, SummaryStep, SummaryWrite};
use matrix_sdk::ruma::{OwnedServerName, RoomId};
use serde_json::Value;

use super::hierarchy::parse_entry;
use super::{ms, Get, NoAnswer, Ran, Reply, Upstream, Worker};
use super::{
    CONCLUSIVE_DENIAL_TTL, FAILURE_BACKOFF_MAX, FAILURE_BACKOFF_MIN, MAX_RESPONSE_BYTES,
    UNSUPPORTED_TTL,
};

/// One room; matches ADR 0126's request timeout.
pub(crate) const SUMMARY_TIMEOUT: Duration = Duration::from_secs(10);
/// A selected room is being looked at; one refresh per hour per room bounds
/// repeat reads.
pub(crate) const SUMMARY_TTL: Duration = Duration::from_secs(60 * 60);
/// Matches the servers Synapse tries per room.
pub(crate) const MAX_VIA_SERVERS: usize = 3;

/// MSC3266's unstable feature flag in `/versions`.
pub(crate) const UNSTABLE_FEATURE: &str = "im.nheko.summary";

pub(crate) fn limits() -> SummaryLimits {
    SummaryLimits {
        summary_ttl_ms: ms(SUMMARY_TTL),
        min_backoff_ms: ms(FAILURE_BACKOFF_MIN),
        max_backoff_ms: ms(FAILURE_BACKOFF_MAX),
    }
}

/// Which room-summary endpoint the account's homeserver serves, from its
/// `/versions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryEndpoint {
    /// Matrix v1.15 or later: `GET /_matrix/client/v1/room_summary/{roomId}`.
    Stable,
    /// Only the `im.nheko.summary` feature:
    /// `GET /_matrix/client/unstable/im.nheko.summary/rooms/{roomId}/summary`,
    /// the MSC3266 path every Synapse with the feature serves.
    Unstable,
    /// Neither: no request is made.
    Unsupported,
    /// `/versions` is not in the SDK's cache, or the cache could not be read.
    Unknown,
}

impl SummaryEndpoint {
    /// The endpoint advertised by a `/versions` answer: whether it lists
    /// Matrix v1.15 or later, and its enabled unstable features.
    pub(crate) fn advertised<'a>(v1_15: bool, mut features: impl Iterator<Item = &'a str>) -> Self {
        if v1_15 {
            Self::Stable
        } else if features.any(|feature| feature == UNSTABLE_FEATURE) {
            Self::Unstable
        } else {
            Self::Unsupported
        }
    }
}

/// At most [`MAX_VIA_SERVERS`] of `vias` that parse as server names, in
/// order.
pub(crate) fn via_servers(vias: &[String]) -> Vec<&str> {
    vias.iter()
        .map(String::as_str)
        .filter(|via| OwnedServerName::try_from(*via).is_ok())
        .take(MAX_VIA_SERVERS)
        .collect()
}

fn failure(category: &'static str, retry_after_ms: Option<i64>) -> SummaryStep {
    SummaryStep::Failure {
        category,
        retry_after_ms,
    }
}

/// What the cache records for `reply` to a summary request for `room_id`. A
/// `200` must describe the requested room; its `membership` is ignored,
/// because local membership is authoritative.
pub(crate) fn interpret(reply: Reply, room_id: &str) -> SummaryStep {
    match reply {
        Reply::Failed(NoAnswer::Timeout) => failure("timeout", None),
        Reply::Failed(NoAnswer::Transport) => failure("transport", None),
        Reply::Failed(NoAnswer::Unauthorized) => failure("unauthorized", None),
        // One room's summary has no children_state; a body this large is
        // not a summary.
        Reply::TooLarge => failure("invalid_response", None),
        Reply::Ok(body) => serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|body| parse_entry(&body))
            .filter(|entry| entry.room_id == room_id)
            .map_or_else(|| failure("invalid_response", None), SummaryStep::Answer),
        Reply::Status {
            status,
            errcode,
            retry_after_ms,
        } => match (status, errcode.as_deref()) {
            (401, _) => failure("unauthorized", retry_after_ms),
            (403, _) => SummaryStep::Denied {
                status: "forbidden",
                ttl_ms: ms(CONCLUSIVE_DENIAL_TTL),
            },
            (404, Some("M_NOT_FOUND")) => SummaryStep::Denied {
                status: "not_found",
                ttl_ms: ms(CONCLUSIVE_DENIAL_TTL),
            },
            (404, None | Some("M_UNRECOGNIZED")) | (405, _) => SummaryStep::Unsupported {
                ttl_ms: ms(UNSUPPORTED_TTL),
            },
            (429, _) => failure("rate_limited", retry_after_ms),
            (500..=599, _) => failure("server_error", retry_after_ms),
            _ => failure("invalid_response", None),
        },
    }
}

/// A short, allowlisted name for a step, for logs.
fn step_label(step: &SummaryStep) -> &'static str {
    match step {
        SummaryStep::Answer(_) => "available",
        SummaryStep::Denied { status, .. } => status,
        SummaryStep::Unsupported { .. } => "unsupported",
        SummaryStep::Failure { category, .. } => category,
    }
}

impl<U: Upstream> Worker<U> {
    /// Run one summary job for the unjoined `room_id`.
    pub(super) async fn run_summary(&mut self, room_id: &RoomId) -> Ran {
        let account_id = self.account_id;
        let mut ran = Ran::default();
        let vias = match self
            .store
            .begin_room_summary(account_id, room_id.as_str())
            .await
        {
            Ok(SummaryBegin::Request { vias }) => vias,
            Ok(SummaryBegin::NotDue) => return ran,
            Ok(SummaryBegin::Ineligible) => {
                tracing::debug!(target: "axon_sync::acquisition", %account_id, %room_id, kind = "summary", outcome = "ineligible", "skipped room summary");
                return ran;
            }
            Err(error) => {
                tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = "summary", reason = error.diagnostic_reason(), "room summary could not read its cache");
                ran.local_failure = true;
                return ran;
            }
        };
        let endpoint = self.upstream.summary_endpoint().await;
        let via = via_servers(&vias);
        let (step, elapsed_ms) = match endpoint {
            SummaryEndpoint::Unsupported => (
                SummaryStep::Unsupported {
                    ttl_ms: ms(UNSUPPORTED_TTL),
                },
                0,
            ),
            SummaryEndpoint::Unknown => (failure("transport", None), 0),
            SummaryEndpoint::Stable | SummaryEndpoint::Unstable => {
                let stable = ["room_summary", room_id.as_str()];
                let unstable = ["im.nheko.summary", "rooms", room_id.as_str(), "summary"];
                let (version, segments): (&'static str, &[&str]) =
                    if endpoint == SummaryEndpoint::Stable {
                        ("v1", &stable)
                    } else {
                        ("unstable", &unstable)
                    };
                let query: Vec<(&str, &str)> = via.iter().map(|server| ("via", *server)).collect();
                let (reply, elapsed_ms) = self
                    .paced(Get {
                        version,
                        segments,
                        query: &query,
                        body_limit: Some(MAX_RESPONSE_BYTES),
                        timeout: SUMMARY_TIMEOUT,
                    })
                    .await;
                (interpret(reply, room_id.as_str()), elapsed_ms)
            }
        };
        let label = step_label(&step);
        let withheld: Vec<&str> = match &step {
            SummaryStep::Answer(entry) => entry.withheld.clone(),
            _ => Vec::new(),
        };
        let write = match self
            .store
            .record_room_summary(account_id, room_id.as_str(), limits(), &step)
            .await
        {
            Ok(write) => write,
            Err(error) => {
                tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = "summary", outcome = label, reason = error.diagnostic_reason(), "room summary could not record its result");
                ran.local_failure = true;
                return ran;
            }
        };
        log_summary(
            account_id,
            room_id,
            endpoint,
            label,
            elapsed_ms,
            via.len(),
            &withheld,
            &write,
        );
        ran.changed = !write.fenced && write.changed;
        ran
    }
}

#[allow(clippy::too_many_arguments)]
fn log_summary(
    account_id: uuid::Uuid,
    room_id: &RoomId,
    endpoint: SummaryEndpoint,
    label: &'static str,
    elapsed_ms: u64,
    via: usize,
    withheld: &[&str],
    write: &SummaryWrite,
) {
    let kind = "summary";
    let outcome = if write.fenced { "fenced" } else { label };
    tracing::debug!(
        target: "axon_sync::acquisition",
        %account_id,
        %room_id,
        kind,
        ?endpoint,
        outcome,
        elapsed_ms,
        via,
        ?withheld,
        changed = write.changed,
        "room summary finished"
    );
    if !write.fenced && write.failures > 1 {
        tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind, outcome, elapsed_ms, failures = write.failures, "room summary keeps failing; backing off");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOM: &str = "!child:localhost";

    fn status(status: u16, errcode: Option<&str>) -> Reply {
        Reply::Status {
            status,
            errcode: errcode.map(str::to_owned),
            retry_after_ms: Some(7_000),
        }
    }

    fn ok(body: Value) -> Reply {
        Reply::Ok(serde_json::to_vec(&body).unwrap())
    }

    #[test]
    fn the_endpoint_follows_versions_then_the_unstable_feature() {
        let none = std::iter::empty::<&str>;
        assert_eq!(
            SummaryEndpoint::advertised(true, ["im.nheko.summary"].into_iter()),
            SummaryEndpoint::Stable
        );
        assert_eq!(
            SummaryEndpoint::advertised(false, ["im.nheko.summary"].into_iter()),
            SummaryEndpoint::Unstable
        );
        assert_eq!(
            SummaryEndpoint::advertised(false, ["org.matrix.msc3266"].into_iter()),
            SummaryEndpoint::Unsupported
        );
        assert_eq!(
            SummaryEndpoint::advertised(false, none()),
            SummaryEndpoint::Unsupported
        );
    }

    #[test]
    fn via_keeps_valid_servers_up_to_the_limit() {
        let vias: Vec<String> = ["a.org", "not a server", "b.org:8448", "c.org", "d.org"]
            .map(str::to_owned)
            .into();
        assert_eq!(via_servers(&vias), ["a.org", "b.org:8448", "c.org"]);
    }

    #[test]
    fn an_answer_must_describe_the_requested_room() {
        let SummaryStep::Answer(entry) = interpret(
            ok(json!({
                "room_id": ROOM,
                "name": "General",
                "membership": "join",
                "num_joined_members": "many",
            })),
            ROOM,
        ) else {
            panic!("a summary of the room is an answer");
        };
        assert_eq!(entry.summary, json!({"name": "General"}));
        assert_eq!(entry.withheld, ["num_joined_members"]);
        for body in [
            json!({"room_id": "!other:localhost", "name": "Other"}),
            json!({"name": "no id"}),
            json!(["not an object"]),
        ] {
            assert_eq!(
                interpret(ok(body.clone()), ROOM),
                failure("invalid_response", None),
                "{body}"
            );
        }
        assert_eq!(
            interpret(Reply::Ok(b"not json".to_vec()), ROOM),
            failure("invalid_response", None)
        );
    }

    #[test]
    fn replies_map_to_the_documented_steps() {
        assert_eq!(
            interpret(status(403, Some("M_FORBIDDEN")), ROOM),
            SummaryStep::Denied {
                status: "forbidden",
                ttl_ms: ms(CONCLUSIVE_DENIAL_TTL)
            }
        );
        assert_eq!(
            interpret(status(404, Some("M_NOT_FOUND")), ROOM),
            SummaryStep::Denied {
                status: "not_found",
                ttl_ms: ms(CONCLUSIVE_DENIAL_TTL)
            }
        );
        for reply in [
            status(404, Some("M_UNRECOGNIZED")),
            status(404, None),
            status(405, None),
        ] {
            assert_eq!(
                interpret(reply, ROOM),
                SummaryStep::Unsupported {
                    ttl_ms: ms(UNSUPPORTED_TTL)
                }
            );
        }
        assert_eq!(
            interpret(status(400, Some("M_UNKNOWN")), ROOM),
            failure("invalid_response", None),
            "Synapse's missing-via 400 is not an answer"
        );
        assert_eq!(
            interpret(status(429, None), ROOM),
            failure("rate_limited", Some(7_000))
        );
        assert_eq!(
            interpret(status(503, None), ROOM),
            failure("server_error", Some(7_000))
        );
        assert_eq!(
            interpret(status(401, None), ROOM),
            failure("unauthorized", Some(7_000))
        );
        assert_eq!(
            interpret(Reply::TooLarge, ROOM),
            failure("invalid_response", None)
        );
        assert_eq!(
            interpret(Reply::Failed(NoAnswer::Timeout), ROOM),
            failure("timeout", None)
        );
    }

    #[test]
    fn the_limits_match_the_adr() {
        assert_eq!(SUMMARY_TIMEOUT, Duration::from_secs(10));
        assert_eq!(SUMMARY_TTL, Duration::from_secs(3600));
        assert_eq!(MAX_VIA_SERVERS, 3);
    }
}
