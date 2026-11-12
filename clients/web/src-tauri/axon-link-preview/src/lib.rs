//! Link previews built on this device, for the composer (ADR 0132 § 3).
//!
//! One restricted reader for the shell on macOS, Windows, Linux, iOS and
//! Android. It enforces the boundary of the Share Extension's Swift reader
//! (`incoming-share/Sources/AxonIncomingShare/PreviewNetwork.swift`) rule for
//! rule; `src/incoming-share/fixtures/preview-boundary.json` holds the cases
//! both test suites must agree on.
//!
//! - **Hosts:** HTTPS, a public-looking DNS name ([`url_check`]), checked
//!   again for every redirect and image URL.
//! - **DNS:** the origin's A records only, and NAT64 discovery through
//!   `ipv4only.arpa` ([`dns`]). An origin's AAAA records are never asked for.
//! - **Connection:** only to a validated address, TLS 1.2 or later verified
//!   against the original name, no proxy ([`net`]).
//! - **Responses:** status 200 only; 32 KiB of headers, a page until its head
//!   ends or 256 KiB, an image up to 2 MiB ([`http`]).
//! - **Time and concurrency:** one 25-second deadline per lookup, cancellable
//!   at any point, and at most two lookups running across the app.
//!
//! The page calls two commands. [`lookup`] takes an id the page chose and a
//! URL, and resolves with a frame: a big-endian `u32` length, that many bytes
//! of JSON metadata, then the thumbnail's bytes, if any. One round trip, and
//! no base64 for up to 2 MiB of image. [`cancel`] stops the lookup with that
//! id. A rejection is `{ "kind": ... }` with no free text ([`error::Kind`]).

mod address;
mod dns;
mod error;
mod http;
mod image;
mod log;
mod net;
mod page;
mod preview;
mod url_check;

#[cfg(test)]
mod tests;

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::ipc::Response;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::webview::PageLoadEvent;
use tauri::{Manager, RunEvent, Runtime, State, Webview, WindowEvent};
use tokio::sync::Semaphore;

pub use error::{Kind, LookupError};

/// Cancels one lookup: the page's abort, a reload, or a closed window.
pub(crate) type Cancel = tokio_util::sync::CancellationToken;

/// At most two lookups run at once across the app; the rest wait their turn.
const RUNNING: usize = 2;
/// More lookups than this registered at once is refused as busy. The page
/// asks for at most three cards per composer.
const REGISTERED: usize = 16;
/// How many cancellations of not-yet-registered lookups are remembered.
const EARLY_CANCELS: usize = 16;

type Key = (String, u64);

/// One command's claim on a key. A reloaded page numbers its lookups from 1
/// again, so a key can be registered anew while the old page's command is
/// still unwinding; the generation tells the two registrations apart.
struct Registration {
    generation: u64,
    cancel: Cancel,
}

/// The registrations, and the keys cancelled before their lookup registered,
/// under one lock so the two cannot disagree.
#[derive(Default)]
struct Book {
    registered: HashMap<Key, Registration>,
    /// `cancel` is a plain command and `lookup` an async one, so a page's
    /// cancel can be handled before the lookup it cancels has registered.
    /// Remembered here, oldest first and only a few, until it does.
    early_cancels: std::collections::VecDeque<Key>,
}

struct Lookups {
    network: Arc<net::Network<dns::System, net::Direct>>,
    running: Arc<Semaphore>,
    normalizer: preview::Normalizer,
    book: Mutex<Book>,
    generations: std::sync::atomic::AtomicU64,
}

