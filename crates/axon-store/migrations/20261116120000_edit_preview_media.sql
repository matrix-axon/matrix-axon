-- An image an edit adds to a link preview lives in the replacement's
-- `m.new_content` (ADR 0132 section 8), where the media proxy must find it.
-- Partial on replacements, so these index only edits; hash-based GIN entries
-- keep sender-controlled JSON out of B-tree tuples, as for sends.
CREATE INDEX events_edit_preview_media_idx
    ON events USING gin ((content -> 'm.new_content' -> 'm.url_previews') jsonb_path_ops)
    WHERE event_type = 'm.room.message' AND relates_to ->> 'rel_type' = 'm.replace';

CREATE INDEX events_edit_beeper_preview_media_idx
    ON events USING gin ((content -> 'm.new_content' -> 'com.beeper.linkpreviews') jsonb_path_ops)
    WHERE event_type = 'm.room.message' AND relates_to ->> 'rel_type' = 'm.replace';
