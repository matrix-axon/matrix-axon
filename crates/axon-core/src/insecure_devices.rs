//! Account-scoped MSC4153 impact shared by the sync engine and API.

/// An encrypted room with known devices that are not cross-signed.
#[derive(Debug, PartialEq, Eq)]
pub struct InsecureDeviceRoom {
    pub room_id: String,
    pub member_user_ids: Vec<String>,
}

/// The affected rooms and an acknowledgement bound to this account and list.
#[derive(Debug, PartialEq, Eq)]
pub struct InsecureDeviceImpact {
    pub rooms: Vec<InsecureDeviceRoom>,
    pub acknowledgement_token: String,
}
