//! Windows: `DnsQuery_W`, a typed query through the DNS Client service, which
//! applies the system's DNS-over-HTTPS settings and the NRPT.
//!
//! Declared here rather than through the `windows` crate: two functions and
//! one record layout, from `windns.h`, are smaller to review than a feature of
//! a generated binding. The call blocks, so it runs on a blocking thread, and
//! cannot be cancelled: a cancelled or expired lookup stops waiting for it,
//! and the thread ends when the DNS Client answers. [`IN_FLIGHT`] bounds how
//! many threads such calls can hold. `DnsQueryEx` with `DnsCancelQuery` would
//! end them early instead; that is issue 1058.

use super::{Answer, Dns, RecordType};
use crate::error::{Failure, Result};
use crate::Cancel;
use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr::NonNull;
use tokio::time::Instant;

/// `DNS_RECORDW`, up to the first 16 bytes of its data union, which is as far
/// as an AAAA record reaches. Only ever read through a pointer, field by
/// field; see [`first_address`]. The union holds pointers, so it starts at
/// an 8-byte boundary, which is where `data` falls. Every field is declared
/// so the layout matches, though not every one is read.
#[repr(C)]
#[allow(dead_code)]
struct DnsRecord {
    next: *mut DnsRecord,
    name: *mut u16,
    kind: u16,
    data_length: u16,
    flags: u32,
    ttl: u32,
    reserved: u32,
    data: [u8; 16],
}

#[link(name = "dnsapi")]
extern "system" {
    fn DnsQuery_W(
        name: *const u16,
        kind: u16,
        options: u32,
        extra: *mut c_void,
        results: *mut *mut DnsRecord,
        reserved: *mut *mut c_void,
    ) -> i32;
    fn DnsFree(data: *mut c_void, free_type: u32);
}

const DNS_QUERY_STANDARD: u32 = 0;
const DNS_FREE_RECORD_LIST: u32 = 1;
const DNS_ERROR_RCODE_NAME_ERROR: i32 = 9003;
const DNS_INFO_NO_RECORDS: i32 = 9501;

pub(crate) struct WindowsDns;

/// At most this many `DnsQuery_W` calls run at once, abandoned ones included.
/// A lookup asks two questions at once and two lookups run at once, so four
/// is what normal use needs; past it, a query waits for a slot inside its own
/// deadline and fails as expired if none frees up.
const IN_FLIGHT: usize = 4;
static CALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(IN_FLIGHT);

fn lookup(name: &str, record: RecordType) -> Result<Answer> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut results: *mut DnsRecord = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; `results` receives a list freed below.
    let status = unsafe {
        DnsQuery_W(
            wide.as_ptr(),
            record.code(),
            DNS_QUERY_STANDARD,
            std::ptr::null_mut(),
            &mut results,
            std::ptr::null_mut(),
        )
    };
    let list = NonNull::new(results);
    let found = match (status, list) {
        // SAFETY: the head of the list the call returned, not yet freed.
        (0, Some(list)) => unsafe { first_address(list, record) },
        _ => None,
    };
    // Freed whatever the status: a failed or negative answer can still come
    // with records, such as the zone's SOA.
    if let Some(list) = list {
        // SAFETY: the list the call returned, freed once.
        unsafe { DnsFree(list.as_ptr().cast(), DNS_FREE_RECORD_LIST) };
    }
    match status {
        0 => Ok(found.map_or(Answer::NoRecord, Answer::Address)),
        DNS_ERROR_RCODE_NAME_ERROR | DNS_INFO_NO_RECORDS => Ok(Answer::NoRecord),
        _ => Err(Failure::Unavailable),
    }
}

/// The first `record` address in the list starting at `list`.
///
/// Fields are read through the pointer, never through a `&DnsRecord`: Windows
/// allocates a record only as long as its data, so an A record ends twelve
/// bytes before `DnsRecord::data` does, and only `data_length` bytes of the
/// data are read.
///
/// # Safety
///
/// `list` is the head of a list `DnsQuery_W` returned, not yet freed.
unsafe fn first_address(list: NonNull<DnsRecord>, record: RecordType) -> Option<IpAddr> {
    let mut cursor = Some(list);
    while let Some(entry) = cursor {
        let entry = entry.as_ptr();
        // SAFETY: the header fields lie inside every record's allocation.
        let (kind, length, next) = unsafe { ((*entry).kind, (*entry).data_length, (*entry).next) };
        // A CNAME chain comes first; its records are skipped.
        if kind == record.code() {
            // SAFETY: the data starts inside the allocation.
            let data = unsafe { std::ptr::addr_of!((*entry).data) }.cast::<u8>();
            return match (record, length) {
                (RecordType::A, 4..) => {
                    let mut octets = [0u8; 4];
                    // SAFETY: `data_length` says at least four bytes follow.
                    unsafe { std::ptr::copy_nonoverlapping(data, octets.as_mut_ptr(), 4) };
                    Some(IpAddr::V4(Ipv4Addr::from(octets)))
                }
                (RecordType::Aaaa, 16..) => {
                    let mut octets = [0u8; 16];
                    // SAFETY: `data_length` says at least sixteen bytes follow.
                    unsafe { std::ptr::copy_nonoverlapping(data, octets.as_mut_ptr(), 16) };
                    Some(IpAddr::V6(Ipv6Addr::from(octets)))
                }
                _ => None,
            };
        }
        cursor = NonNull::new(next);
    }
    None
}

impl Dns for WindowsDns {
    async fn query(
        &self,
        name: &str,
        record: RecordType,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Answer> {
        let owned = name.to_owned();
        let call = async move {
            let slot = CALLS.acquire().await.map_err(|_| Failure::Unavailable)?;
            // The slot travels with the call, so it is held until the call
            // really returns, not just until this lookup stops waiting.
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                lookup(&owned, record)
            })
            .await
            .unwrap_or(Err(Failure::Unavailable))
        };
        tokio::select! {
            result = call => result,
            () = tokio::time::sleep_until(deadline) => Err(Failure::Expired),
            () = cancel.cancelled() => Err(Failure::Cancelled),
        }
    }
}
