//! DNS messages (RFC 1035): one question out, the first matching address back.
//!
//! Used by the Linux wire client, which builds and sends the query, and by
//! Android, where netd builds it from the same name and type and hands back
//! the raw answer. Nothing here sizes an allocation from a peer's number:
//! every read is bounds-checked against the message, and compression pointers
//! may only point backwards.

use super::{Answer, RecordType};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The UDP payload size advertised with EDNS(0), so a long answer is not
/// truncated to 512 bytes (RFC 9715's safe size).
pub(crate) const UDP_PAYLOAD: u16 = 1232;

const CLASS_IN: u16 = 1;
const TYPE_OPT: u16 = 41;
const NXDOMAIN: u8 = 3;

/// A query for one name and type, recursion desired, with an EDNS(0) record.
pub(crate) fn query(id: u16, name: &str, record: RecordType) -> Option<Vec<u8>> {
    let mut message = Vec::with_capacity(32 + name.len());
    message.extend_from_slice(&id.to_be_bytes());
    // RD; one question; one additional (OPT).
    message.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 1]);
    encode_name(name, &mut message)?;
    message.extend_from_slice(&record.code().to_be_bytes());
    message.extend_from_slice(&CLASS_IN.to_be_bytes());
    // OPT: root name, type 41, class = payload size, TTL 0, no options.
    message.push(0);
    message.extend_from_slice(&TYPE_OPT.to_be_bytes());
    message.extend_from_slice(&UDP_PAYLOAD.to_be_bytes());
    message.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    Some(message)
}

fn encode_name(name: &str, into: &mut Vec<u8>) -> Option<()> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > 253 {
        return None;
    }
    for label in name.split('.') {
        let length = u8::try_from(label.len())
            .ok()
            .filter(|&n| (1..=63).contains(&n))?;
        into.push(length);
        into.extend_from_slice(label.as_bytes());
    }
    into.push(0);
    Some(())
}

/// Why an answer could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    /// Not a response to this query: wrong id or question. Keep waiting.
    Mismatch,
    /// Truncated over UDP; ask again over TCP.
    Truncated,
    /// The server failed (SERVFAIL, REFUSED and the like). Try another.
    ServerFailure,
    /// Not a well-formed message.
    Malformed,
}

struct Reader<'a> {
    message: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.message.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u16(&mut self) -> Option<u16> {
        self.bytes(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    /// A name, lower-cased, following compression pointers backwards only.
    fn name(&mut self) -> Option<String> {
        let mut labels: Vec<String> = Vec::new();
        let mut at = self.at;
        let mut resume = None;
        let mut total = 0usize;
        loop {
            let length = *self.message.get(at)?;
            match length {
                0 => {
                    at += 1;
                    break;
                }
                1..=63 => {
                    let label = self.message.get(at + 1..at + 1 + usize::from(length))?;
                    total += label.len() + 1;
                    if total > 255 {
                        return None;
                    }
                    labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
                    at += 1 + usize::from(length);
                }
                0xC0..=0xFF => {
                    let low = *self.message.get(at + 1)?;
                    let target = (usize::from(length & 0x3F) << 8) | usize::from(low);
                    if target >= at {
                        return None;
                    }
                    resume.get_or_insert(at + 2);
                    at = target;
                }
                _ => return None,
            }
        }
        self.at = resume.unwrap_or(at);
        Some(labels.join("."))
    }
}

/// The first `record` address in the answer section of a response to
/// `(id, name, record)`. `id` is `None` where the platform built the query
/// and keeps the id to itself.
pub(crate) fn parse(
    message: &[u8],
    id: Option<u16>,
    name: &str,
    record: RecordType,
) -> Result<Answer, Problem> {
    let mut reader = Reader { message, at: 0 };
    let header = reader.bytes(12).ok_or(Problem::Malformed)?;
    let response_id = u16::from_be_bytes([header[0], header[1]]);
    if id.is_some_and(|id| id != response_id) {
        return Err(Problem::Mismatch);
    }
    let flags = u16::from_be_bytes([header[2], header[3]]);
    let questions = u16::from_be_bytes([header[4], header[5]]);
    let answers = u16::from_be_bytes([header[6], header[7]]);
    // QR set, opcode QUERY.
    if flags & 0x8000 == 0 || (flags >> 11) & 0xF != 0 {
        return Err(Problem::Mismatch);
    }
    if questions != 1 {
        return Err(Problem::Mismatch);
    }
    let asked = reader.name().ok_or(Problem::Malformed)?;
    let question_type = reader.u16().ok_or(Problem::Malformed)?;
    let question_class = reader.u16().ok_or(Problem::Malformed)?;
    let expected = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    if asked != expected || question_type != record.code() || question_class != CLASS_IN {
        return Err(Problem::Mismatch);
    }
    if flags & 0x0200 != 0 {
        return Err(Problem::Truncated);
    }
    let rcode = (flags & 0x000F) as u8;
    if rcode == NXDOMAIN {
        return Ok(Answer::NoRecord);
    }
    if rcode != 0 {
        return Err(Problem::ServerFailure);
    }
    for _ in 0..answers {
        reader.name().ok_or(Problem::Malformed)?;
        let kind = reader.u16().ok_or(Problem::Malformed)?;
        let class = reader.u16().ok_or(Problem::Malformed)?;
        reader.bytes(4).ok_or(Problem::Malformed)?;
        let length = reader.u16().ok_or(Problem::Malformed)?;
        let data = reader
            .bytes(usize::from(length))
            .ok_or(Problem::Malformed)?;
        // A CNAME chain comes first; its records are skipped.
        if class != CLASS_IN || kind != record.code() {
            continue;
        }
        return match (record, data.len()) {
            (RecordType::A, 4) => Ok(Answer::Address(IpAddr::V4(Ipv4Addr::new(
                data[0], data[1], data[2], data[3],
            )))),
            (RecordType::Aaaa, 16) => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(data);
                Ok(Answer::Address(IpAddr::V6(Ipv6Addr::from(octets))))
            }
            _ => Err(Problem::Malformed),
        };
    }
    Ok(Answer::NoRecord)
}

