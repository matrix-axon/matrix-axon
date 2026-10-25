-- Keep generation retries bounded independently of ordinary staged attachments.
-- PostgreSQL applies this constant default without rewriting the table.
ALTER TABLE media_uploads ADD COLUMN generated_preview BOOLEAN NOT NULL DEFAULT false;
CREATE INDEX media_uploads_generated_account_idx ON media_uploads (account_id)
    WHERE generated_preview;
