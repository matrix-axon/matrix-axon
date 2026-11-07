-- Unjoined-room discovery from space hierarchies (ADR 0128 decisions 3, 4,
-- and 6).
--
-- Never merged into room_state or room_summaries: discovered summaries have
-- different access, completeness, and freshness semantics and no event
-- provenance (ADR 0111). Times are Unix milliseconds from the PostgreSQL
-- clock.

-- One row per account and discovered room, shared by every space that links
-- the room. `summary` holds only the fields that passed validation, as a JSON
-- object; a field missing from the latest answer is missing here too.
-- `withheld` names the fields that failed validation. Every status other
-- than `available` carries no fields.
CREATE TABLE room_discovery (
    account_id  UUID    NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    room_id     TEXT    NOT NULL,
    status      TEXT    NOT NULL
        CHECK (status IN ('available', 'not_returned', 'forbidden', 'not_found')),
    source      TEXT    NOT NULL CHECK (source IN ('hierarchy', 'summary')),
    summary     JSONB,
    withheld    TEXT[]  NOT NULL DEFAULT '{}',
    observed_at BIGINT  NOT NULL,
    expires_at  BIGINT  NOT NULL,
    PRIMARY KEY (account_id, room_id),
    CHECK ((status = 'available') = (summary IS NOT NULL)),
    CHECK (summary IS NULL OR jsonb_typeof(summary) = 'object'),
    CHECK (status = 'available' OR withheld = '{}')
);

-- One row per account and space: the space's crawl. `generation` counts
-- crawls started from the first page; resuming from `cursor` keeps it.
-- `tokens` are the `from` tokens this generation has used, for loop
-- detection, bounded by MAX_PAGES. `failures` counts consecutive transient
-- failures for the backoff and survives a restart from the first page.
CREATE TABLE space_discovery (
    account_id      UUID    NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    space_id        TEXT    NOT NULL,
    generation      BIGINT  NOT NULL CHECK (generation > 0),
    outcome         TEXT    NOT NULL
        CHECK (outcome IN ('in_progress', 'complete', 'truncated', 'failed')),
    cursor          TEXT,
    tokens          TEXT[]  NOT NULL DEFAULT '{}',
    pages           INTEGER NOT NULL DEFAULT 0 CHECK (pages >= 0),
    page_limit      INTEGER NOT NULL CHECK (page_limit > 0),
    restarts        INTEGER NOT NULL DEFAULT 0 CHECK (restarts >= 0),
    unidentified    INTEGER NOT NULL DEFAULT 0 CHECK (unidentified >= 0),
    rooms_cached    INTEGER NOT NULL DEFAULT 0 CHECK (rooms_cached >= 0),
    started_at      BIGINT  NOT NULL,
    completed_at    BIGINT,
    expires_at      BIGINT,
    support         TEXT    NOT NULL DEFAULT 'unknown'
        CHECK (support IN ('unknown', 'supported', 'unsupported')),
    failure         TEXT
        CHECK (failure IN ('timeout', 'server_error', 'rate_limited', 'transport',
                           'unauthorized', 'invalid_response', 'forbidden', 'not_found',
                           'too_large', 'pagination_loop', 'cursor_expired')),
    failures        INTEGER NOT NULL DEFAULT 0 CHECK (failures >= 0),
    next_attempt_at BIGINT,
    PRIMARY KEY (account_id, space_id),
    -- Only an unfinished crawl has a cursor to resume from.
    CHECK (cursor IS NULL OR outcome IN ('in_progress', 'failed')),
    CHECK ((outcome IN ('complete', 'truncated')) = (completed_at IS NOT NULL)),
    CHECK (outcome <> 'complete' OR failure IS NULL)
);

-- For each (account, space, child), the generation in which the space's
-- crawl last returned the child: completion marks a child `not_returned`
-- only when this generation did not return it (decision 3).
CREATE TABLE space_discovery_children (
    account_id UUID   NOT NULL,
    space_id   TEXT   NOT NULL,
    child_id   TEXT   NOT NULL,
    generation BIGINT NOT NULL,
    PRIMARY KEY (account_id, space_id, child_id),
    FOREIGN KEY (account_id, space_id)
        REFERENCES space_discovery (account_id, space_id) ON DELETE CASCADE
);

