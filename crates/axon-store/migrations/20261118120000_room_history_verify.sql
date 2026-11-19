-- The one-time pass that re-reads each room's stored history against the
-- homeserver, to repair the holes limited syncs left before ADR 0140 started
-- recording them. See ADR 0140 decision 4 (as amended 2026-11-18), #1125.
--
-- One row per (account, room). The backfill task walks each joined room from
-- its live end, stores every page (already-stored events are no-ops), and
-- marks the row complete at the first stop:
--
--   * the page holds `stop_event_ids`, the room's lowest-`id` event: the first
--     event Axon stored there, so every limited-sync hole is above it;
--   * `floor_run` reaches 500: that many events in a row that were not stored
--     and are older than `floor_ts`, so the walk has passed everything Axon
--     stored and the homeserver no longer serves the stop event;
--   * the room's start.
--
-- Columns:
--   * from_token       — the next page's `from`; NULL means the live end.
--   * stop_event_ids, floor_ts — the stop points, NULL until the room's first
--                        page, when they are read from the room's rows. The
--                        floor is the oldest `origin_ts` among the room's
--                        `sync.timeline_limit` lowest-id events, which this
--                        migration cannot read.
--   * floor_run        — unstored events older than the floor seen in a row.
--   * empty_run        — empty pages in a row. Ten back the room off rather
--                        than count as progress, so a homeserver that keeps
--                        answering with empty pages parks the room.
--   * walk_generation  — bumped by every restart (the management action, an
--                        expired gap row, a kept room's purge). Every write a
--                        walk makes names the generation it read, so a walk
--                        already in flight cannot overwrite a restarted one.
--   * complete, stop_reason, completed_at — which stop ended the walk.
--   * pages_fetched, events_recovered — progress, for logs and status.
--   * consecutive_failures, last_failure, retry_at, parked, token_rejections
--                      — the ADR 0121 failure backoff, as on `room_backfill`.
--   * not_joined       — the account was not in the room when the backfill task
--                        last looked; only joined rooms are walked. For status.
--
-- Rows go with the account (ON DELETE CASCADE) and with a purged room
-- (`purge_room`, under the same preserve guard as `room_backfill`; a kept
-- room's incomplete row is restarted with fresh stop points).
CREATE TABLE room_history_verify (
    account_id            UUID        NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    room_id               TEXT        NOT NULL,
    from_token            TEXT,
    stop_event_ids        TEXT[],
    floor_ts              BIGINT,
    floor_run             INTEGER     NOT NULL DEFAULT 0,
    empty_run             INTEGER     NOT NULL DEFAULT 0,
    walk_generation       BIGINT      NOT NULL DEFAULT 0,
    complete              BOOLEAN     NOT NULL DEFAULT false,
    stop_reason           TEXT
                          CHECK (stop_reason IN ('first_stored_event', 'floor', 'room_start', 'no_history')),
    completed_at          TIMESTAMPTZ,
    pages_fetched         INTEGER     NOT NULL DEFAULT 0,
    events_recovered      BIGINT      NOT NULL DEFAULT 0,
    consecutive_failures  INTEGER     NOT NULL DEFAULT 0,
    last_failure          TEXT,
    retry_at              TIMESTAMPTZ,
    parked                BOOLEAN     NOT NULL DEFAULT false,
    token_rejections      INTEGER     NOT NULL DEFAULT 0,
    not_joined            BOOLEAN     NOT NULL DEFAULT false,
    queued_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, room_id)
);

-- The backfill task lists an account's incomplete rows once per sweep; once
-- the pass is done that is no rows, read without touching the finished ones.
CREATE INDEX room_history_verify_pending_idx
    ON room_history_verify (account_id) WHERE NOT complete;

CREATE TRIGGER room_history_verify_set_updated_at
    BEFORE UPDATE ON room_history_verify
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();

-- Seed every room that has stored events. The pass then runs on its own after
-- the upgrade, with nothing for a user or operator to do.
INSERT INTO room_history_verify (account_id, room_id)
SELECT DISTINCT account_id, room_id FROM events;
