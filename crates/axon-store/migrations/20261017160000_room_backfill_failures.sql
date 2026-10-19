-- Persisted backoff and parking for failing backfill rooms. See ADR 0121.
--
-- ADR 0043 counted a room's consecutive `/messages` failures in memory and, after
-- three, re-paged the room from its live timeline end. A room whose page kept
-- timing out therefore looped forever, and a restart reset the count. These
-- columns move that state into the cursor row so it survives restarts:
--   * consecutive_failures — failed pages since the last successful one.
--   * last_failure — the class of the most recent failure (`retryable`,
--     `rejected`, `inaccessible`), for diagnosis. NULL after a success.
--   * retry_at — when the room may be paged again. NULL means now. A sweep skips a
--     room whose retry_at is still in the future without issuing a request.
--   * parked — the room failed too often (or the homeserver refused it) and is
--     retried only once per park interval; counted by `GET /v1/status`.
--   * token_rejections — how many times the homeserver has rejected the room's
--     resume token. A second rejection parks the room.
--
-- A successful page clears the first four. It does not reset token_rejections:
-- the pages back down to a rejected depth succeed, so a count they cleared could
-- never reach two.

ALTER TABLE room_backfill
    ADD COLUMN consecutive_failures INTEGER     NOT NULL DEFAULT 0,
    ADD COLUMN last_failure         TEXT,
    ADD COLUMN retry_at             TIMESTAMPTZ,
    ADD COLUMN parked               BOOLEAN     NOT NULL DEFAULT false,
    ADD COLUMN token_rejections     INTEGER     NOT NULL DEFAULT 0;
