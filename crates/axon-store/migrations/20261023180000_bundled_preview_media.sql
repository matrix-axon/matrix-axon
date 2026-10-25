-- Hash-based GIN entries keep sender-controlled JSON out of B-tree tuples.
-- The media proxy uses containment probes, not an array expansion per event.
CREATE INDEX events_bundled_preview_media_idx
    ON events USING gin ((content -> 'm.url_previews') jsonb_path_ops)
    WHERE event_type = 'm.room.message';

CREATE INDEX events_beeper_preview_media_idx
    ON events USING gin ((content -> 'com.beeper.linkpreviews') jsonb_path_ops)
    WHERE event_type = 'm.room.message';
