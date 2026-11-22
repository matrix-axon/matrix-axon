-- ADR 0142 section 1 (issue 1207): staged uploads gain the `video` and `audio`
-- kinds, which send as `m.video` and `m.audio` with their declared type.
--
-- 20260710120000_media_uploads.sql created the column's CHECK without a name,
-- so Postgres named it `media_uploads_kind_check`.
ALTER TABLE media_uploads
    DROP CONSTRAINT media_uploads_kind_check,
    ADD CONSTRAINT media_uploads_kind_check
        CHECK (kind IN ('image', 'file', 'video', 'audio'));
