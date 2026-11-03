//! Bounded reads of HTTP response bodies sent outside the SDK, which buffers
//! and parses a whole body before exposing it.

/// Why a bounded read stopped short.
pub(crate) enum BoundedBodyError {
    TooLarge,
    Transport(reqwest::Error),
}

/// Read a response body, refusing one larger than `max_bytes` before it is
/// buffered or parsed. Shared by every Matrix OAuth response and by
/// room-detail acquisition (ADR 0126).
pub(crate) async fn read_bounded_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, BoundedBodyError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(BoundedBodyError::TooLarge);
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(max_bytes as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(BoundedBodyError::Transport)?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(BoundedBodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
