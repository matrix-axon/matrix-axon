//! Shared room and redaction predicates for timeline and media reads.

/// Delivery alone is not proof a redaction applies (room versions 3+).
/// Match the room, then accept SDK-pruned evidence, same-server authority,
/// or cached permission to redact others' events, as state repair does.
/// Aliases are compile-time SQL identifiers, never external input.
fn redaction_with_current_authority(redaction: &str, target: &str) -> String {
    let identity = redaction_identity(redaction, target);
    let immutable = immutable_redaction_authority(redaction, target);
    let sender_level = power_number(&format!("COALESCE(power.content#>ARRAY['users',{redaction}.sender], power.content->'users_default','0'::jsonb)"));
    let redact_level = power_number("COALESCE(power.content->'redact','50'::jsonb)");
    format!(
        "{identity} AND ({immutable} \
             OR EXISTS (SELECT 1 FROM room_state power \
                 WHERE power.account_id = {target}.account_id AND power.room_id = {target}.room_id \
                   AND power.event_type = 'm.room.power_levels' AND power.state_key = '' \
                   AND power.redacted IS DISTINCT FROM true \
                   AND {sender_level} >= {redact_level}) \
             OR EXISTS (SELECT 1 FROM room_state creation \
                 WHERE creation.account_id={target}.account_id AND creation.room_id={target}.room_id \
                   AND creation.event_type='m.room.create' AND creation.state_key='' \
                   AND creation.redacted IS DISTINCT FROM true \
                   AND ((creation.content->>'room_version'='12' AND \
                       (creation.sender={redaction}.sender OR (jsonb_typeof(creation.content->'additional_creators')='array' \
                         AND creation.content->'additional_creators' ? {redaction}.sender))) \
                     OR (COALESCE(creation.content->>'creator',creation.sender)={redaction}.sender \
                       AND NOT EXISTS (SELECT 1 FROM room_state levels \
                           WHERE levels.account_id={target}.account_id AND levels.room_id={target}.room_id \
                             AND levels.event_type='m.room.power_levels' AND levels.state_key='')))))"
    )
}

/// Authority that was proven during persistence stays proven after a
/// moderator is demoted. Rechecking current power on every read would expose
/// redacted content again. Same-server and SDK-pruned evidence are immutable.
pub(crate) fn redaction_matches(redaction: &str, target: &str) -> String {
    let identity = redaction_identity(redaction, target);
    let immutable = immutable_redaction_authority(redaction, target);
    format!(
        "{identity} AND ({immutable} \
           OR EXISTS (SELECT 1 FROM event_redaction_proofs proof \
             WHERE proof.account_id={target}.account_id AND proof.room_id={target}.room_id \
               AND proof.redaction_event_id={redaction}.event_id AND proof.target_event_id={target}.event_id))"
    )
}

fn redaction_identity(redaction: &str, target: &str) -> String {
    format!(
        "{redaction}.account_id={target}.account_id AND {redaction}.room_id={target}.room_id \
        AND {redaction}.event_type='m.room.redaction' AND {redaction}.redacts={target}.event_id"
    )
}

fn immutable_redaction_authority(redaction: &str, target: &str) -> String {
    format!(
        "{target}.raw_event#>>'{{unsigned,redacted_because,event_id}}'={redaction}.event_id \
        OR ({redaction}.sender LIKE '@%:%' AND {target}.sender LIKE '@%:%' \
            AND substring({redaction}.sender FROM position(':' IN {redaction}.sender)+1) \
              = substring({target}.sender FROM position(':' IN {target}.sender)+1))"
    )
}

/// `ins` is the event INSERT/UPDATE's RETURNING relation. Capture evidence for
/// either arrival order in that same statement, so a crash cannot lose it.
pub(crate) fn capture_redaction_proofs() -> String {
    let valid = redaction_with_current_authority("redaction", "target");
    let columns = "redaction.account_id,redaction.room_id,redaction.event_id,target.event_id";
    format!(
        "INSERT INTO event_redaction_proofs (account_id,room_id,redaction_event_id,target_event_id) \
         SELECT {columns} FROM ins redaction CROSS JOIN events target WHERE {valid} \
         UNION ALL \
         SELECT {columns} FROM events redaction CROSS JOIN ins target WHERE {valid} \
         ON CONFLICT DO NOTHING"
    )
}

/// JSON numeric values are already validated by PostgreSQL; bounded integer
/// strings preserve legacy room power levels without allowing arbitrary casts.
fn power_number(value: &str) -> String {
    format!(
        "(CASE WHEN jsonb_typeof({value})='number' THEN ({value}::text)::numeric \
        WHEN jsonb_typeof({value})='string' AND ({value}#>>'{{}}') ~ '^-?[0-9]{{1,16}}$' \
        THEN ({value}#>>'{{}}')::numeric END)"
    )
}

pub(crate) fn event_is_redacted(target: &str) -> String {
    let matches = redaction_matches("redaction", target);
    format!("EXISTS (SELECT 1 FROM events redaction WHERE {matches})")
}

/// Missing membership follows the established get_event_if_joined behavior.
pub(crate) fn media_room_is_visible(target: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM room_state membership \
         JOIN accounts owner ON owner.account_id = membership.account_id \
         WHERE membership.account_id = {target}.account_id AND membership.room_id = {target}.room_id \
           AND membership.event_type = 'm.room.member' AND membership.state_key = owner.user_id \
           AND membership.content->>'membership' IN ('leave', 'ban'))"
    )
}
