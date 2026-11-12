//! macOS and iOS: `DNSServiceGetAddrInfo`, exactly as the Swift reader calls
//! it (`PreviewNetwork.resolve`). `kDNSServiceProtocol_IPv4` makes
//! mDNSResponder issue A queries only, and `kDNSServiceProtocol_IPv6` AAAA
//! only. `kDNSServiceFlagsReturnIntermediates` reports a negative answer at
//! once instead of letting the budget run out.
//!
//! Answers are delivered on a serial dispatch queue
//! (`DNSServiceSetDispatchQueue`), as the Swift reader receives them. The
//! first version polled `DNSServiceRefSockFD` on a blocking thread instead,
//! which works on macOS and never delivered on iOS 27: mDNSResponder logged
//! both answers within 2 ms, the callback never ran, and every lookup spent
//! its DNS budget and failed as a timeout. The reference is deallocated on
//! its queue, so no callback runs after it, and the callback's state is freed
//! only then.

use super::{Answer, Dns, RecordType};
use crate::error::{Failure, Result};
use crate::Cancel;
use std::ffi::{c_char, c_int, c_void, CString};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tokio::sync::oneshot;
use tokio::time::Instant;

type DnsServiceRef = *mut c_void;
type DispatchQueue = *mut c_void;
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

// In libSystem on both platforms (`<dns_sd.h>`, `<dispatch/dispatch.h>`), so
// nothing extra is linked.
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
    fn DNSServiceSetDispatchQueue(sd_ref: DnsServiceRef, queue: DispatchQueue) -> i32;
    fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
    fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> DispatchQueue;
    fn dispatch_sync_f(
        queue: DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_release(object: *mut c_void);
}

const FLAGS_ADD: u32 = 0x2;
const FLAGS_RETURN_INTERMEDIATES: u32 = 0x1000;
const PROTOCOL_IPV4: u32 = 0x01;
const PROTOCOL_IPV6: u32 = 0x02;
const ERR_NO_SUCH_RECORD: i32 = -65554;

pub(crate) struct AppleDns;

/// What the callback reports to. Touched only by the callback, on the queue,
/// from `DNSServiceSetDispatchQueue` until `Lookup::drop` deallocates the
/// reference on that same queue.
struct State {
    family: RecordType,
    sender: Option<oneshot::Sender<Result<Answer>>>,
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
    // SAFETY: `context` is the `State` the `Lookup` owns, which outlives the
    // reference, and only this callback touches it while the reference lives.
    let state = unsafe { &mut *context.cast::<State>() };
    // The first answer decides, as the Swift reader's callback does.
    if let Some(sender) = state.sender.take() {
        let _ = sender.send(answer(state.family, flags, error_code, address));
    }
}

