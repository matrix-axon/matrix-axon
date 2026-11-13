//! Why a stored event could not be decrypted: the SDK-free vocabulary that
//! sync, store and API share, and the bounds on detail strings that come from
//! other users' devices.

/// The class of a decryption failure. Stored as [`as_str`](Self::as_str) in
/// `events.utd_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UtdReasonKind {
    /// The Megolm session is unknown, or known only from a later message
    /// index, and no withheld notice arrived. A key may still arrive.
    MissingKey,
    /// The sender's device told us, with an `m.room_key.withheld` message, that
    /// it will not share the key. [`UtdReason::withheld_code`] says why.
    Withheld,
    /// The key is present, but the sending device does not meet the account's
    /// trust requirement. Only possible once ADR 0136 Phase 1 is on.
    /// [`UtdReason::verification_level`] says what the SDK found.
    SenderNotTrusted,
    /// Any other failure: a malformed envelope, a bad MAC, mismatched identity
    /// keys, an undeserializable payload.
    Other,
}

impl UtdReasonKind {
    /// The stored and wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingKey => "missing_key",
            Self::Withheld => "withheld",
            Self::SenderNotTrusted => "sender_not_trusted",
            Self::Other => "other",
        }
    }

    /// Parse a stored spelling. `None` for anything else, which a reader treats
    /// as "no recorded reason" rather than failing the row.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "missing_key" => Self::MissingKey,
            "withheld" => Self::Withheld,
            "sender_not_trusted" => Self::SenderNotTrusted,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

/// Longest withheld code Axon stores. The spec's codes are at most
/// `m.history_not_shared` (20 bytes); a custom code from another user's device
/// longer than this is dropped, not truncated.
pub const MAX_WITHHELD_CODE_BYTES: usize = 64;

/// Why one stored event has no content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtdReason {
    /// The failure class.
    pub kind: UtdReasonKind,
    /// For [`UtdReasonKind::Withheld`], the code the sender's device gave, such
    /// as `m.unverified`. `None` for other kinds, and for a custom code that
    /// fails [`sanitize_withheld_code`].
    pub withheld_code: Option<String>,
    /// For [`UtdReasonKind::SenderNotTrusted`], the SDK's verification level of
    /// the sender, as one of [`VERIFICATION_LEVELS`]. `None` for other kinds.
    pub verification_level: Option<String>,
}

/// The spellings of matrix-sdk's `VerificationLevel` that Axon stores for a
/// [`UtdReasonKind::SenderNotTrusted`] reason.
pub const VERIFICATION_LEVELS: [&str; 6] = [
    "unverified_identity",
    "verification_violation",
    "unsigned_device",
    "missing_device",
    "insecure_source",
    "mismatched_sender",
];

impl UtdReason {
    /// A reason with no detail.
    pub const fn new(kind: UtdReasonKind) -> Self {
        Self {
            kind,
            withheld_code: None,
            verification_level: None,
        }
    }

    /// A withheld reason, keeping `code` only if [`sanitize_withheld_code`]
    /// accepts it.
    pub fn withheld(code: &str) -> Self {
        Self {
            kind: UtdReasonKind::Withheld,
            withheld_code: sanitize_withheld_code(code).map(str::to_owned),
            verification_level: None,
        }
    }

    /// A trust refusal at `level`, which must be one of
    /// [`VERIFICATION_LEVELS`]; any other value is dropped.
    pub fn sender_not_trusted(level: &str) -> Self {
        Self {
            kind: UtdReasonKind::SenderNotTrusted,
            withheld_code: None,
            verification_level: VERIFICATION_LEVELS
                .contains(&level)
                .then(|| level.to_owned()),
        }
    }

    /// The one detail column's value: the withheld code or the verification
    /// level, whichever this kind carries.
    pub fn detail(&self) -> Option<&str> {
        match self.kind {
            UtdReasonKind::Withheld => self.withheld_code.as_deref(),
            UtdReasonKind::SenderNotTrusted => self.verification_level.as_deref(),
            UtdReasonKind::MissingKey | UtdReasonKind::Other => None,
        }
    }

    /// Rebuild a reason from its two stored columns. `None` when `kind` is
    /// absent or unknown. A detail that no longer passes validation is dropped.
    pub fn from_columns(kind: Option<&str>, detail: Option<&str>) -> Option<Self> {
        let kind = UtdReasonKind::parse(kind?)?;
        Some(match (kind, detail) {
            (UtdReasonKind::Withheld, Some(code)) => Self::withheld(code),
            (UtdReasonKind::SenderNotTrusted, Some(level)) => Self::sender_not_trusted(level),
            _ => Self::new(kind),
        })
    }
}

/// `code` if it is safe to store and show: non-empty, at most
/// [`MAX_WITHHELD_CODE_BYTES`], and printable ASCII without spaces. A withheld
/// code comes from another user's device, so anything else is dropped.
pub fn sanitize_withheld_code(code: &str) -> Option<&str> {
    (!code.is_empty()
        && code.len() <= MAX_WITHHELD_CODE_BYTES
        && code.bytes().all(|b| b.is_ascii_graphic()))
    .then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_their_spelling() {
        for kind in [
            UtdReasonKind::MissingKey,
            UtdReasonKind::Withheld,
            UtdReasonKind::SenderNotTrusted,
            UtdReasonKind::Other,
        ] {
            assert_eq!(UtdReasonKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(UtdReasonKind::parse("bogus"), None);
    }

    #[test]
    fn withheld_codes_are_bounded_and_printable() {
        assert_eq!(
            UtdReason::withheld("m.unverified").withheld_code.as_deref(),
            Some("m.unverified")
        );
        assert_eq!(
            UtdReason::withheld("com.example.custom")
                .withheld_code
                .as_deref(),
            Some("com.example.custom")
        );
        let long = "x".repeat(MAX_WITHHELD_CODE_BYTES + 1);
        for bad in ["", "has space", "new\nline", "é", long.as_str()] {
            let reason = UtdReason::withheld(bad);
            assert_eq!(reason.kind, UtdReasonKind::Withheld);
            assert_eq!(reason.withheld_code, None, "{bad:?} must be dropped");
        }
    }

    #[test]
    fn unknown_verification_levels_are_dropped() {
        assert_eq!(
            UtdReason::sender_not_trusted("unsigned_device")
                .verification_level
                .as_deref(),
            Some("unsigned_device")
        );
        assert_eq!(
            UtdReason::sender_not_trusted("made_up").verification_level,
            None
        );
    }

    #[test]
    fn columns_round_trip() {
        for reason in [
            UtdReason::new(UtdReasonKind::MissingKey),
            UtdReason::new(UtdReasonKind::Other),
            UtdReason::withheld("m.unverified"),
            UtdReason::sender_not_trusted("mismatched_sender"),
        ] {
            assert_eq!(
                UtdReason::from_columns(Some(reason.kind.as_str()), reason.detail()),
                Some(reason)
            );
        }
        assert_eq!(UtdReason::from_columns(None, Some("m.unverified")), None);
        assert_eq!(UtdReason::from_columns(Some("bogus"), None), None);
        // A detail on a kind that carries none is ignored.
        assert_eq!(
            UtdReason::from_columns(Some("missing_key"), Some("m.unverified")),
            Some(UtdReason::new(UtdReasonKind::MissingKey))
        );
    }
}
