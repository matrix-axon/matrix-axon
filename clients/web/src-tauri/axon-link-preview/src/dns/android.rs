//! Android: `android_res_nquery` (API 29), a typed query through netd, which
//! applies the network's resolver and Private DNS. netd builds the message
//! from the name and type given here and returns the raw answer, which
//! [`message::parse`] reads.
//!
//! The shell's `minSdk` is 24, so the symbols are looked up at runtime. On
//! Android 7 to 9 they are absent and every lookup fails as unavailable:
//! there is no typed query to use, and a hinted `getaddrinfo` is not one.

use super::message::{self, Problem};
use super::{Answer, Dns, RecordType};
use crate::error::{Failure, Result};
use crate::Cancel;
use std::ffi::{c_char, c_int, c_void, CString};
use tokio::time::Instant;

/// `NETWORK_UNSPECIFIED`: the app's default network.
const NETWORK_UNSPECIFIED: u64 = 0;
const NS_C_IN: c_int = 1;
const ANSWER_BYTES: usize = 8 * 1024;

type Query = unsafe extern "C" fn(u64, *const c_char, c_int, c_int, u32) -> c_int;
type Fetch = unsafe extern "C" fn(c_int, *mut c_int, *mut u8, usize) -> c_int;
type CancelQuery = unsafe extern "C" fn(c_int);

#[derive(Clone, Copy)]
struct Symbols {
    query: Query,
    result: Fetch,
    cancel: CancelQuery,
}

pub(crate) struct AndroidDns {
    symbols: Option<Symbols>,
}

impl AndroidDns {
    pub(crate) fn load() -> Self {
        AndroidDns { symbols: symbols() }
    }
}

fn symbols() -> Option<Symbols> {
    // SAFETY: plain dlopen/dlsym of a system library; the handle is never
    // closed, so the function pointers stay valid.
    unsafe {
        let library = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
        if library.is_null() {
            return None;
        }
        let find = |name: &std::ffi::CStr| {
            let symbol = libc::dlsym(library, name.as_ptr());
            (!symbol.is_null()).then_some(symbol)
        };
        let query: *mut c_void = find(c"android_res_nquery")?;
        let result: *mut c_void = find(c"android_res_nresult")?;
        let cancel: *mut c_void = find(c"android_res_cancel")?;
        Some(Symbols {
            query: std::mem::transmute::<*mut c_void, Query>(query),
            result: std::mem::transmute::<*mut c_void, Fetch>(result),
            cancel: std::mem::transmute::<*mut c_void, CancelQuery>(cancel),
        })
    }
}

fn lookup(
    symbols: Symbols,
    name: &str,
    record: RecordType,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Answer> {
    let host = CString::new(name).map_err(|_| Failure::PreviewUnavailable)?;
    // SAFETY: a valid C string; the returned descriptor is owned below.
    let fd = unsafe {
        (symbols.query)(
            NETWORK_UNSPECIFIED,
            host.as_ptr(),
            NS_C_IN,
            c_int::from(record.code()),
            0,
        )
    };
    if fd < 0 {
        return Err(Failure::Unavailable);
    }
    loop {
        let stop = if cancel.is_cancelled() {
            Some(Failure::Cancelled)
        } else if Instant::now() >= deadline {
            Some(Failure::Expired)
        } else {
            None
        };
        if let Some(failure) = stop {
            // SAFETY: closes the descriptor the query returned.
            unsafe { (symbols.cancel)(fd) };
            return Err(failure);
        }
        let slice = (deadline - Instant::now()).as_millis().min(100) as c_int;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pollfd, 1, slice) };
        if ready == 0
            || (ready < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted)
        {
            continue;
        }
        let mut rcode: c_int = 0;
        let mut answer = vec![0u8; ANSWER_BYTES];
        // SAFETY: closes `fd` and writes at most `answer.len()` bytes.
        let length = unsafe { (symbols.result)(fd, &mut rcode, answer.as_mut_ptr(), answer.len()) };
        let Ok(length) = usize::try_from(length) else {
            return Err(Failure::Unavailable);
        };
        return match message::parse(&answer[..length.min(answer.len())], None, name, record) {
            Ok(answer) => Ok(answer),
            Err(
                Problem::Mismatch
                | Problem::Truncated
                | Problem::ServerFailure
                | Problem::Malformed,
            ) => Err(Failure::Unavailable),
        };
    }
}

impl Dns for AndroidDns {
    async fn query(
        &self,
        name: &str,
        record: RecordType,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Answer> {
        let Some(symbols) = self.symbols else {
            return Err(Failure::Unavailable);
        };
        let name = name.to_owned();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || lookup(symbols, &name, record, deadline, &cancel))
            .await
            .unwrap_or(Err(Failure::Unavailable))
    }
}