fn answer(
    family: RecordType,
    flags: u32,
    error_code: i32,
    address: *const libc::sockaddr,
) -> Result<Answer> {
    if error_code == ERR_NO_SUCH_RECORD {
        return Ok(Answer::NoRecord);
    }
    if error_code != 0 || flags & FLAGS_ADD == 0 || address.is_null() {
        return Err(Failure::Unavailable);
    }
    // SAFETY: mDNSResponder passes a valid sockaddr of the family it names.
    let ip = unsafe {
        match c_int::from((*address).sa_family) {
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
    match (ip, family) {
        (Some(ip @ IpAddr::V4(_)), RecordType::A)
        | (Some(ip @ IpAddr::V6(_)), RecordType::Aaaa) => Ok(Answer::Address(ip)),
        _ => Err(Failure::PreviewUnavailable),
    }
}

/// One query's queue, reference and callback state, released in the order
/// that keeps the callback sound however the lookup ends: answered, expired,
/// cancelled, or its future dropped mid-wait.
struct Lookup {
    queue: DispatchQueue,
    /// Null until the query starts, and after it is released.
    reference: DnsServiceRef,
    state: *mut State,
}

// SAFETY: the pointers are only handed to libdispatch and dns_sd, which take
// them from any thread for these calls; `state` is freed in `drop`, after the
// reference that could reach it is gone.
unsafe impl Send for Lookup {}

extern "C" fn deallocate(reference: *mut c_void) {
    // SAFETY: a live reference, deallocated once, on its queue.
    unsafe { DNSServiceRefDeallocate(reference) };
}

impl Drop for Lookup {
    fn drop(&mut self) {
        // SAFETY: each pointer was created in `start` and is released once.
        // Deallocating on the serial queue waits out a callback in progress,
        // and none runs after it, so `state` is unused from here on.
        unsafe {
            if !self.reference.is_null() {
                dispatch_sync_f(self.queue, self.reference, deallocate);
            }
            dispatch_release(self.queue);
            drop(Box::from_raw(self.state));
        }
    }
}

fn start(name: &str, record: RecordType) -> Result<(Lookup, oneshot::Receiver<Result<Answer>>)> {
    let host = CString::new(format!("{name}.")).map_err(|_| Failure::PreviewUnavailable)?;
    let protocol = match record {
        RecordType::A => PROTOCOL_IPV4,
        RecordType::Aaaa => PROTOCOL_IPV6,
    };
    // SAFETY: a serial queue (null attributes) with a static label.
    let queue = unsafe {
        dispatch_queue_create(
            c"org.matrixaxon.axon.link-preview-dns".as_ptr(),
            std::ptr::null_mut(),
        )
    };
    if queue.is_null() {
        return Err(Failure::Unavailable);
    }
    let (sender, receiver) = oneshot::channel();
    let mut lookup = Lookup {
        queue,
        reference: std::ptr::null_mut(),
        state: Box::into_raw(Box::new(State {
            family: record,
            sender: Some(sender),
        })),
    };
    let mut reference: DnsServiceRef = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call. No callback can run until
    // the reference is scheduled on the queue below.
    let status = unsafe {
        DNSServiceGetAddrInfo(
            &mut reference,
            FLAGS_RETURN_INTERMEDIATES,
            0,
            protocol,
            host.as_ptr(),
            reply,
            lookup.state.cast(),
        )
    };
    if status != 0 || reference.is_null() {
        return Err(Failure::Unavailable);
    }
    // SAFETY: a live reference and queue.
    if unsafe { DNSServiceSetDispatchQueue(reference, queue) } != 0 {
        // Never scheduled, so no callback can run: it is released here, off
        // any queue.
        // SAFETY: a live reference, deallocated once.
        unsafe { DNSServiceRefDeallocate(reference) };
        return Err(Failure::Unavailable);
    }
    lookup.reference = reference;
    Ok((lookup, receiver))
}

impl Dns for AppleDns {
    async fn query(
        &self,
        name: &str,
        record: RecordType,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Answer> {
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        let (lookup, receiver) = start(name, record)?;
        let result = tokio::select! {
            answer = receiver => answer.unwrap_or(Err(Failure::Unavailable)),
            () = tokio::time::sleep_until(deadline) => Err(Failure::Expired),
            () = cancel.cancelled() => Err(Failure::Cancelled),
        };
        // Released before returning, not at some later drop, so the
        // reference ends with the query (mDNSResponder logs it as `STOP`).
        drop(lookup);
        result
    }
}

#[cfg(test)]
mod tests {
    //! The real resolver, on the paths where the release order matters. A
    //! lookup that ends before its answer arrives must deallocate on the
    //! queue and free the callback's state without a late callback touching
    //! it; a use-after-free here would crash the test process.
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn an_expired_deadline_releases_the_query() {
        for _ in 0..50 {
            let result = AppleDns
                .query("example.com", RecordType::A, Instant::now(), &Cancel::new())
                .await;
            assert!(
                matches!(result, Err(Failure::Expired) | Ok(_)),
                "{result:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_cancelled_query_is_released_at_once() {
        let cancel = Cancel::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let result = AppleDns
            .query(
                "unanswered-axon-preview-test.invalid",
                RecordType::A,
                Instant::now() + Duration::from_secs(10),
                &cancel,
            )
            .await;
        assert!(
            matches!(result, Err(Failure::Cancelled) | Ok(Answer::NoRecord)),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_dropped_query_releases_its_reference() {
        let cancel = Cancel::new();
        for _ in 0..50 {
            let query = AppleDns.query(
                "example.com",
                RecordType::Aaaa,
                Instant::now() + Duration::from_secs(10),
                &cancel,
            );
            // Dropped part-way: started, and abandoned before any answer.
            let _ = tokio::time::timeout(Duration::from_micros(1), query).await;
        }
    }
}
