-- Room summaries of selected unjoined rooms (ADR 0128 decision 5).
--
-- A room-summary answer itself lands in room_discovery (source 'summary').
-- This table holds what a request leaves behind that is not a summary: the
-- endpoint's support, the last failure, and the backoff. A row can exist
-- before the room has any room_discovery row, such as after a first request
-- that failed. Times are Unix milliseconds from the PostgreSQL clock.
--
-- `next_attempt_at` is the end of a failure backoff, or, while `support` is
-- 'unsupported', when the endpoint is checked again.
CREATE TABLE room_discovery_refresh (
    account_id      UUID    NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    room_id         TEXT    NOT NULL,
    support         TEXT    NOT NULL DEFAULT 'unknown'
        CHECK (support IN ('unknown', 'supported', 'unsupported')),
    failure         TEXT
        CHECK (failure IN ('timeout', 'server_error', 'rate_limited', 'transport',
                           'unauthorized', 'invalid_response')),
    failures        INTEGER NOT NULL DEFAULT 0 CHECK (failures >= 0),
    next_attempt_at BIGINT,
    PRIMARY KEY (account_id, room_id),
    CHECK (support <> 'unsupported' OR next_attempt_at IS NOT NULL),
    CHECK ((failure IS NULL) = (failures = 0))
);

-- Refresh state goes wherever the room's discovery row goes (decision 6):
-- on a new membership epoch of the room, and, for a space, with every
-- discovered row it linked that no joined space still links. These replace
-- the step 3 functions with the same statements plus the refresh rows.
CREATE OR REPLACE FUNCTION delete_left_room_discovery() RETURNS TRIGGER AS $$
BEGIN
    DELETE FROM room_discovery
    WHERE account_id = NEW.account_id AND room_id = NEW.room_id;
    DELETE FROM room_discovery_refresh
    WHERE account_id = NEW.account_id AND room_id = NEW.room_id;
    DELETE FROM space_discovery
    WHERE account_id = NEW.account_id AND space_id = NEW.room_id;
    DELETE FROM room_discovery d
    WHERE d.account_id = NEW.account_id
      AND d.room_id IN (SELECT l.state_key FROM room_state l
                        WHERE l.account_id = NEW.account_id AND l.room_id = NEW.room_id
                          AND l.event_type = 'm.space.child')
      AND NOT room_discovery_is_linked(d.account_id, d.room_id);
    DELETE FROM room_discovery_refresh r
    WHERE r.account_id = NEW.account_id
      AND r.room_id IN (SELECT l.state_key FROM room_state l
                        WHERE l.account_id = NEW.account_id AND l.room_id = NEW.room_id
                          AND l.event_type = 'm.space.child')
      AND NOT room_discovery_is_linked(r.account_id, r.room_id);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION delete_purged_room_discovery() RETURNS TRIGGER AS $$
BEGIN
    DELETE FROM room_discovery
    WHERE account_id = OLD.account_id AND room_id = OLD.room_id;
    DELETE FROM room_discovery_refresh
    WHERE account_id = OLD.account_id AND room_id = OLD.room_id;
    DELETE FROM space_discovery
    WHERE account_id = OLD.account_id AND space_id = OLD.room_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
