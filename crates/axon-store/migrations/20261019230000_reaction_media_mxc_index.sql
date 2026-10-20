-- MSC4027 reaction images use the annotation key as a plaintext MXC URL.
-- Match the media lookup predicate so downloads do not scan account history.
-- Like the other media indexes, this is a transactional startup migration.
-- Sync and HTTP serving start only after migration completion; do not run
-- another writer during the maintenance restart. Failure rolls back the index.
CREATE INDEX IF NOT EXISTS events_content_reaction_key_idx
    ON events (account_id, (content->'m.relates_to'->>'key'), id)
    WHERE event_type = 'm.reaction'
      AND content->'m.relates_to'->>'rel_type' = 'm.annotation'
      AND content->'m.relates_to'->>'key' LIKE 'mxc://%'
      -- Bound sender-controlled keys below PostgreSQL's B-tree tuple limit.
      -- The media lookup uses the same predicate.
      AND octet_length(content->'m.relates_to'->>'key') <= 2048;
