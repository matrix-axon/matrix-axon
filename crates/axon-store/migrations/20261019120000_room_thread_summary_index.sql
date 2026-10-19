-- Serve a room's thread summaries (`Store::room_threads`, `GET …/threads`)
-- from an index-only scan (#763).
--
-- Until now the summary query could only use `events_relation_target_idx`,
-- which is keyed `(account_id, rel_type, event_id)` with no `room_id`. So it
-- read every thread member in the *account*, discarded the other rooms' rows
-- in a heap filter, and fetched (and detoasted) each row's `raw_event` just to
-- test `state_key`. On a production room with 18.8k thread members that cost
-- ~50k random heap reads and up to 18 s, past the 10 s statement timeout, so
-- the endpoint returned 500 and clients lost every thread badge in the room.
--
-- This index is scoped to one room's thread members, its predicate matches the
-- query's own thread and non-state filters, and it `INCLUDE`s every column the
-- aggregate reads (`relates_to` for the root expression, plus `origin_ts`,
-- `id` and `event_id` for the latest reply). A change to the query's
-- `WHERE` clause must keep matching this predicate, or the planner silently
-- falls back to the account-wide scan; `room_threads_uses_index_only_scan`
-- in `crates/axon-store/src/events.rs` guards that.
CREATE INDEX events_room_thread_root_idx
    ON events (account_id, room_id, (relates_to->>'event_id'))
    INCLUDE (relates_to, origin_ts, id, event_id)
    WHERE relates_to->>'rel_type' = 'm.thread'
      AND raw_event->>'state_key' IS NULL;
