-- New message/decryption writes derive this bounded projection using an HTML
-- parser, never a regex. NULL marks historical rows awaiting resumable repair;
-- an empty array means the row has been parsed and contains no usable images.
-- Startup migrations are transactional and finish before client traffic starts.
ALTER TABLE events ADD COLUMN inline_media_urls TEXT[];
CREATE INDEX events_inline_media_urls_idx ON events USING gin (inline_media_urls)
    WHERE event_type = 'm.room.message' AND inline_media_urls <> '{}';
CREATE INDEX events_inline_media_pending_idx ON events (id)
    WHERE event_type = 'm.room.message' AND inline_media_urls IS NULL;

-- Remember applied authority rather than re-evaluating a moderator's current
-- power on every read, which could restore redacted media after demotion.
CREATE TABLE event_redaction_proofs (
    account_id UUID NOT NULL,
    room_id TEXT NOT NULL,
    redaction_event_id TEXT NOT NULL,
    target_event_id TEXT NOT NULL,
    PRIMARY KEY (account_id, redaction_event_id),
    FOREIGN KEY (account_id, redaction_event_id) REFERENCES events (account_id, event_id) ON DELETE CASCADE,
    FOREIGN KEY (account_id, target_event_id) REFERENCES events (account_id, event_id) ON DELETE CASCADE
);
CREATE INDEX event_redaction_proofs_target_idx ON event_redaction_proofs (account_id, target_event_id);
