//! Windows: `DnsQuery_W`, a typed query through the DNS Client service, which
//! applies the system's DNS-over-HTTPS settings and the NRPT.
//!
//! Declared here rather than through the `windows` crate: two functions and
//! one record layout, from `windns.h`, are smaller to review than a feature of
//! a generated binding. The call blocks, so it runs on a blocking thread; a
//! cancelled or expired lookup stops waiting for it, and the thread ends when
//! the DNS Client answers.

use super::{Answer, Dns, RecordType};
use crate::error::{Failure, Result};
use crate::Cancel;
use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tokio::time::Instant;

/// `DNS_RECORDW`, up to the first 16 bytes of its data union, which is as far
/// as an A or AAAA record reaches. The union holds pointers, so it starts at
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
    if status == DNS_ERROR_RCODE_NAME_ERROR || status == DNS_INFO_NO_RECORDS {
        return Ok(Answer::NoRecord);
    }
    if status != 0 {
        return Err(Failure::Unavailable);
    }
    let mut found = None;
    let mut cursor = results;
    while !cursor.is_null() {
        // SAFETY: a record in the list the call returned.
        let entry = unsafe { &*cursor };
        // A CNAME chain comes first; its records are skipped.
        if entry.kind == record.code() {
            found = match record {
                RecordType::A if entry.data_length >= 4 => {
                    let b = entry.data;
                    Some(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3])))
                }
                RecordType::Aaaa if entry.data_length >= 16 => {
                    Some(IpAddr::V6(Ipv6Addr::from(entry.data)))
                }
                _ => None,
            };
            break;
        }
        cursor = entry.next;
    }
    if !results.is_null() {
        // SAFETY: frees the list the call returned, once.
        unsafe { DnsFree(results.cast(), DNS_FREE_RECORD_LIST) };
    }
    Ok(found.map_or(Answer::NoRecord, Answer::Address))
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
        let call = tokio::task::spawn_blocking(move || lookup(&owned, record));
        tokio::select! {
            result = call => result.unwrap_or(Err(Failure::Unavailable)),
            () = tokio::time::sleep_until(deadline) => Err(Failure::Expired),
            () = cancel.cancelled() => Err(Failure::Cancelled),
        }
    }
}
