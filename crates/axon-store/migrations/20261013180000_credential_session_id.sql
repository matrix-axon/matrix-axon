-- A credential session id, shared by the rows of one sign-in.
--
-- An OAuth sign-in writes one id on the access token and the refresh token.
-- Refresh rotation copies that id onto the new refresh row, and the access
-- token minted for that rotation uses it too. A later registration can then
-- outlive any single access token.
--
-- A non-expiring credential has no pair to share with, so its session id is
-- its own row id. Rows already in these tables are backfilled the same way.
--
-- The insert trigger is the backstop for a writer that omits the column,
-- including an older binary. PostgreSQL applies `id`'s default before a
-- BEFORE INSERT trigger runs, so NEW.id is the row id either way. A writer
-- that supplies a session id keeps it. NULL is not a session: the trigger
-- fills it rather than leaving the column null.
--
-- Indexed for a lookup by session. There is no foreign key. A session has to
-- outlive any one of its rows, and a cascade from a swept or rotated row
-- would drop that session's other state.

ALTER TABLE tokens ADD COLUMN session_id UUID;
UPDATE tokens SET session_id = id WHERE session_id IS NULL;
ALTER TABLE tokens ALTER COLUMN session_id SET NOT NULL;
CREATE INDEX tokens_session_id_idx ON tokens (session_id);

ALTER TABLE oauth_refresh_tokens ADD COLUMN session_id UUID;
UPDATE oauth_refresh_tokens SET session_id = id WHERE session_id IS NULL;
ALTER TABLE oauth_refresh_tokens ALTER COLUMN session_id SET NOT NULL;
CREATE INDEX oauth_refresh_tokens_session_id_idx ON oauth_refresh_tokens (session_id);

CREATE OR REPLACE FUNCTION credential_session_id()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.session_id := COALESCE(NEW.session_id, NEW.id);
    RETURN NEW;
END;
$$;

CREATE TRIGGER tokens_set_session_id
    BEFORE INSERT ON tokens
    FOR EACH ROW EXECUTE FUNCTION credential_session_id();

CREATE TRIGGER oauth_refresh_tokens_set_session_id
    BEFORE INSERT ON oauth_refresh_tokens
    FOR EACH ROW EXECUTE FUNCTION credential_session_id();
