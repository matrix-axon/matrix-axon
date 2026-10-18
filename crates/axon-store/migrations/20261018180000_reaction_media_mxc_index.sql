-- MSC4027 reaction images use the annotation key as a plaintext MXC URL.
-- Match the media lookup predicate so downloads do not scan account history.
CREATE INDEX IF NOT EXISTS events_content_reaction_key_idx
    ON events (account_id, (content->'m.relates_to'->>'key'), id)
    WHERE event_type = 'm.reaction'
      AND content->'m.relates_to'->>'rel_type' = 'm.annotation'
      AND (content->'m.relates_to'->>'key') IS NOT NULL;
