//! Latest-message previews for room-list rows, many rooms per read (#278).
//!
//! A client used to resolve each row's preview itself: up to three timeline
//! pages per room, scanning for the first message with a body, plus the room's
//! whole member list for the sender's display name. A phone's room list fires
//! that for every visible row at once, about fifty requests on one connection.
//! [`Store::room_previews`] answers the same question for many rooms in one
//! statement, with the timeline read's own rules, so the result is what the
//! client would have found.

use sqlx_core::from_row::FromRow;
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use std::sync::LazyLock;
use uuid::Uuid;

use crate::events::latest_edit_lateral;
use crate::{Store, StoreError};

/// How far back a preview looks, in timeline rows: the three 50-event pages
/// the web client scanned before giving up. A room whose newest 150 visible
/// events hold no message (a bridge's state churn, a run of membership
/// changes) has no preview, as before.
pub const ROOM_PREVIEW_SCAN_LIMIT: i64 = 150;

/// The longest preview body returned, in characters (Unicode scalar values).
/// A row shows a line or two, so the rest is bytes on a slow link. On a
/// production account the 95th-percentile body was about 360 characters,
/// so most bodies arrive whole and `truncated` says when one did not.
pub const ROOM_PREVIEW_BODY_CHARS: i32 = 500;

/// The latest message of one room, as a room-list row previews it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomPreview {
    /// Axon account the room belongs to.
    pub account_id: Uuid,
    /// Matrix room ID.
    pub room_id: String,
    /// The message the preview shows. Not necessarily the room list's
    /// `last_event_id`, which can be a membership change or other event
    /// without a body.
    pub event_id: String,
    /// The message's sender.
    pub sender: String,
    /// The sender's `displayname` from the room's current `m.room.member`
    /// state, when it has one.
    pub sender_display_name: Option<String>,
    /// `origin_server_ts` of the message, in milliseconds.
    pub origin_ts: i64,
    /// `content.msgtype`, after its latest edit.
    pub msgtype: Option<String>,
    /// The message body after its latest edit, cut to
    /// [`ROOM_PREVIEW_BODY_CHARS`].
    pub body: String,
    /// Whether `body` was cut.
    pub truncated: bool,
}

impl FromRow<'_, PgRow> for RoomPreview {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(RoomPreview {
            account_id: row.try_get("account_id")?,
            room_id: row.try_get("room_id")?,
            event_id: row.try_get("event_id")?,
            sender: row.try_get("sender")?,
            sender_display_name: row.try_get("sender_display_name")?,
            origin_ts: row.try_get("origin_ts")?,
            msgtype: row.try_get("msgtype")?,
            body: row.try_get("body")?,
            truncated: row.try_get("truncated")?,
        })
    }
}

/// One statement for every requested room. Per room, a lateral read takes the
/// newest [`ROOM_PREVIEW_SCAN_LIMIT`] rows the timeline would show (the same
/// strip `room_timeline` applies: no redaction events, edits or reactions as
/// rows of their own), then the newest of them that is a non-state
/// `m.room.message`, not redacted, whose body after its latest edit has a
/// non-space character. That is `isPreviewEvent` in the web client, applied to
/// what `TIMELINE_SELECT` returns. The redaction match and the edit lateral
/// are the timeline's own fragments, so a preview never disagrees with the
/// row the user sees on opening the room.
///
/// `$1`/`$2` are parallel `uuid[]`/`text[]` arrays of the requested rooms,
/// `$3` the scan limit and `$4` the body cap. Rows come back in request order;
/// a room with no preview is absent.
static ROOM_PREVIEWS_SQL: LazyLock<String> = LazyLock::new(|| {
    let root_redaction = crate::event_visibility::redaction_matches("rr", "e");
    let latest_edit = latest_edit_lateral();
    format!(
        "SELECT req.account_id, req.room_id, p.event_id, p.sender, p.origin_ts, p.msgtype, \
                left(p.full_body, $4) AS body, char_length(p.full_body) > $4 AS truncated, \
                CASE WHEN jsonb_typeof(m.content->'displayname') = 'string' \
                     THEN m.content->>'displayname' END AS sender_display_name \
         FROM unnest($1::uuid[], $2::text[]) WITH ORDINALITY AS req(account_id, room_id, ord) \
         CROSS JOIN LATERAL ( \
             SELECT e.event_id, e.sender, e.origin_ts, \
                    COALESCE(edit.new_body, e.decrypted_body_text) AS full_body, \
                    CASE WHEN edit.new_content IS NOT NULL \
                         THEN edit.new_content->>'msgtype' \
                         ELSE e.content->>'msgtype' END AS msgtype \
             FROM ( \
                 SELECT tail.* FROM events tail \
                 WHERE tail.account_id = req.account_id AND tail.room_id = req.room_id \
                   AND tail.event_type <> 'm.room.redaction' \
                   AND COALESCE(tail.relates_to->>'rel_type', '') <> 'm.replace' \
                   AND NOT (COALESCE(tail.relates_to->>'rel_type', '') = 'm.annotation' \
                            AND tail.event_type = 'm.reaction') \
                 ORDER BY tail.origin_ts DESC, tail.id DESC \
                 LIMIT $3 \
             ) e \
             LEFT JOIN LATERAL ( \
                 SELECT rr.event_id FROM events rr \
                 WHERE {root_redaction} \
                 LIMIT 1 \
             ) r ON TRUE \
             {latest_edit} \
             WHERE e.event_type = 'm.room.message' \
               AND e.raw_event->>'state_key' IS NULL \
               AND r.event_id IS NULL \
               AND COALESCE(edit.new_body, e.decrypted_body_text) ~ '[^[:space:]]' \
             ORDER BY e.origin_ts DESC, e.id DESC \
             LIMIT 1 \
         ) p \
         LEFT JOIN room_state m \
           ON m.account_id = req.account_id AND m.room_id = req.room_id \
          AND m.event_type = 'm.room.member' AND m.state_key = p.sender \
         ORDER BY req.ord"
    )
});

impl Store {
    /// The latest-message preview of each `(account_id, room_id)` in `rooms`,
    /// in request order. A room with no previewable message in its newest
    /// [`ROOM_PREVIEW_SCAN_LIMIT`] timeline rows, or unknown to this store, is
    /// left out rather than reported as an error, as the timeline read answers
    /// an unknown room with an empty page. The caller bounds and dedupes
    /// `rooms`; a duplicate is answered twice.
    pub async fn room_previews(
        &self,
        rooms: &[(Uuid, String)],
    ) -> Result<Vec<RoomPreview>, StoreError> {
        if rooms.is_empty() {
            return Ok(Vec::new());
        }
        let (account_ids, room_ids): (Vec<Uuid>, Vec<String>) = rooms.iter().cloned().unzip();
        let rows = sqlx_core::query_as::query_as::<Postgres, RoomPreview>(&ROOM_PREVIEWS_SQL)
            .bind(account_ids)
            .bind(room_ids)
            .bind(ROOM_PREVIEW_SCAN_LIMIT)
            .bind(ROOM_PREVIEW_BODY_CHARS)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }
}