/// The question of a query this module built: `(id, name, type)`. For the
/// tests' DNS server, and nothing else.
#[cfg(test)]
pub(crate) fn question(message: &[u8]) -> Option<(u16, String, u16)> {
    let mut reader = Reader { message, at: 0 };
    let header = reader.bytes(12)?;
    let name = reader.name()?;
    let kind = reader.u16()?;
    Some((u16::from_be_bytes([header[0], header[1]]), name, kind))
}

/// A response to `request` carrying `answers`, as a test server sends it.
#[cfg(test)]
pub(crate) fn respond(request: &[u8], rcode: u8, answers: &[IpAddr], truncated: bool) -> Vec<u8> {
    let mut reader = Reader {
        message: request,
        at: 12,
    };
    reader.name().unwrap();
    let question_end = reader.at + 4;
    let mut message = request[..2].to_vec();
    let flags: u16 = 0x8180 | u16::from(rcode) | if truncated { 0x0200 } else { 0 };
    message.extend_from_slice(&flags.to_be_bytes());
    message.extend_from_slice(&[0, 1]);
    message.extend_from_slice(&(answers.len() as u16 + 1).to_be_bytes());
    message.extend_from_slice(&[0, 0, 0, 0]);
    message.extend_from_slice(&request[12..question_end]);
    // A CNAME first, pointing at the question name, to exercise skipping.
    message.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xC0, 12]);
    for answer in answers {
        message.extend_from_slice(&[0xC0, 12]);
        let (kind, data): (u16, Vec<u8>) = match answer {
            IpAddr::V4(v4) => (1, v4.octets().to_vec()),
            IpAddr::V6(v6) => (28, v6.octets().to_vec()),
        };
        message.extend_from_slice(&kind.to_be_bytes());
        message.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
        message.extend_from_slice(&(data.len() as u16).to_be_bytes());
        message.extend_from_slice(&data);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_has_one_question_of_the_asked_type() {
        let message = query(0x1234, "Page.Example.org.", RecordType::A).unwrap();
        assert_eq!(
            question(&message),
            Some((0x1234, "page.example.org".to_owned(), 1))
        );
        assert!(query(1, "", RecordType::A).is_none());
        assert!(query(1, "a..b", RecordType::A).is_none());
        assert!(query(1, &format!("{}.org", "a".repeat(64)), RecordType::A).is_none());
    }

    #[test]
    fn the_first_address_of_the_asked_type_is_returned() {
        let request = query(7, "page.example.org", RecordType::A).unwrap();
        let answers = ["8.8.4.4".parse().unwrap(), "1.1.1.1".parse().unwrap()];
        let response = respond(&request, 0, &answers, false);
        assert_eq!(
            parse(&response, Some(7), "page.example.org", RecordType::A),
            Ok(Answer::Address(answers[0]))
        );
        assert_eq!(
            parse(&response, Some(8), "page.example.org", RecordType::A),
            Err(Problem::Mismatch)
        );
        assert_eq!(
            parse(&response, Some(7), "other.example.org", RecordType::A),
            Err(Problem::Mismatch)
        );
        assert_eq!(
            parse(&response, Some(7), "page.example.org", RecordType::Aaaa),
            Err(Problem::Mismatch)
        );
    }

    #[test]
    fn negative_truncated_and_failed_answers_are_told_apart() {
        let request = query(9, "page.example.org", RecordType::A).unwrap();
        let name = "page.example.org";
        assert_eq!(
            parse(
                &respond(&request, 3, &[], false),
                Some(9),
                name,
                RecordType::A
            ),
            Ok(Answer::NoRecord)
        );
        assert_eq!(
            parse(
                &respond(&request, 0, &[], false),
                Some(9),
                name,
                RecordType::A
            ),
            Ok(Answer::NoRecord)
        );
        assert_eq!(
            parse(
                &respond(&request, 2, &[], false),
                Some(9),
                name,
                RecordType::A
            ),
            Err(Problem::ServerFailure)
        );
        assert_eq!(
            parse(
                &respond(&request, 0, &[], true),
                Some(9),
                name,
                RecordType::A
            ),
            Err(Problem::Truncated)
        );
    }

    #[test]
    fn hostile_messages_fail_without_panicking() {
        let request = query(1, "page.example.org", RecordType::A).unwrap();
        let good = respond(&request, 0, &["8.8.8.8".parse().unwrap()], false);
        for cut in 0..good.len() {
            let _ = parse(&good[..cut], Some(1), "page.example.org", RecordType::A);
        }
        // A pointer to itself, and a forward pointer.
        let mut looped = good[..12].to_vec();
        looped.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1]);
        assert_eq!(
            parse(&looped, Some(1), "page.example.org", RecordType::A),
            Err(Problem::Malformed)
        );
        // A count far larger than the message.
        let mut counted = good.clone();
        counted[6] = 0xFF;
        counted[7] = 0xFF;
        assert!(parse(&counted, Some(1), "page.example.org", RecordType::A).is_ok());
    }
}
