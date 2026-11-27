-- Explicit nested browsing uses the hierarchy state machine with its own
-- cursor, generation, and backoff. An existing depth-one crawl can resume.
CREATE TABLE space_browse_crawl (LIKE space_discovery INCLUDING ALL);
ALTER TABLE space_browse_crawl
    ADD FOREIGN KEY (account_id, space_id)
    REFERENCES room_summaries (account_id, room_id) ON DELETE CASCADE;

CREATE INDEX space_browse_crawl_by_start ON space_browse_crawl (account_id, started_at)
    WHERE rooms_cached > 0;

CREATE INDEX space_browse_crawl_expiry ON space_browse_crawl (started_at, account_id, space_id)
    WHERE rooms_cached > 0;

-- A returned room is a candidate, scoped to the joined space whose crawl
-- returned it. Only candidates reachable through current links are readable.
-- The crawl transaction discards all candidates before changing generation.
CREATE TABLE space_browse_rooms (
    account_id UUID NOT NULL,
    space_id   TEXT NOT NULL,
    room_id    TEXT NOT NULL,
    summary    JSONB,
    withheld   JSONB NOT NULL,
    children   JSONB NOT NULL,
    observed_at BIGINT NOT NULL,
    membership_epoch BIGINT,
    PRIMARY KEY (account_id, space_id, room_id),
    FOREIGN KEY (account_id, space_id)
        REFERENCES space_browse_crawl (account_id, space_id) ON DELETE CASCADE
);

CREATE INDEX space_browse_rooms_by_room ON space_browse_rooms (account_id, room_id);

-- A new membership epoch always fences an in-flight request. Keep committed
-- browse evidence when the account is still joined, including profile updates.
-- A membership loss or visibility transition clears the joined root and
-- remote data for the changed room under other roots.
CREATE FUNCTION invalidate_space_browse() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.hidden_left IS NOT DISTINCT FROM OLD.hidden_left
       AND NOT NEW.hidden_left
       AND EXISTS (
           SELECT 1 FROM accounts a JOIN room_state m
             ON m.account_id = a.account_id AND m.state_key = a.user_id
           WHERE a.account_id = NEW.account_id AND m.room_id = NEW.room_id
             AND m.event_type = 'm.room.member' AND m.content->>'membership' = 'join'
             AND m.redacted IS DISTINCT FROM true
       )
    THEN
        RETURN NULL;
    END IF;

    DELETE FROM space_browse_crawl
    WHERE account_id = NEW.account_id AND space_id = NEW.room_id;
    UPDATE space_browse_rooms
    SET summary = NULL, withheld = '[]', children = '{}'
    WHERE account_id = NEW.account_id AND room_id = NEW.room_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER room_summaries_invalidate_space_browse
    AFTER UPDATE ON room_summaries
    FOR EACH ROW
    WHEN (NEW.membership_epoch IS DISTINCT FROM OLD.membership_epoch)
    EXECUTE FUNCTION invalidate_space_browse();
