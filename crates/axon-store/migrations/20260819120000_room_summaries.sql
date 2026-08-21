-- Persisted per-room activity markers for GET /v1/rooms (issue #85, ADR 0094).
--
-- list_rooms used to recompute last_activity_ts / last_event_id by aggregating
-- the whole events table on every request. This table holds one row per
-- (account_id, room_id) and is maintained incrementally by upsert_event and
-- update_decrypted_event (same-statement as the event write, like search_outbox).
-- last_event_row_id is events.id, kept so incremental compares use the same
-- (origin_ts, id) tiebreak as the old ORDER BY. last_activity_is_content records
-- whether the current marker came from a content-bearing event
-- (decrypted_body_text IS NOT NULL) so a later membership/redaction/UTD cannot
-- bump a room that already has real messages, while a first content event can
-- replace a newer non-content marker.
CREATE TABLE room_summaries (
    account_id               UUID        NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    room_id                  TEXT        NOT NULL,
    last_activity_ts         BIGINT      NOT NULL,
    last_event_id            TEXT        NOT NULL,
    last_event_row_id        BIGINT      NOT NULL,
    last_activity_is_content BOOLEAN     NOT NULL,
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, room_id)
);

CREATE INDEX room_summaries_activity_idx
    ON room_summaries (account_id, last_activity_ts DESC, room_id);

CREATE TRIGGER room_summaries_set_updated_at
    BEFORE UPDATE ON room_summaries
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();

-- One-time backfill from events. DISTINCT ON against the timeline index is a
-- single full scan; after this, writes keep the table current. Content-bearing
-- rooms first so the fallback insert only fills rooms that have events but no
-- decrypted_body_text.
INSERT INTO room_summaries (
    account_id, room_id, last_activity_ts, last_event_id,
    last_event_row_id, last_activity_is_content
)
SELECT DISTINCT ON (account_id, room_id)
    account_id, room_id, origin_ts, event_id, id, TRUE
FROM events
WHERE decrypted_body_text IS NOT NULL
ORDER BY account_id, room_id, origin_ts DESC, id DESC;

INSERT INTO room_summaries (
    account_id, room_id, last_activity_ts, last_event_id,
    last_event_row_id, last_activity_is_content
)
SELECT DISTINCT ON (e.account_id, e.room_id)
    e.account_id, e.room_id, e.origin_ts, e.event_id, e.id, FALSE
FROM events e
WHERE NOT EXISTS (
    SELECT 1 FROM room_summaries s
    WHERE s.account_id = e.account_id AND s.room_id = e.room_id
)
ORDER BY e.account_id, e.room_id, e.origin_ts DESC, e.id DESC;
