//! Bounded PEM load shared by Apple sign-in and the push providers.
//!
//! One of an inline PEM or a path. A path is opened as a regular file of at
//! most 16 KiB, owner-only on Unix, without blocking. Errors name the
//! operation. They do not include the key bytes or the path.

use std::path::Path;
use std::time::Duration;

const MAX_KEY_BYTES: usize = 16 * 1024;

/// One PEM source. An empty inline string or an empty path is set, but it is
/// not a usable source. Together with the other field it is still "both".
/// Apple uses that rule. Push resolves emptiness in its own config check.
pub(crate) enum PrivateKeySource<'a> {
    Inline(&'a str),
    Path(&'a Path),
}

/// Require exactly one of an inline PEM or a path.
///
/// `label` is a short name such as `"Apple"` and is the only caller-controlled
/// text that appears in an error. An empty `private_key` or an empty
/// `private_key_path` still counts as that field being set.
pub(crate) fn one_private_key<'a>(
    label: &str,
    inline: Option<&'a str>,
    path: Option<&'a Path>,
) -> anyhow::Result<PrivateKeySource<'a>> {
    match (inline, path) {
        (Some(pem), None) if !pem.is_empty() => Ok(PrivateKeySource::Inline(pem)),
        (None, Some(path)) if !path.as_os_str().is_empty() => Ok(PrivateKeySource::Path(path)),
        _ => anyhow::bail!(
            "set exactly one of {label} private_key (at most 16 KiB) or private_key_path"
        ),
    }
}

/// Read `source`. A path is opened as a regular file of at most 16 KiB,
/// owner-only on Unix, without blocking. Errors name the operation. They do
/// not include the key bytes or the path.
pub(crate) async fn load_private_key(
    label: &str,
    source: &PrivateKeySource<'_>,
) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    Ok(match source {
        PrivateKeySource::Inline(pem) if pem.len() > MAX_KEY_BYTES => {
            anyhow::bail!("{label} private_key must be at most 16 KiB");
        }
        PrivateKeySource::Inline(pem) if !pem.is_empty() => pem.as_bytes().to_vec(),
        PrivateKeySource::Path(path) => tokio::time::timeout(Duration::from_secs(5), async {
            let metadata = tokio::fs::metadata(path).await.map_err(|error| {
                key_file_error(label, "inspect", path.is_relative(), error.kind())
            })?;
            anyhow::ensure!(
                metadata.is_file() && metadata.len() <= MAX_KEY_BYTES as u64,
                "{label} key must be a regular file of at most 16 KiB"
            );
            let mut options = tokio::fs::OpenOptions::new();
            options.read(true);
            // A path swapped to a FIFO after inspection must not strand a
            // blocking filesystem worker even after the async timeout fires.
            #[cfg(unix)]
            options.custom_flags(libc::O_NONBLOCK);
            let file = options
                .open(path)
                .await
                .map_err(|error| key_file_error(label, "read", path.is_relative(), error.kind()))?;
            let metadata = file.metadata().await.map_err(|error| {
                key_file_error(label, "inspect", path.is_relative(), error.kind())
            })?;
            anyhow::ensure!(metadata.is_file(), "{label} key must be a regular file");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                anyhow::ensure!(
                    metadata.permissions().mode() & 0o077 == 0,
                    "{label} key file must have owner-only permissions (chmod 600)"
                );
            }
            let mut bytes = Vec::new();
            file.take((MAX_KEY_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| key_file_error(label, "read", path.is_relative(), error.kind()))?;
            anyhow::ensure!(
                !bytes.is_empty() && bytes.len() <= MAX_KEY_BYTES,
                "{label} key file must contain at most 16 KiB of PEM"
            );
            Ok::<_, anyhow::Error>(bytes)
        })
        .await
        .map_err(|_| anyhow::anyhow!("{label} key file read timed out"))??,
        PrivateKeySource::Inline(_) => anyhow::bail!(
            "set exactly one of {label} private_key (at most 16 KiB) or private_key_path"
        ),
    })
}

fn key_file_error(
    label: &str,
    operation: &'static str,
    relative: bool,
    kind: std::io::ErrorKind,
) -> anyhow::Error {
    let hint = if relative {
        "private_key_path is relative to the process working directory; use an absolute path"
    } else {
        "check private_key_path and file permissions"
    };
    anyhow::anyhow!("cannot {operation} {label} key file ({kind:?}); {hint}")
}
