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
-- PostgreSQL can't serve an index-only scan from an expression over a JSONB
-- column (`relates_to->>'event_id'`) without the whole column in the index,
-- and `relates_to` is the sender's entire `m.relates_to` object, unbounded in
-- size: a few KB of extra keys would exceed the 2704-byte B-tree row limit and
-- fail both this migration and every later insert of such an event. So the
-- thread root gets a plain column instead, bounded to the 255 bytes the Matrix
-- spec allows an event ID, and the index covers that.

-- The thread root of a non-state `m.thread` member, else NULL. A root ID over
-- 255 bytes is not a valid event ID, so no real root can match it; such a
-- member stays in `events` (and in the thread-scoped timeline) but is left out
-- of the room's thread summaries.
ALTER TABLE events ADD COLUMN thread_root TEXT;

-- Kept in step with `relates_to` and `raw_event` by the database, like
-- `updated_at` elsewhere, so no write path has to remember it — including
-- `update_decrypted_event`, which learns `relates_to` only once a UTD decrypts.
CREATE FUNCTION events_set_thread_root() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.thread_root := CASE
        WHEN NEW.relates_to->>'rel_type' = 'm.thread'
         AND NEW.raw_event->>'state_key' IS NULL
         AND octet_length(NEW.relates_to->>'event_id') <= 255
        THEN NEW.relates_to->>'event_id'
    END;
    RETURN NEW;
END;
$$;

CREATE TRIGGER events_set_thread_root
    BEFORE INSERT OR UPDATE OF relates_to, raw_event ON events
    FOR EACH ROW EXECUTE FUNCTION events_set_thread_root();

-- Existing rows. Only thread members change, so this rewrites a small fraction
-- of the table; it sets the column directly, so the trigger does not fire.
UPDATE events
SET thread_root = relates_to->>'event_id'
WHERE relates_to->>'rel_type' = 'm.thread'
  AND raw_event->>'state_key' IS NULL
  AND octet_length(relates_to->>'event_id') <= 255;

-- Every column here is bounded or already a key of another `events` index
-- (`room_id` in `events_room_timeline_idx`, `event_id` in the unique key), so
-- no event this index accepts is one the table would otherwise have rejected.
-- `INCLUDE` covers what the aggregate reads, for an index-only scan.
CREATE INDEX events_room_thread_root_idx
    ON events (account_id, room_id, thread_root)
    INCLUDE (origin_ts, id, event_id)
    WHERE thread_root IS NOT NULL;