-- Reverse lookup of a child's links, for "does any joined space still link
-- this room": the room_state primary key leads with the parent room.
CREATE INDEX room_state_space_child_by_child
    ON room_state (account_id, state_key)
    WHERE event_type = 'm.space.child';

-- Whether a joined space of the account links `p_room_id` with a current
-- `m.space.child` (a valid `via`; the same rule as `SPACE_LINK_IS_CURRENT`
-- in axon-store's spaces.rs). Joined here is the summary's `hidden_left`
-- flag, which errs toward keeping a row; reads list only the requested
-- space's links and withhold discovery for joined rooms, so a kept row is
-- never shown out of place.
CREATE FUNCTION room_discovery_is_linked(p_account_id UUID, p_room_id TEXT)
RETURNS BOOLEAN AS $$
    SELECT EXISTS (
        SELECT 1 FROM room_state l
        JOIN room_summaries s ON s.account_id = l.account_id AND s.room_id = l.room_id
        WHERE l.account_id = p_account_id AND l.event_type = 'm.space.child'
          AND l.state_key = p_room_id AND NOT s.hidden_left
          AND l.redacted IS DISTINCT FROM true
          AND jsonb_typeof(l.content->'via') = 'array'
          AND l.content->'via' <> '[]'::jsonb
          AND NOT jsonb_path_exists(l.content->'via', '$[*] ? (@.type() != "string" || @ == "")'))
$$ LANGUAGE sql STABLE;

-- A new membership epoch (ADR 0126) is a leave, a rejoin, or a new own
-- member event (decision 6):
--   * the room's own discovery row goes, so synced state is authoritative
--     from a join on, and a row written by a commit that raced the join never
--     outlives the membership;
--   * as a space, its crawl and returned set go, and with them every
--     discovered row it linked that no joined space still links.
-- Joining a room that has no summary row yet starts no epoch here; the store
-- deletes its discovery row in the transaction that writes the member event.
CREATE FUNCTION delete_left_room_discovery() RETURNS TRIGGER AS $$
BEGIN
    DELETE FROM room_discovery
    WHERE account_id = NEW.account_id AND room_id = NEW.room_id;
    DELETE FROM space_discovery
    WHERE account_id = NEW.account_id AND space_id = NEW.room_id;
    DELETE FROM room_discovery d
    WHERE d.account_id = NEW.account_id
      AND d.room_id IN (SELECT l.state_key FROM room_state l
                        WHERE l.account_id = NEW.account_id AND l.room_id = NEW.room_id
                          AND l.event_type = 'm.space.child')
      AND NOT room_discovery_is_linked(d.account_id, d.room_id);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER room_summaries_delete_left_discovery
    AFTER UPDATE ON room_summaries
    FOR EACH ROW
    WHEN (NEW.membership_epoch IS DISTINCT FROM OLD.membership_epoch)
    EXECUTE FUNCTION delete_left_room_discovery();

-- A purge or a summary rebuild deletes the summary row rather than updating
-- it, and a rebuilt row takes a new epoch, so anything an in-flight crawl
-- recorded against the old row is stale; the space's crawl and the room's
-- own discovery row go with it, as ADR 0126's cascade does for its details.
-- Its children's rows are left to the orphan step of the next complete
-- crawl, because a purge may already have removed the links that name them.
CREATE FUNCTION delete_purged_room_discovery() RETURNS TRIGGER AS $$
BEGIN
    DELETE FROM room_discovery
    WHERE account_id = OLD.account_id AND room_id = OLD.room_id;
    DELETE FROM space_discovery
    WHERE account_id = OLD.account_id AND space_id = OLD.room_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER room_summaries_delete_purged_discovery
    AFTER DELETE ON room_summaries
    FOR EACH ROW
    EXECUTE FUNCTION delete_purged_room_discovery();
