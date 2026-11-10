//! macOS and iOS: `DNSServiceGetAddrInfo`, exactly as the Swift reader calls
//! it (`PreviewNetwork.resolve`). `kDNSServiceProtocol_IPv4` makes
//! mDNSResponder issue A queries only, and `kDNSServiceProtocol_IPv6` AAAA
//! only. `kDNSServiceFlagsReturnIntermediates` reports a negative answer at
//! once instead of letting the budget run out.
//!
//! The lookup's socket is polled on a blocking thread in 100 ms slices, so a
//! cancellation or the deadline releases it promptly. The reference is
//! deallocated on that thread, after the last callback can run.

use super::{Answer, Dns, RecordType};
use crate::error::{Failure, Result};
use crate::Cancel;
use std::ffi::{c_char, c_int, c_void, CString};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tokio::time::Instant;

type DnsServiceRef = *mut c_void;
type DnsServiceGetAddrInfoReply = extern "C" fn(
    sd_ref: DnsServiceRef,
    flags: u32,
    interface_index: u32,
    error_code: i32,
    hostname: *const c_char,
    address: *const libc::sockaddr,
    ttl: u32,
    context: *mut c_void,
);

// In libSystem on both platforms (`<dns_sd.h>`), so nothing extra is linked.
extern "C" {
    fn DNSServiceGetAddrInfo(
        sd_ref: *mut DnsServiceRef,
        flags: u32,
        interface_index: u32,
        protocol: u32,
        hostname: *const c_char,
        callback: DnsServiceGetAddrInfoReply,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(sd_ref: DnsServiceRef) -> c_int;
    fn DNSServiceProcessResult(sd_ref: DnsServiceRef) -> i32;
    fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
}

const FLAGS_ADD: u32 = 0x2;
const FLAGS_RETURN_INTERMEDIATES: u32 = 0x1000;
const PROTOCOL_IPV4: u32 = 0x01;
const PROTOCOL_IPV6: u32 = 0x02;
const ERR_NO_SUCH_RECORD: i32 = -65554;

pub(crate) struct AppleDns;

/// What the callback found. Written only by the callback, which runs inside
/// `DNSServiceProcessResult` on the thread that owns this.
struct State {
    family: RecordType,
    result: Option<Result<Answer>>,
}

extern "C" fn reply(
    _sd_ref: DnsServiceRef,
    flags: u32,
    _interface_index: u32,
    error_code: i32,
    _hostname: *const c_char,
    address: *const libc::sockaddr,
    _ttl: u32,
    context: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the `State` `lookup` passed, which outlives the
    // reference, and this runs on the thread that owns it.
    let state = unsafe { &mut *context.cast::<State>() };
    if state.result.is_some() {
        return;
    }
    if error_code == ERR_NO_SUCH_RECORD {
        state.result = Some(Ok(Answer::NoRecord));
        return;
    }
    if error_code != 0 || flags & FLAGS_ADD == 0 || address.is_null() {
        state.result = Some(Err(Failure::Unavailable));
        return;
    }
    // SAFETY: mDNSResponder passes a valid sockaddr of the family it names.
    let ip = unsafe {
        match i32::from((*address).sa_family) {
            libc::AF_INET => {
                let v4 = &*address.cast::<libc::sockaddr_in>();
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
            }
            libc::AF_INET6 => {
                let v6 = &*address.cast::<libc::sockaddr_in6>();
                Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    };
    let expected = match (ip, state.family) {
        (Some(ip @ IpAddr::V4(_)), RecordType::A)
        | (Some(ip @ IpAddr::V6(_)), RecordType::Aaaa) => Some(ip),
        _ => None,
    };
    state.result = Some(
        expected
            .map(Answer::Address)
            .ok_or(Failure::PreviewUnavailable),
    );
}

fn lookup(name: &str, record: RecordType, deadline: Instant, cancel: &Cancel) -> Result<Answer> {
    let host = CString::new(format!("{name}.")).map_err(|_| Failure::PreviewUnavailable)?;
    let protocol = match record {
        RecordType::A => PROTOCOL_IPV4,
        RecordType::Aaaa => PROTOCOL_IPV6,
    };
    let mut state = Box::new(State {
        family: record,
        result: None,
    });
    let mut reference: DnsServiceRef = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call; `state` outlives
    // `reference`, which is deallocated below on this thread.
    let status = unsafe {
        DNSServiceGetAddrInfo(
            &mut reference,
            FLAGS_RETURN_INTERMEDIATES,
            0,
            protocol,
            host.as_ptr(),
            reply,
            (&mut *state as *mut State).cast(),
        )
    };
    if status != 0 || reference.is_null() {
        return Err(Failure::Unavailable);
    }
    let result = poll(reference, &mut state, deadline, cancel);
    // SAFETY: allocated above and not used again.
    unsafe { DNSServiceRefDeallocate(reference) };
    result
}

fn poll(
    reference: DnsServiceRef,
    state: &mut State,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Answer> {
    // SAFETY: `reference` is live.
    let fd = unsafe { DNSServiceRefSockFD(reference) };
    if fd < 0 {
        return Err(Failure::Unavailable);
    }
    loop {
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Failure::Expired);
        }
        let slice = (deadline - now).as_millis().min(100) as c_int;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pollfd, 1, slice) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Failure::Unavailable);
        }
        if ready == 0 {
            continue;
        }
        // SAFETY: `reference` is live; the callback writes into `state`.
        if unsafe { DNSServiceProcessResult(reference) } != 0 {
            return Err(Failure::Unavailable);
        }
        if let Some(result) = state.result.take() {
            return result;
        }
    }
}

impl Dns for AppleDns {
    async fn query(
        &self,
        name: &str,
        record: RecordType,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Answer> {
        let name = name.to_owned();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || lookup(&name, record, deadline, &cancel))
            .await
            .unwrap_or(Err(Failure::Unavailable))
    }
}
