-- New message/decryption writes derive this bounded projection using an HTML
-- parser, never a regex. Existing messages retain their previous media behavior.
ALTER TABLE events ADD COLUMN inline_media_urls TEXT[] NOT NULL DEFAULT '{}';
CREATE INDEX events_inline_media_urls_idx ON events USING gin (inline_media_urls)
    WHERE event_type = 'm.room.message' AND inline_media_urls <> '{}';
