//! Matrix links (ADR 0138): links whose card the recipient's client builds
//! from its own access, so a sender never authors one.

/// Whether `url` is a Matrix link (ADR 0138 section 1): any link whose origin
/// is `https://matrix.to`, and any `matrix:` URI, whatever it points at. The
/// same test as `isMatrixLink` in `clients/web/src/matrix-to.ts`.
pub fn is_matrix_link(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "matrix"
            || (u.scheme() == "https" && u.host_str() == Some("matrix.to") && u.port().is_none())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_links_are_matrix_to_origin_and_matrix_uris() {
        for url in [
            "https://matrix.to/#/!room:example.org/$event",
            "https://matrix.to/#/#alias:example.org",
            "https://matrix.to/#/@alice:example.org",
            "https://MATRIX.to/#/!room:example.org",
            "https://matrix.to:443/#/!room:example.org",
            "matrix:roomid/room:example.org/e/event",
            "matrix:u/alice:example.org",
        ] {
            assert!(is_matrix_link(url), "{url}");
        }
        for url in [
            "http://matrix.to/#/!room:example.org",
            "https://matrix.to:8443/#/!room:example.org",
            "https://www.matrix.to/#/!room:example.org",
            "https://matrix.org/",
            "https://example.org/matrix.to",
            "not a url",
        ] {
            assert!(!is_matrix_link(url), "{url}");
        }
    }
}