impl Lookups {
    fn new() -> Self {
        Lookups {
            network: Arc::new(net::Network {
                dns: dns::system(),
                dial: net::Direct,
                tls: net::tls_config(net::mozilla_roots()),
                race_delay: net::RACE_DELAY,
            }),
            running: Arc::new(Semaphore::new(RUNNING)),
            normalizer: preview::Normalizer::new(),
            book: Mutex::new(Book::default()),
            generations: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn book(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The registration's generation and its cancellation, or `Cancelled`
    /// when the page already cancelled this id.
    fn register(&self, key: Key) -> Result<(u64, Cancel), LookupError> {
        let mut book = self.book();
        if let Some(index) = book.early_cancels.iter().position(|early| *early == key) {
            book.early_cancels.remove(index);
            return Err(Kind::Cancelled.into());
        }
        let registered = &mut book.registered;
        if registered.len() >= REGISTERED || registered.contains_key(&key) {
            return Err(Kind::Busy.into());
        }
        let generation = self
            .generations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cancel = Cancel::new();
        registered.insert(
            key,
            Registration {
                generation,
                cancel: cancel.clone(),
            },
        );
        Ok((generation, cancel))
    }

    /// Forget `key` only if it is still this command's registration, so an
    /// old command finishing cannot unregister a new one under the same key.
    fn finish(&self, key: &Key, generation: u64) {
        let registered = &mut self.book().registered;
        if registered
            .get(key)
            .is_some_and(|entry| entry.generation == generation)
        {
            registered.remove(key);
        }
    }

    fn cancel(&self, key: &Key) {
        let mut book = self.book();
        if let Some(entry) = book.registered.remove(key) {
            entry.cancel.cancel();
            return;
        }
        // Either the lookup has not registered yet, or it already finished.
        // A finished one is remembered too, which costs a slot: enough of
        // them between an early cancel and its registration would evict it,
        // and that lookup would run on, holding a running slot until its
        // deadline. The page's binding makes them rare: it never cancels a
        // lookup it has seen settle, so one is recorded only when a cancel
        // crosses a result still on its way back. A page never reuses an
        // id, and a reload forgets its window's ids.
        if book.early_cancels.len() == EARLY_CANCELS {
            book.early_cancels.pop_front();
        }
        book.early_cancels.push_back(key.clone());
    }

    /// One lookup, from registration to cleanup: what the `lookup` command
    /// does, apart from the webview it takes its key from.
    async fn run<H: net::Hop>(
        &self,
        hop: &H,
        key: Key,
        url: &str,
    ) -> Result<preview::Preview, LookupError> {
        // One deadline from here: waiting for a slot spends the same 25
        // seconds the lookup has, so the page waits 25 seconds at most.
        let deadline = tokio::time::Instant::now() + preview::DEADLINE;
        let (generation, cancel) = self.register(key.clone())?;
        let result = async {
            let _slot = tokio::select! {
                permit = tokio::time::timeout_at(deadline, self.running.clone().acquire_owned()) => match permit {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => return Err(LookupError::from(Kind::Unavailable)),
                    Err(_) => return Err(Kind::Timeout.into()),
                },
                () = cancel.cancelled() => return Err(Kind::Cancelled.into()),
            };
            preview::lookup(hop, url, deadline, &cancel, &self.normalizer).await
        }
        .await;
        self.finish(&key, generation);
        if let Err(error) = &result {
            log::event(&format!("device_preview_failed kind={:?}", error.kind));
        }
        result
    }

    /// A reload or a closed window leaves no page to receive what the old one
    /// asked for.
    fn forget_webview(&self, label: &str) {
        let mut book = self.book();
        book.registered.retain(|(owner, _), entry| {
            if owner == label {
                entry.cancel.cancel();
            }
            owner != label
        });
        book.early_cancels.retain(|(owner, _)| owner != label);
    }
}

/// The JSON half of a frame.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Head<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    site_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<ImageHead>,
    image_unavailable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageHead {
    mime_type: &'static str,
    width: u32,
    height: u32,
    byte_length: usize,
}

fn frame(preview: &preview::Preview) -> Vec<u8> {
    let head = Head {
        url: &preview.url,
        title: preview.title.as_deref(),
        description: preview.description.as_deref(),
        site_name: preview.site_name.as_deref(),
        image: preview.image.as_ref().map(|image| ImageHead {
            mime_type: image.mime,
            width: image.width,
            height: image.height,
            byte_length: image.bytes.len(),
        }),
        image_unavailable: preview.image_unavailable,
    };
    let json = serde_json::to_vec(&head).expect("the head serializes");
    let image = preview.image.as_ref().map_or(&[][..], |image| &image.bytes);
    let mut framed = Vec::with_capacity(4 + json.len() + image.len());
    framed.extend_from_slice(&(json.len() as u32).to_be_bytes());
    framed.extend_from_slice(&json);
    framed.extend_from_slice(image);
    framed
}

#[tauri::command]
async fn lookup<R: Runtime>(
    webview: Webview<R>,
    lookups: State<'_, Lookups>,
    id: u64,
    url: String,
) -> Result<Response, LookupError> {
    let preview = lookups
        .run(
            &*lookups.network.clone(),
            (webview.label().to_owned(), id),
            &url,
        )
        .await?;
    Ok(Response::new(frame(&preview)))
}

#[tauri::command]
fn cancel<R: Runtime>(webview: Webview<R>, lookups: State<'_, Lookups>, id: u64) {
    lookups.cancel(&(webview.label().to_owned(), id));
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("axon-link-preview")
        .invoke_handler(tauri::generate_handler![lookup, cancel])
        .setup(|app, _api| {
            app.manage(Lookups::new());
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                if let Some(lookups) = webview.try_state::<Lookups>() {
                    lookups.forget_webview(webview.label());
                }
            }
        })
        .on_event(|app, event| {
            if let RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } = event
            {
                if let Some(lookups) = app.try_state::<Lookups>() {
                    lookups.forget_webview(label);
                }
            }
        })
        .build()
}
