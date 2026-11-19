-- Holes that limited sliding-sync responses leave in a room's stored history.
-- See ADR 0140 (#1125).
--
-- When more than `sync.timeline_limit` events reach a room between two sync
-- responses, the homeserver returns only the newest ones with `limited: true`
-- and a `prev_batch` token. Everything between those events and what Axon
-- already had is a hole that ordinary backfill (ADR 0043) never reaches,
-- because backfill only pages behind the oldest stored event.
--
-- Each row is one hole (or one lag probe), recorded by the room-update watcher
-- and paged backward by the backfill task until it meets history stored before
-- the hole. A surrogate key lets one room hold any number of holes and probes,
-- and replacing `from_token` after each page can never collide with another row.
--
--   * origin           — 'limited' (a limited response), 'lag_probe' (the
--                        watcher's broadcast lagged, ADR 0140 decision 3), or
--                        'token_rejected' (the homeserver rejected a saved
--                        token and the row restarted from the live end).
--   * from_token       — the next page's `from`; NULL means the live end.
--   * stored_before    — the stop boundary: a page holding an event whose row
--                        was stored (`events.received_at`) before it closes the
--                        row. A lag probe's is set when it is recorded; a
--                        limited row's is NULL until the filler resolves it to
--                        the earliest `received_at` among the batch's events.
--   * batch_event_ids  — the limited batch's event ids, for the first-window
--                        test (a room's first window is not a hole).
--   * batch_checked    — the first-window test has run and found a hole, so it
--                        is not evaluated again. Lag probes are created checked;
--                        they have no batch.
--   * stop_event_ids   — a lag probe's extra stop: the room's lowest-id event
--                        (the first Axon stored there) when the probe was
--                        recorded, so a room first seen during the lag is not
--                        walked to its start. Empty for other rows.
--   * pages_fetched, events_recovered — progress, for logs and status.
--   * consecutive_failures, last_failure, retry_at, parked — the ADR 0121
--                        failure backoff, as on `room_backfill`.
--
-- Rows go with the account (ON DELETE CASCADE) and with a purged room
-- (`purge_room`, under the same preserve guard as `room_backfill`).
CREATE TABLE room_timeline_gaps (
    id                    BIGSERIAL   PRIMARY KEY,
    account_id            UUID        NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    room_id               TEXT        NOT NULL,
    origin                TEXT        NOT NULL
                          CHECK (origin IN ('limited', 'lag_probe', 'token_rejected')),
    from_token            TEXT,
    stored_before         TIMESTAMPTZ,
    batch_event_ids       TEXT[]      NOT NULL DEFAULT '{}',
    batch_checked         BOOLEAN     NOT NULL DEFAULT false,
    stop_event_ids        TEXT[]      NOT NULL DEFAULT '{}',
    detected_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    pages_fetched         INTEGER     NOT NULL DEFAULT 0,
    events_recovered      BIGINT      NOT NULL DEFAULT 0,
    consecutive_failures  INTEGER     NOT NULL DEFAULT 0,
    last_failure          TEXT,
    retry_at              TIMESTAMPTZ,
    parked                BOOLEAN     NOT NULL DEFAULT false,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX room_timeline_gaps_room_idx ON room_timeline_gaps (account_id, room_id);

CREATE TRIGGER room_timeline_gaps_set_updated_at
    BEFORE UPDATE ON room_timeline_gaps
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();
