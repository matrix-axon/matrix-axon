//! The boundary, held to the fixture the Swift reader's tests read too, and
//! the transport, driven end to end against a local TLS server.

use crate::address::{Address, Nat64Prefix};
use crate::dns::message;
use crate::dns::wire::WireDns;
use crate::dns::{self, Answer, Dns, RecordType, DISCOVERY};
use crate::error::{Failure, Kind};
use crate::http::Kind as Resource;
use crate::net::{self, Dial, Hop, Network, Response};
use crate::url_check::{self, Checked};
use crate::{page, preview, Cancel};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::Instant;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::{self, RootCertStore};

// ---------------------------------------------------------------------------
// Allocation measurement, for the decoder's limits. Only the thread that asks
// is measured, so tests running in parallel do not disturb the reading.

struct Counting;

thread_local! {
    static MEASURING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LARGEST: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn note(size: usize) {
    // `try_with`: the allocator can run while thread-locals are torn down.
    let _ = MEASURING.try_with(|measuring| {
        if measuring.get() {
            let _ = LARGEST.try_with(|largest| largest.set(largest.get().max(size)));
        }
    });
}

// SAFETY: every call is forwarded to the system allocator unchanged.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        note(layout.size());
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        note(layout.size());
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        note(size);
        unsafe { std::alloc::System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// `work`'s result, and the largest single allocation it made on this thread.
pub(crate) fn largest_allocation<T>(work: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.with(|largest| largest.set(0));
    MEASURING.with(|measuring| measuring.set(true));
    let result = work();
    MEASURING.with(|measuring| measuring.set(false));
    (result, LARGEST.with(std::cell::Cell::get))
}

// ---------------------------------------------------------------------------
// The shared fixtures.

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src/incoming-share/fixtures")
}

#[derive(Deserialize)]
struct UrlCase {
    url: String,
    eligible: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Boundary {
    #[serde(rename = "rejectedBeforeDNS")]
    rejected_before_dns: Vec<String>,
    head_ends: Vec<HeadEnd>,
    addresses: Addresses,
    operator_mappings: Vec<String>,
    nat64: Nat64,
    resolution: Vec<Resolution>,
    metadata: Vec<Metadata>,
}

#[derive(Deserialize)]
struct HeadEnd {
    text: String,
    ends: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Addresses {
    public: Vec<String>,
    non_public: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Nat64 {
    translation_target: String,
    vectors: Vec<Vector>,
    malformed_probes: Vec<String>,
    untranslatable: Vec<String>,
}

#[derive(Deserialize)]
struct Vector {
    length: usize,
    probe: String,
    translated: String,
}

#[derive(Deserialize)]
struct Resolution {
    name: String,
    a: Option<String>,
    probe: Option<String>,
    addresses: Option<Vec<String>>,
    failure: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    name: String,
    html: String,
    latin1: Option<bool>,
    content_type: Option<String>,
    title: Option<String>,
    description: Option<String>,
    site_name: Option<String>,
    image: Option<String>,
    failure: Option<String>,
}

fn boundary() -> Boundary {
    let text = std::fs::read_to_string(fixtures().join("preview-boundary.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The Swift name of a failure, as the fixture spells it.
fn swift_name(failure: Failure) -> &'static str {
    match failure {
        Failure::PreviewUnavailable => "previewUnavailable",
        Failure::PublicIpv4Required => "publicIPv4Required",
        Failure::InvalidResponse => "invalidResponse",
        Failure::Quota => "quota",
        Failure::Unavailable => "unavailable",
        Failure::Expired => "expired",
        Failure::Cancelled => "cancelled",
        Failure::Invalid => "invalid",
    }
}

fn address(text: &str) -> Address {
    Address::parse(text).unwrap_or_else(|| panic!("{text} is not an address"))
}

#[test]
fn static_url_eligibility_matches_the_composer_and_the_swift_reader() {
    let text = std::fs::read_to_string(fixtures().join("device-preview-urls.json")).unwrap();
    let cases: Vec<UrlCase> = serde_json::from_str(&text).unwrap();
    for case in cases {
        assert_eq!(
            url_check::validate(&case.url).is_ok(),
            case.eligible,
            "{}",
            case.url
        );
    }
}

#[test]
fn fixture_urls_are_rejected_before_dns() {
    for raw in boundary().rejected_before_dns {
        assert_eq!(
            url_check::validate(&raw).err(),
            Some(Failure::PreviewUnavailable),
            "{raw}"
        );
    }
}

#[test]
fn fixture_head_ends() {
    for case in boundary().head_ends {
        assert_eq!(
            crate::http::ends_head(case.text.as_bytes()),
            case.ends,
            "{:?}",
            case.text
        );
    }
}

#[test]
fn fixture_address_scope() {
    let fixture = boundary().addresses;
    for text in fixture.public {
        assert!(address(&text).is_public(), "{text}");
    }
    for text in fixture.non_public {
        assert!(!address(&text).is_public(), "{text}");
        assert!(!address(&text).approved_for_connection(), "{text}");
    }
}

#[test]
fn fixture_operator_mappings_are_never_approved() {
    for text in boundary().operator_mappings {
        let mapped = address(&text);
        assert!(
            mapped.is_public(),
            "global scope alone is not authorization: {text}"
        );
        assert!(!mapped.approved_for_connection(), "{text}");
    }
}

#[test]
fn fixture_nat64_vectors() {
    let nat64 = boundary().nat64;
    let target = address(&nat64.translation_target);
    for vector in &nat64.vectors {
        let prefix = Nat64Prefix::discover(&address(&vector.probe))
            .unwrap_or_else(|| panic!("{} encodes no prefix", vector.probe));
        assert_eq!(prefix.length(), vector.length, "{}", vector.probe);
        let translated = Address::translated(&target, &prefix).unwrap();
        assert_eq!(
            translated.ip(),
            address(&vector.translated).ip(),
            "{}",
            vector.probe
        );
        assert!(translated.approved_for_connection(), "{}", vector.probe);
        // DNS-supplied copies of the same bits carry no public-A provenance.
        assert!(!address(&vector.translated).approved_for_connection());
        for unsafe_target in &nat64.untranslatable {
            assert!(Address::translated(&address(unsafe_target), &prefix).is_none());
        }
    }
    for probe in nat64.malformed_probes {
        assert!(Nat64Prefix::discover(&address(&probe)).is_none(), "{probe}");
    }
}

/// Answers from a table, and records every question it was asked.
#[derive(Default)]
struct Recording {
    answers: HashMap<(String, RecordType), Result<Answer, Failure>>,
    delays: HashMap<RecordType, Duration>,
    asked: Mutex<Vec<(String, RecordType)>>,
}

impl Recording {
    fn answering(a: Option<&str>, probe: Option<&str>) -> Self {
        let answer = |text: Option<&str>| {
            Ok(text.map_or(Answer::NoRecord, |text| {
                Answer::Address(text.parse::<IpAddr>().unwrap())
            }))
        };
        let mut recording = Recording::default();
        recording
            .answers
            .insert(("page.example.org".into(), RecordType::A), answer(a));
        recording
            .answers
            .insert((DISCOVERY.into(), RecordType::Aaaa), answer(probe));
        recording
    }

    fn asked(&self) -> Vec<(String, RecordType)> {
        self.asked.lock().unwrap().clone()
    }
}

impl Dns for Recording {
    fn query(
        &self,
        name: &str,
        record: RecordType,
        _deadline: Instant,
        _cancel: &Cancel,
    ) -> impl Future<Output = Result<Answer, Failure>> + Send {
        self.asked.lock().unwrap().push((name.to_owned(), record));
        let answer = self
            .answers
            .get(&(name.to_owned(), record))
            .cloned()
            .unwrap_or(Err(Failure::Unavailable));
        let delay = self.delays.get(&record).copied().unwrap_or_default();
        async move {
            tokio::time::sleep(delay).await;
            answer
        }
    }
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(1)
}

#[tokio::test]
async fn fixture_resolution_asks_for_an_a_record_and_discovery_only() {
    for case in boundary().resolution {
        let dns = Recording::answering(case.a.as_deref(), case.probe.as_deref());
        let result = dns::resolve(&dns, "page.example.org", soon(), &Cancel::new()).await;
        match (&case.failure, result) {
            (None, Ok(addresses)) => {
                let expected: Vec<IpAddr> = case
                    .addresses
                    .unwrap()
                    .iter()
                    .map(|text| address(text).ip())
                    .collect();
                let found: Vec<IpAddr> = addresses.iter().map(Address::ip).collect();
                assert_eq!(found, expected, "{}", case.name);
                assert!(addresses.iter().all(Address::approved_for_connection));
            }
            (Some(failure), Err(actual)) => {
                assert_eq!(swift_name(actual), failure, "{}", case.name)
            }
            (expected, actual) => panic!("{}: expected {expected:?}, got {actual:?}", case.name),
        }
        // The proof ADR 0132 asks for, at the level every backend shares:
        // the origin is asked for A only, and AAAA is asked only of
        // `ipv4only.arpa`, whatever the answers were.
        let asked: HashSet<_> = dns.asked().into_iter().collect();
        assert_eq!(
            asked,
            HashSet::from([
                ("page.example.org".to_owned(), RecordType::A),
                (DISCOVERY.to_owned(), RecordType::Aaaa),
            ]),
            "{}",
            case.name
        );
    }
}

#[test]
fn fixture_metadata() {
    for case in boundary().metadata {
        let bytes: Vec<u8> = if case.latin1 == Some(true) {
            case.html
                .chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap())
                .collect()
        } else {
            case.html.clone().into_bytes()
        };
        let content_type = case.content_type.as_deref().unwrap_or("text/html");
        match (page::parse(&bytes, content_type), &case.failure) {
            (Ok(page), None) => {
                assert_eq!(page.title, case.title, "{}", case.name);
                assert_eq!(page.description, case.description, "{}", case.name);
                assert_eq!(page.site_name, case.site_name, "{}", case.name);
                assert_eq!(page.image, case.image, "{}", case.name);
            }
            (Err(actual), Some(expected)) => {
                assert_eq!(swift_name(actual), expected, "{}", case.name)
            }
            (actual, expected) => panic!("{}: expected {expected:?}, got {actual:?}", case.name),
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution timing and cancellation, as `PreviewResolutionTests` has them.

#[tokio::test(start_paused = true)]
async fn slow_discovery_keeps_the_public_a_within_the_shared_budget() {
    let mut dns = Recording::answering(Some("1.1.1.1"), Some("64:ff9b::c000:aa"));
    dns.delays.insert(RecordType::Aaaa, Duration::from_secs(10));
    let deadline = Instant::now() + Duration::from_millis(20);
    let addresses = dns::resolve(&dns, "page.example.org", deadline, &Cancel::new())
        .await
        .unwrap();
    assert_eq!(
        addresses.iter().map(Address::ip).collect::<Vec<_>>(),
        vec![address("1.1.1.1").ip()]
    );
    let mut dns = Recording::answering(Some("1.1.1.1"), Some("64:ff9b::c000:aa"));
    dns.delays.insert(RecordType::A, Duration::from_secs(10));
    assert_eq!(
        dns::resolve(&dns, "page.example.org", deadline, &Cancel::new()).await,
        Err(Failure::Expired)
    );
}

#[tokio::test]
async fn optional_discovery_errors_never_authorize_raw_ipv6() {
    for failure in [
        Failure::Expired,
        Failure::Unavailable,
        Failure::PreviewUnavailable,
    ] {
        let mut dns = Recording::answering(Some("1.1.1.1"), None);
        dns.answers
            .insert((DISCOVERY.into(), RecordType::Aaaa), Err(failure));
        let addresses = dns::resolve(&dns, "page.example.org", soon(), &Cancel::new())
            .await
            .unwrap();
        assert_eq!(addresses.len(), 1);
    }
    let dns = Recording::answering(Some("1.1.1.1"), Some("2606:4700:4700::1111"));
    assert_eq!(
        dns::resolve(&dns, "page.example.org", soon(), &Cancel::new())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn cancellation_wins_over_a_usable_answer() {
    let dns = Recording::answering(Some("1.1.1.1"), None);
    let cancel = Cancel::new();
    cancel.cancel();
    assert_eq!(
        dns::resolve(&dns, "page.example.org", soon(), &cancel).await,
        Err(Failure::Cancelled)
    );
    assert!(dns.asked().is_empty());
    let mut dns = Recording::answering(Some("1.1.1.1"), None);
    dns.answers.insert(
        (DISCOVERY.into(), RecordType::Aaaa),
        Err(Failure::Cancelled),
    );
    assert_eq!(
        dns::resolve(&dns, "page.example.org", soon(), &Cancel::new()).await,
        Err(Failure::Cancelled)
    );
}

// ---------------------------------------------------------------------------
// The Linux wire client, against a real DNS server on the loopback. It is the
// same code on every platform, so this proves the query types wherever the
// tests run: the server sees exactly one A question for the origin and one
// AAAA question for `ipv4only.arpa`.

/// A question the server received: name, type, and `udp` or `tcp`.
type Question = (String, u16, &'static str);

struct DnsServer {
    address: SocketAddr,
    questions: Arc<Mutex<Vec<Question>>>,
}

async fn dns_server(truncate_a: bool) -> DnsServer {
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = udp.local_addr().unwrap();
    let tcp = TcpListener::bind(address).await.unwrap();
    let questions = Arc::new(Mutex::new(Vec::new()));
    let answer = |request: &[u8], truncated: bool| {
        let (_, name, kind) = message::question(request).unwrap();
        let answers: Vec<IpAddr> = match (name.as_str(), kind) {
            ("page.example.org", 1) => vec!["8.8.4.4".parse().unwrap()],
            ("ipv4only.arpa", 28) => vec!["64:ff9b::c000:aa".parse().unwrap()],
            _ => vec![],
        };
        message::respond(
            request,
            if answers.is_empty() { 3 } else { 0 },
            &answers,
            truncated,
        )
    };
    let seen = questions.clone();
    tokio::spawn(async move {
        let mut buffer = [0u8; 2048];
        loop {
            let Ok((length, peer)) = udp.recv_from(&mut buffer).await else {
                return;
            };
            let request = buffer[..length].to_vec();
            let (_, name, kind) = message::question(&request).unwrap();
            seen.lock().unwrap().push((name, kind, "udp"));
            let truncated = truncate_a && kind == 1;
            let _ = udp.send_to(&answer(&request, truncated), peer).await;
        }
    });
    let seen = questions.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = tcp.accept().await {
            let mut prefix = [0u8; 2];
            stream.read_exact(&mut prefix).await.unwrap();
            let mut request = vec![0u8; usize::from(u16::from_be_bytes(prefix))];
            stream.read_exact(&mut request).await.unwrap();
            let (_, name, kind) = message::question(&request).unwrap();
            seen.lock().unwrap().push((name, kind, "tcp"));
            let response = answer(&request, false);
            let mut framed = (response.len() as u16).to_be_bytes().to_vec();
            framed.extend_from_slice(&response);
            stream.write_all(&framed).await.unwrap();
        }
    });
    DnsServer { address, questions }
}

#[tokio::test]
async fn the_wire_client_sends_only_an_a_question_for_the_origin() {
    let server = dns_server(false).await;
    let dns = WireDns::new(vec![server.address]);
    let addresses = dns::resolve(&dns, "page.example.org", soon(), &Cancel::new())
        .await
        .unwrap();
    assert_eq!(
        addresses.iter().map(Address::ip).collect::<Vec<_>>(),
        vec![address("64:ff9b::808:404").ip(), address("8.8.4.4").ip()]
    );
    let mut questions = server.questions.lock().unwrap().clone();
    questions.sort();
    assert_eq!(
        questions,
        vec![
            ("ipv4only.arpa".to_owned(), 28, "udp"),
            ("page.example.org".to_owned(), 1, "udp"),
        ]
    );
}

#[tokio::test]
async fn a_truncated_answer_is_asked_again_over_tcp_with_the_same_question() {
    let server = dns_server(true).await;
    let dns = WireDns::new(vec![server.address]);
    dns::resolve(&dns, "page.example.org", soon(), &Cancel::new())
        .await
        .unwrap();
    let mut questions = server.questions.lock().unwrap().clone();
    questions.sort();
    assert_eq!(
        questions,
        vec![
            ("ipv4only.arpa".to_owned(), 28, "udp"),
            ("page.example.org".to_owned(), 1, "tcp"),
            ("page.example.org".to_owned(), 1, "udp"),
        ]
    );
}

#[tokio::test]
async fn a_negative_answer_on_the_wire_requires_public_ipv4() {
    let server = dns_server(false).await;
    let dns = WireDns::new(vec![server.address]);
    assert_eq!(
        dns::resolve(&dns, "missing.example.org", soon(), &Cancel::new()).await,
        Err(Failure::PublicIpv4Required)
    );
}

#[tokio::test]
async fn a_silent_server_runs_into_the_deadline() {
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dns = WireDns::new(vec![silent.local_addr().unwrap()]);
    let deadline = Instant::now() + Duration::from_millis(200);
    assert_eq!(
        dns::resolve(&dns, "page.example.org", deadline, &Cancel::new()).await,
        Err(Failure::Expired)
    );
}

/// A `resolv.conf` can only name port 53, so this checks the list each query
/// would use rather than sending to it; the tests above cover sending.
#[tokio::test]
async fn the_wire_client_rereads_resolv_conf_for_each_query() {
    let path = std::env::temp_dir().join(format!(
        "axon-link-preview-resolv-{}.conf",
        std::process::id()
    ));
    let dns = WireDns::from_file(&path);
    std::fs::write(&path, "nameserver 192.0.2.1\n").unwrap();
    assert_eq!(
        dns.current().await,
        vec!["192.0.2.1:53".parse::<SocketAddr>().unwrap()]
    );
    // The network changed, and NetworkManager rewrote the file.
    std::fs::write(&path, "nameserver 198.51.100.1\nnameserver 192.0.2.2\n").unwrap();
    assert_eq!(
        dns.current().await,
        vec![
            "198.51.100.1:53".parse::<SocketAddr>().unwrap(),
            "192.0.2.2:53".parse::<SocketAddr>().unwrap(),
        ]
    );
    // And when the file is gone, glibc's default.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        dns.current().await,
        vec!["127.0.0.1:53".parse::<SocketAddr>().unwrap()]
    );
}

// ---------------------------------------------------------------------------
// The fetch loop, with stand-ins for both steps of a hop, as
// `RestrictedPreviewTests` and `PreviewResolutionTests` have it.

type Resolver = Box<dyn Fn(&str) -> Result<Vec<Address>, Failure> + Send + Sync>;
type Exchanger =
    Box<dyn Fn(&Checked, &[Address], Resource) -> Result<Response, Failure> + Send + Sync>;

struct Scripted {
    resolve: Resolver,
    exchange: Exchanger,
    lookups: AtomicUsize,
    exchanges: AtomicUsize,
    resolve_delay: Duration,
    image_delay: Duration,
}

impl Scripted {
    fn new(resolve: Resolver, exchange: Exchanger) -> Self {
        Scripted {
            resolve,
            exchange,
            lookups: AtomicUsize::new(0),
            exchanges: AtomicUsize::new(0),
            resolve_delay: Duration::ZERO,
            image_delay: Duration::ZERO,
        }
    }
}

impl Hop for Scripted {
    async fn resolve(
        &self,
        host: &str,
        _deadline: Instant,
        _cancel: &Cancel,
    ) -> Result<Vec<Address>, Failure> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.resolve_delay).await;
        (self.resolve)(host)
    }

    async fn exchange(
        &self,
        checked: &Checked,
        addresses: &[Address],
        kind: Resource,
        _deadline: Instant,
    ) -> Result<Response, Failure> {
        self.exchanges.fetch_add(1, Ordering::SeqCst);
        if kind == Resource::Image {
            tokio::time::sleep(self.image_delay).await;
        }
        (self.exchange)(checked, addresses, kind)
    }
}

fn response(status: u16, headers: &[(&str, &str)], body: &[u8], url: &url::Url) -> Response {
    Response {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body: body.to_vec(),
        url: url.clone(),
    }
}

fn public(_host: &str) -> Result<Vec<Address>, Failure> {
    Ok(vec![address("8.8.8.8")])
}

#[tokio::test]
async fn dns_rebinding_and_redirects_cannot_reach_private_endpoints() {
    let lookups = Arc::new(AtomicUsize::new(0));
    let counter = lookups.clone();
    let hop = Scripted::new(
        Box::new(move |host| {
            assert!(host.ends_with("example.org"));
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            Ok(vec![address(if first { "8.8.8.8" } else { "127.0.0.1" })])
        }),
        Box::new(|checked, addresses, _| {
            assert_eq!(addresses, [address("8.8.8.8")]);
            assert_eq!(checked.host, "page.example.org");
            Ok(response(
                302,
                &[("location", "/redirected")],
                b"",
                &checked.url,
            ))
        }),
    );
    let result = net::fetch(
        &hop,
        "https://page.example.org/a",
        Resource::Page,
        soon(),
        &Cancel::new(),
    )
    .await;
    assert_eq!(result.err(), Some(Failure::PreviewUnavailable));
    assert_eq!(hop.lookups.load(Ordering::SeqCst), 2);
    assert_eq!(hop.exchanges.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_literal_private_redirect_is_rejected_before_a_second_lookup() {
    let hop = Scripted::new(
        Box::new(public),
        Box::new(|checked, _, _| {
            Ok(response(
                302,
                &[("location", "https://192.168.1.1/admin")],
                b"",
                &checked.url,
            ))
        }),
    );
    let result = net::fetch(
        &hop,
        "https://example.org",
        Resource::Page,
        soon(),
        &Cancel::new(),
    )
    .await;
    assert_eq!(result.err(), Some(Failure::PreviewUnavailable));
    assert_eq!(hop.lookups.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn redirects_stop_after_six_requests() {
    let hop = Scripted::new(
        Box::new(public),
        Box::new(|checked, _, _| Ok(response(301, &[("location", "/again")], b"", &checked.url))),
    );
    let result = net::fetch(
        &hop,
        "https://example.org/",
        Resource::Page,
        soon(),
        &Cancel::new(),
    )
    .await;
    assert_eq!(result.err(), Some(Failure::PreviewUnavailable));
    assert_eq!(hop.exchanges.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn unsafe_or_oversized_candidate_lists_never_reach_the_exchange() {
    for addresses in [
        vec![],
        vec![address("1.1.1.1"), address("127.0.0.1")],
        vec![address("1.1.1.1"); 3],
        vec![address("2606:4700:1234:5678::a00:1")],
    ] {
        let hop = Scripted::new(
            Box::new(move |_| Ok(addresses.clone())),
            Box::new(|_, _, _| panic!("unsafe candidates reached the exchange")),
        );
        let result = net::fetch(
            &hop,
            "https://example.org",
            Resource::Page,
            soon(),
            &Cancel::new(),
        )
        .await;
        assert_eq!(result.err(), Some(Failure::PreviewUnavailable));
    }
}

#[tokio::test]
async fn cancellation_prevents_any_network_work() {
    let hop = Scripted::new(
        Box::new(|_| panic!("a cancelled lookup resolved")),
        Box::new(|_, _, _| panic!("a cancelled lookup connected")),
    );
    let cancel = Cancel::new();
    cancel.cancel();
    let result = net::fetch(&hop, "https://example.org", Resource::Page, soon(), &cancel).await;
    assert_eq!(result.err(), Some(Failure::Cancelled));
}

#[tokio::test(start_paused = true)]
async fn resolution_cannot_reset_the_request_deadline() {
    let mut hop = Scripted::new(
        Box::new(public),
        Box::new(|_, _, _| panic!("resolution consumed the original deadline")),
    );
    hop.resolve_delay = Duration::from_millis(20);
    let deadline = Instant::now() + Duration::from_millis(10);
    let result = net::fetch(
        &hop,
        "https://example.org",
        Resource::Page,
        deadline,
        &Cancel::new(),
    )
    .await;
    assert_eq!(result.err(), Some(Failure::Expired));
}

#[tokio::test]
async fn a_blocked_image_keeps_the_text_and_never_connects() {
    let hop = Scripted::new(
        Box::new(public),
        Box::new(|checked, _, _| {
            Ok(response(
                200,
                &[("content-type", "text/html")],
                b"<title>Keep title</title><meta property='og:image' content='https://127.0.0.1/image'>",
                &checked.url,
            ))
        }),
    );
    let fetched = preview::fetch(&hop, "https://example.org/article", soon(), &Cancel::new())
        .await
        .unwrap();
    assert_eq!(hop.exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(fetched.title.as_deref(), Some("Keep title"));
    assert!(fetched.image_unavailable);
    assert!(fetched.image.is_none());
}

#[tokio::test]
async fn a_relative_image_uses_the_validated_transport() {
    let urls = Arc::new(Mutex::new(Vec::new()));
    let seen = urls.clone();
    let hop = Scripted::new(
        Box::new(public),
        Box::new(move |checked, _, kind| {
            seen.lock().unwrap().push(checked.url.to_string());
            Ok(match kind {
                Resource::Page => response(
                    200,
                    &[("content-type", "text/html")],
                    b"<meta property='og:title' content='Title'><meta property='og:image' content='photo.png'>",
                    &checked.url,
                ),
                Resource::Image => {
                    response(200, &[("content-type", "image/png")], &[1, 2, 3], &checked.url)
                }
            })
        }),
    );
    let fetched = preview::fetch(
        &hop,
        "https://example.org/path/article",
        soon(),
        &Cancel::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        *urls.lock().unwrap(),
        vec![
            "https://example.org/path/article".to_owned(),
            "https://example.org/path/photo.png".to_owned()
        ]
    );
    assert_eq!(fetched.image, Some(vec![1, 2, 3]));
}

#[tokio::test]
async fn a_non_html_page_is_unavailable_and_an_image_page_is_titled() {
    let hop = Scripted::new(
        Box::new(public),
        Box::new(|checked, _, _| {
            Ok(response(
                200,
                &[("content-type", "application/json")],
                b"{}",
                &checked.url,
            ))
        }),
    );
    assert_eq!(
        preview::fetch(&hop, "https://example.org/a", soon(), &Cancel::new())
            .await
            .err(),
        Some(Failure::PreviewUnavailable)
    );
    let hop = Scripted::new(
        Box::new(public),
        Box::new(|checked, _, _| {
            Ok(response(
                200,
                &[("content-type", "image/png")],
                &[9],
                &checked.url,
            ))
        }),
    );
    let fetched = preview::fetch(
        &hop,
        "https://example.org/a/cat%20photo.png",
        soon(),
        &Cancel::new(),
    )
    .await
    .unwrap();
    assert_eq!(fetched.title.as_deref(), Some("cat photo.png"));
    assert_eq!(fetched.image, Some(vec![9]));
}

#[tokio::test]
async fn a_lookup_reports_the_kinds_the_page_sees() {
    let hop = Scripted::new(
        Box::new(|_| Err(Failure::PublicIpv4Required)),
        Box::new(|_, _, _| unreachable!()),
    );
    let failure = |url: &'static str| {
        let hop = &hop;
        async move {
            preview::lookup(
                hop,
                url,
                Instant::now() + preview::DEADLINE,
                &Cancel::new(),
                &preview::Normalizer::new(),
            )
            .await
            .unwrap_err()
            .kind
        }
    };
    assert_eq!(failure("http://example.org/").await, Kind::UnsupportedUrl);
    assert_eq!(
        failure("https://example.org/").await,
        Kind::PublicIpv4Required
    );
    let empty = Scripted::new(
        Box::new(public),
        Box::new(|checked, _, _| {
            Ok(response(
                200,
                &[("content-type", "image/png")],
                b"not a png",
                &checked.url,
            ))
        }),
    );
    // An image-only page whose image fails to decode has nothing to show,
    // except the title Foundation gives the root path.
    let preview = preview::lookup(
        &empty,
        "https://example.org/",
        Instant::now() + preview::DEADLINE,
        &Cancel::new(),
        &preview::Normalizer::new(),
    )
    .await
    .unwrap();
    assert_eq!(preview.title.as_deref(), Some("/"));
    assert!(preview.image_unavailable);
}

// ---------------------------------------------------------------------------
// Decoding stays inside the lookup's deadline and cancellation, and within a
// budget of its own when a lookup stops waiting for it.

/// A decoder that blocks until released, and counts how many decodes began.
struct HeldDecoder {
    started: Arc<AtomicUsize>,
    begun: Arc<tokio::sync::Notify>,
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl HeldDecoder {
    fn new() -> Self {
        HeldDecoder {
            started: Arc::new(AtomicUsize::new(0)),
            begun: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        }
    }

    fn normalizer(&self) -> preview::Normalizer {
        let (started, begun, release) = (
            self.started.clone(),
            self.begun.clone(),
            self.release.clone(),
        );
        preview::Normalizer::with(Arc::new(move |_bytes: &[u8]| {
            started.fetch_add(1, Ordering::SeqCst);
            begun.notify_one();
            let (released, wake) = &*release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            Err(Failure::Invalid)
        }))
    }

    fn release(&self) {
        let (released, wake) = &*self.release;
        *released.lock().unwrap() = true;
        wake.notify_all();
    }
}

fn page_with_image() -> Arc<Scripted> {
    Arc::new(Scripted::new(
        Box::new(public),
        Box::new(|checked, _, kind| {
            Ok(match kind {
                Resource::Page => response(
                    200,
                    &[("content-type", "text/html")],
                    b"<title>Kept</title><meta property='og:image' content='/i.png'>",
                    &checked.url,
                ),
                Resource::Image => {
                    response(200, &[("content-type", "image/png")], &[1], &checked.url)
                }
            })
        }),
    ))
}

fn spawn_lookup(
    hop: Arc<Scripted>,
    normalizer: Arc<preview::Normalizer>,
    cancel: Cancel,
) -> tokio::task::JoinHandle<Result<preview::Preview, crate::LookupError>> {
    tokio::spawn(async move {
        preview::lookup(
            &*hop,
            "https://example.org/a",
            Instant::now() + preview::DEADLINE,
            &cancel,
            &normalizer,
        )
        .await
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_lookup_stops_waiting_for_its_image() {
    let decoder = HeldDecoder::new();
    let normalizer = Arc::new(decoder.normalizer());
    let cancel = Cancel::new();
    let lookup = spawn_lookup(page_with_image(), normalizer, cancel.clone());
    decoder.begun.notified().await;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), lookup).await;
    // Released before asserting, so a failure fails instead of leaving a
    // blocked thread the runtime waits for at shutdown.
    decoder.release();
    assert_eq!(result.unwrap().unwrap().unwrap_err().kind, Kind::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_covers_decoding_and_abandoned_decodes_stay_within_budget() {
    let decoder = HeldDecoder::new();
    let normalizer = Arc::new(decoder.normalizer());
    // Two lookups stop waiting for their decodes, which keep running.
    for _ in 0..2 {
        let cancel = Cancel::new();
        let lookup = spawn_lookup(page_with_image(), normalizer.clone(), cancel.clone());
        decoder.begun.notified().await;
        cancel.cancel();
        assert_eq!(lookup.await.unwrap().unwrap_err().kind, Kind::Cancelled);
    }
    // A third waits for a decoding slot only until its deadline, and keeps
    // its text.
    let begun = Instant::now();
    let lookup = spawn_lookup(page_with_image(), normalizer.clone(), Cancel::new());
    tokio::task::yield_now().await;
    tokio::time::advance(preview::DEADLINE + Duration::from_secs(1)).await;
    let result = tokio::time::timeout(Duration::from_secs(60), lookup).await;
    let started = decoder.started.load(Ordering::SeqCst);
    decoder.release();
    let preview = result.unwrap().unwrap().unwrap();
    assert_eq!(preview.title.as_deref(), Some("Kept"));
    assert!(preview.image.is_none());
    assert!(preview.image_unavailable);
    assert!(Instant::now() - begun >= preview::DEADLINE);
    // No third decode was started while two were still held.
    assert_eq!(started, 2);
}

#[tokio::test(start_paused = true)]
async fn an_image_that_misses_the_deadline_costs_only_the_image() {
    let hop = page_with_image();
    let mut hop = Arc::try_unwrap(hop).ok().unwrap();
    hop.image_delay = preview::DEADLINE * 2;
    let preview = preview::lookup(
        &hop,
        "https://example.org/a",
        Instant::now() + preview::DEADLINE,
        &Cancel::new(),
        &preview::Normalizer::new(),
    )
    .await
    .unwrap();
    assert_eq!(preview.title.as_deref(), Some("Kept"));
    assert!(preview.image_unavailable);
}

#[tokio::test]
async fn an_image_url_that_cannot_be_resolved_is_unavailable() {
    let hop = Scripted::new(
        Box::new(public),
        Box::new(|checked, _, _| {
            Ok(response(
                200,
                &[("content-type", "text/html")],
                b"<title>Kept</title><meta property='og:image' content='https://['>",
                &checked.url,
            ))
        }),
    );
    let fetched = preview::fetch(&hop, "https://example.org/a", soon(), &Cancel::new())
        .await
        .unwrap();
    assert_eq!(fetched.title.as_deref(), Some("Kept"));
    assert!(fetched.image_unavailable);
    assert_eq!(hop.exchanges.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Registrations, as the commands make them.

#[tokio::test]
async fn a_cancel_that_arrives_before_its_lookup_still_cancels_it() {
    let lookups = crate::Lookups::new();
    let key = ("main".to_owned(), 7);
    lookups.cancel(&key);
    let hop = Scripted::new(
        Box::new(|_| panic!("a cancelled lookup resolved")),
        Box::new(|_, _, _| panic!("a cancelled lookup connected")),
    );
    let result = lookups.run(&hop, key.clone(), "https://example.org/").await;
    assert_eq!(result.unwrap_err().kind, Kind::Cancelled);
    assert!(lookups.book().early_cancels.is_empty(), "used once");
    // Bounded, and a reload forgets its window's.
    for id in 0..40 {
        lookups.cancel(&("main".to_owned(), id));
    }
    assert_eq!(lookups.book().early_cancels.len(), 16);
    lookups.forget_webview("main");
    assert!(lookups.book().early_cancels.is_empty());
}

#[tokio::test(start_paused = true)]
async fn waiting_for_a_slot_spends_the_lookups_own_deadline() {
    let lookups = Arc::new(crate::Lookups::new());
    // Both slots busy for 20 seconds; then the page's DNS takes 10.
    let held = lookups.running.clone().acquire_many_owned(2).await.unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        drop(held);
    });
    let mut hop = Scripted::new(
        Box::new(public),
        Box::new(|checked, _, _| {
            Ok(response(
                200,
                &[("content-type", "text/html")],
                b"<title>T</title>",
                &checked.url,
            ))
        }),
    );
    hop.resolve_delay = Duration::from_secs(10);
    let begun = Instant::now();
    let result = lookups
        .run(&hop, ("main".to_owned(), 1), "https://example.org/")
        .await;
    assert_eq!(result.unwrap_err().kind, Kind::Timeout);
    assert_eq!(Instant::now() - begun, preview::DEADLINE);
}

#[test]
fn an_old_command_finishing_keeps_a_new_registration_under_the_same_key() {
    let lookups = crate::Lookups::new();
    let key = ("main".to_owned(), 1);
    let (old, old_cancel) = lookups.register(key.clone()).unwrap();
    // A reload: the old page's lookup is cancelled, and the new page starts
    // its ids from 1 again.
    lookups.forget_webview("main");
    assert!(old_cancel.is_cancelled());
    let (_new, new_cancel) = lookups.register(key.clone()).unwrap();
    // The old command unwinds after the new one registered.
    lookups.finish(&key, old);
    lookups.cancel(&key);
    assert!(
        new_cancel.is_cancelled(),
        "the new lookup can still be cancelled"
    );
    assert!(lookups.book().registered.is_empty());
}

// ---------------------------------------------------------------------------
// The connection race, as `PreviewConnectionRaceTests` has it.

#[tokio::test(start_paused = true)]
async fn a_stalled_preferred_address_falls_back_after_the_delay() {
    let started = Arc::new(Mutex::new(Vec::new()));
    let log = started.clone();
    let winner = net::race(&[0u8, 1], Duration::from_millis(250), |index| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push((index, Instant::now()));
            if index == 0 {
                std::future::pending::<()>().await;
            }
            Ok(index)
        }
    })
    .await;
    assert_eq!(winner, Ok(1));
    let started = started.lock().unwrap();
    assert_eq!(started.len(), 2);
    assert_eq!(started[1].1 - started[0].1, Duration::from_millis(250));
}

#[tokio::test(start_paused = true)]
async fn a_fast_preferred_address_does_not_start_the_alternate() {
    let started = Arc::new(AtomicUsize::new(0));
    let count = started.clone();
    let winner = net::race(&[0u8, 1], Duration::from_millis(250), |index| {
        count.fetch_add(1, Ordering::SeqCst);
        async move { Ok(index) }
    })
    .await;
    assert_eq!(winner, Ok(0));
    assert_eq!(started.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn an_immediate_failure_starts_the_alternate_at_once() {
    let begun = Instant::now();
    let winner = net::race(&[0u8, 1], Duration::from_secs(10), |index| async move {
        if index == 0 {
            Err(Failure::Unavailable)
        } else {
            Ok(index)
        }
    })
    .await;
    assert_eq!(winner, Ok(1));
    assert_eq!(Instant::now(), begun);
}

#[tokio::test(start_paused = true)]
async fn when_every_address_fails_the_race_fails() {
    let result: Result<u8, Failure> = net::race(&[0u8, 1], Duration::from_millis(5), |_| async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        Err(Failure::Unavailable)
    })
    .await;
    assert_eq!(result, Err(Failure::Unavailable));
    let none: Result<u8, Failure> =
        net::race(&[] as &[u8], Duration::ZERO, |_| async { Ok(0) }).await;
    assert_eq!(none, Err(Failure::PreviewUnavailable));
}

// ---------------------------------------------------------------------------
// End to end: the production transport, TLS and all, against a local server.
// DNS answers a public address and the dialer redirects that one connection
// to the loopback, so the validation in front of the socket stays real.

fn tls_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(
            include_bytes!("../testdata/root.cert.der").to_vec(),
        ))
        .unwrap();
    roots
}

#[derive(Clone)]
struct Loopback {
    port: u16,
    dialed: Arc<Mutex<Vec<SocketAddr>>>,
}

impl Dial for Loopback {
    async fn dial(&self, address: SocketAddr) -> std::io::Result<TcpStream> {
        self.dialed.lock().unwrap().push(address);
        TcpStream::connect(("127.0.0.1", self.port)).await
    }
}

struct PublicDns;

impl Dns for PublicDns {
    async fn query(
        &self,
        name: &str,
        record: RecordType,
        _deadline: Instant,
        _cancel: &Cancel,
    ) -> Result<Answer, Failure> {
        match (name, record) {
            (DISCOVERY, RecordType::Aaaa) => Ok(Answer::NoRecord),
            (_, RecordType::A) => Ok(Answer::Address("93.184.215.14".parse().unwrap())),
            _ => panic!("an AAAA query for the origin {name}"),
        }
    }
}

/// Serves one canned response per path, and records each request's head.
async fn https_server(routes: HashMap<&'static str, Vec<u8>>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                include_bytes!("../testdata/page.cert.der").to_vec(),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                include_bytes!("../testdata/page.key.der").to_vec(),
            )),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let routes = routes.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if tls.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                seen.lock().unwrap().push(head);
                let body = routes.get(path.as_str()).cloned().unwrap_or_else(|| {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                });
                let _ = tls.write_all(&body).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, requests)
}

fn network(port: u16) -> (Network<PublicDns, Loopback>, Arc<Mutex<Vec<SocketAddr>>>) {
    let dialed = Arc::new(Mutex::new(Vec::new()));
    (
        Network {
            dns: PublicDns,
            dial: Loopback {
                port,
                dialed: dialed.clone(),
            },
            tls: net::tls_config(tls_roots()),
            race_delay: net::RACE_DELAY,
        },
        dialed,
    )
}

fn ok(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[tokio::test]
async fn a_lookup_end_to_end_over_tls() {
    let photo = crate::image::tests::encoded(
        &image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            1600,
            800,
            image::Rgb([9, 99, 199]),
        )),
        image::ImageFormat::Png,
    );
    let routes = HashMap::from([
        (
            "/start?x=1",
            b"HTTP/1.1 302 Found\r\nLocation: https://images.example.org/article\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
        ),
        (
            "/article",
            ok(
                "text/html; charset=utf-8",
                b"<html><head><title>Fallback</title><meta property='og:title' content='A title'><meta property='og:description' content='About it'><meta property='og:site_name' content='Site'><meta property='og:image' content='/photo.png'></head><body>never read",
            ),
        ),
        ("/photo.png", ok("image/png", &photo)),
    ]);
    let (port, requests) = https_server(routes).await;
    let (network, dialed) = network(port);
    let preview = preview::lookup(
        &network,
        "https://page.example.org/start?x=1#frag",
        Instant::now() + preview::DEADLINE,
        &Cancel::new(),
        &preview::Normalizer::new(),
    )
    .await
    .unwrap();
    assert_eq!(preview.url, "https://images.example.org/article");
    assert_eq!(preview.title.as_deref(), Some("A title"));
    assert_eq!(preview.description.as_deref(), Some("About it"));
    assert_eq!(preview.site_name.as_deref(), Some("Site"));
    let image = preview.image.unwrap();
    assert_eq!(
        (image.mime, image.width, image.height),
        ("image/jpeg", 1200, 600)
    );
    assert!(!preview.image_unavailable);
    // Every connection went to the validated public address, on 443.
    assert!(dialed
        .lock()
        .unwrap()
        .iter()
        .all(|address| *address == "93.184.215.14:443".parse().unwrap()));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0],
        "GET /start?x=1 HTTP/1.1\r\nHost: page.example.org\r\nUser-Agent: AxonLinkPreview/1.0\r\nAccept: text/html,application/xhtml+xml,image/*;q=0.8\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    assert!(requests[1].starts_with("GET /article HTTP/1.1\r\nHost: images.example.org\r\n"));
    for request in requests.iter() {
        let lower = request.to_ascii_lowercase();
        assert!(!lower.contains("cookie") && !lower.contains("authorization"));
    }
}

#[tokio::test]
async fn tls_is_verified_against_the_original_host_name() {
    let (port, requests) =
        https_server(HashMap::from([("/", ok("text/html", b"<title>T</title>"))])).await;
    let (network, dialed) = network(port);
    // The certificate names page.example.org and images.example.org only.
    let result = preview::lookup(
        &network,
        "https://other.example.org/",
        Instant::now() + preview::DEADLINE,
        &Cancel::new(),
        &preview::Normalizer::new(),
    )
    .await;
    assert_eq!(result.unwrap_err().kind, Kind::Unavailable);
    assert_eq!(dialed.lock().unwrap().len(), 1);
    assert!(requests.lock().unwrap().is_empty());
    // And against roots it was given: Mozilla's do not include the test root.
    let (mut network, _) = self::network(port);
    network.tls = net::tls_config(net::mozilla_roots());
    let result = preview::lookup(
        &network,
        "https://page.example.org/",
        Instant::now() + preview::DEADLINE,
        &Cancel::new(),
        &preview::Normalizer::new(),
    )
    .await;
    assert_eq!(result.unwrap_err().kind, Kind::Unavailable);
}

#[tokio::test]
async fn a_cancelled_lookup_stops_waiting() {
    // A server that accepts and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let (network, _) = network(port);
    let cancel = Cancel::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel();
    });
    let begun = std::time::Instant::now();
    let result = preview::lookup(
        &network,
        "https://page.example.org/",
        Instant::now() + preview::DEADLINE,
        &cancel,
        &preview::Normalizer::new(),
    )
    .await;
    assert_eq!(result.unwrap_err().kind, Kind::Cancelled);
    assert!(begun.elapsed() < Duration::from_secs(5));
}

// ---------------------------------------------------------------------------
// The frame the page decodes.

#[test]
fn a_frame_is_a_length_json_and_the_image_bytes() {
    let preview = preview::Preview {
        url: "https://example.org/".into(),
        title: Some("T".into()),
        description: None,
        site_name: None,
        image: Some(crate::image::Thumbnail {
            bytes: vec![1, 2, 3],
            mime: "image/png",
            width: 2,
            height: 1,
        }),
        image_unavailable: false,
    };
    let frame = crate::frame(&preview);
    let length = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    let head: serde_json::Value = serde_json::from_slice(&frame[4..4 + length]).unwrap();
    assert_eq!(
        head,
        serde_json::json!({
            "url": "https://example.org/",
            "title": "T",
            "image": { "mimeType": "image/png", "width": 2, "height": 1, "byteLength": 3 },
            "imageUnavailable": false,
        })
    );
    assert_eq!(&frame[4 + length..], &[1, 2, 3]);
}

#[test]
fn errors_serialize_as_a_kind_and_nothing_else() {
    for (kind, text) in [
        (Kind::UnsupportedUrl, "unsupported_url"),
        (Kind::PublicIpv4Required, "public_ipv4_required"),
        (Kind::Timeout, "timeout"),
        (Kind::NoMetadata, "no_metadata"),
        (Kind::Unavailable, "unavailable"),
        (Kind::Busy, "busy"),
        (Kind::Cancelled, "cancelled"),
    ] {
        assert_eq!(
            serde_json::to_value(crate::LookupError::from(kind)).unwrap(),
            serde_json::json!({ "kind": text })
        );
    }
}

// ---------------------------------------------------------------------------
// Manual probes of the production path, this platform's resolver included, as
// the Swift reader's `testLive*` tests are. Run with
// `AXON_PREVIEW_LIVE=1 cargo test -p tauri-plugin-axon-link-preview live -- --nocapture`;
// `AXON_PREVIEW_URL` picks the page.

fn live() -> Option<Network<dns::System, net::Direct>> {
    (std::env::var_os("AXON_PREVIEW_LIVE").is_some_and(|v| v == "1")).then(|| Network {
        dns: dns::system(),
        dial: net::Direct,
        tls: net::tls_config(net::mozilla_roots()),
        race_delay: net::RACE_DELAY,
    })
}

#[tokio::test]
async fn live_public_https_metadata() {
    let Some(network) = live() else {
        return;
    };
    let url = std::env::var("AXON_PREVIEW_URL").unwrap_or_else(|_| "https://example.com/".into());
    let preview = preview::lookup(
        &network,
        &url,
        Instant::now() + preview::DEADLINE,
        &Cancel::new(),
        &preview::Normalizer::new(),
    )
    .await
    .unwrap();
    assert!(preview.title.is_some());
    println!(
        "PREVIEW_LIVE title={} image_bytes={} image_unavailable={}",
        preview.title.is_some(),
        preview.image.as_ref().map_or(0, |image| image.bytes.len()),
        preview.image_unavailable
    );
}

#[tokio::test]
async fn live_ipv6_only_origin_reports_public_ipv4_requirement() {
    let Some(network) = live() else {
        return;
    };
    let result = preview::lookup(
        &network,
        "https://ipv6.google.com/",
        Instant::now() + preview::DEADLINE,
        &Cancel::new(),
        &preview::Normalizer::new(),
    )
    .await;
    assert_eq!(result.unwrap_err().kind, Kind::PublicIpv4Required);
}
