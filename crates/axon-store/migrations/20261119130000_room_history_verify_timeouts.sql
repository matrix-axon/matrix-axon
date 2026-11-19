-- A verification walk that keeps timing out is closed rather than retried
-- (ADR 0140 decision 4, amended 2026-11-19).
--
-- A homeserver that no longer holds a room's history (it purged the room after
-- its last local user left, or a retention policy removed old events) fetches
-- it over federation to answer each `/messages` page: minutes of work per page,
-- long after Axon's 30 s timeout. Each retry gets the page the homeserver
-- finished meanwhile, so the walk's consecutive failures keep resetting, and
-- it never parks. `walk_timeouts` counts timed-out pages over the whole walk;
-- the third closes it as `timed_out`.
ALTER TABLE room_history_verify ADD COLUMN walk_timeouts INTEGER NOT NULL DEFAULT 0;

ALTER TABLE room_history_verify DROP CONSTRAINT room_history_verify_stop_reason_check;
ALTER TABLE room_history_verify ADD CONSTRAINT room_history_verify_stop_reason_check
    CHECK (stop_reason IN ('first_stored_event', 'floor', 'room_start', 'no_history', 'timed_out'));
