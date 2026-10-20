//! Media proxy with bounded LRU disk cache for MXC URLs.
//!
//! This crate provides MXC URI validation ([`parse_mxc`]) and the bounded
//! on-disk LRU [`MediaCache`]. The actual authenticated, decrypting download is
//! done through the `axon-sync` SDK client (which carries the account's access
//! token) behind the [`MediaFetcher`] trait, so this crate stays free of
//! `matrix-sdk`; `axon-server` wires the cache in front of the fetcher and
//! adapts the pair onto the `axon-api` `MediaProxy` port.

mod cache;

pub use cache::{
    etag_for, etag_for_thumbnail, FetchError, MediaCache, MediaCacheError, MediaCacheHandle,
    MediaFetcher, MediaResource,
};

pub use axon_core::media::parse_mxc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_mxc_uri() {
        let (server, media_id) = parse_mxc("mxc://matrix.org/abc123XYZ").unwrap();
        assert_eq!(server, "matrix.org");
        assert_eq!(media_id, "abc123XYZ");
    }

    #[test]
    fn rejects_non_mxc_scheme() {
        assert!(parse_mxc("https://matrix.org/abc123").is_none());
    }

    #[test]
    fn rejects_empty_server() {
        assert!(parse_mxc("mxc:///abc123").is_none());
    }

    #[test]
    fn rejects_empty_media_id() {
        assert!(parse_mxc("mxc://matrix.org/").is_none());
    }

    #[test]
    fn rejects_media_id_with_slash() {
        assert!(parse_mxc("mxc://matrix.org/abc/def").is_none());
    }
}
