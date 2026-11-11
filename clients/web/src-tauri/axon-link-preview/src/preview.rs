//! One lookup: the page, then its preview image, inside one 25-second
//! deadline. Ported from `RestrictedPreview.fetch` and the parts of
//! `DevicePreview.include` that do not touch the share store.

use crate::error::{Failure, Kind, LookupError};
use crate::http::{Kind as Resource, THUMBNAIL_BYTES};
use crate::image::{self, Thumbnail};
use crate::net::{self, Hop};
use crate::page::{self, preview_field, DESCRIPTION_LIMIT, SITE_NAME_LIMIT, TITLE_LIMIT};
use crate::url_check;
use crate::Cancel;
use percent_encoding::percent_decode_str;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::Instant;

/// One deadline covers the page, its redirects and its image.
pub(crate) const DEADLINE: Duration = Duration::from_secs(25);

/// At most this many images are decoded at once, counting decodes a
/// cancelled or expired lookup stopped waiting for.
const NORMALIZING: usize = 2;

type Normalize = dyn Fn(&[u8]) -> Result<Thumbnail, Failure> + Send + Sync;

/// Decodes preview images on blocking threads, within a budget of its own.
///
/// A lookup waits for its image only until its deadline or its cancellation,
/// but a decode already running cannot be stopped. Each decode holds a slot
/// until it really ends, so abandoned decodes can never occupy more than
/// [`NORMALIZING`] threads, and a lookup never holds its `running` permit
/// while waiting for one.
pub(crate) struct Normalizer {
    slots: Arc<Semaphore>,
    run: Arc<Normalize>,
}

impl Normalizer {
    pub(crate) fn new() -> Self {
        Normalizer::with(Arc::new(image::normalize))
    }

    pub(crate) fn with(run: Arc<Normalize>) -> Self {
        Normalizer {
            slots: Arc::new(Semaphore::new(NORMALIZING)),
            run,
        }
    }

    /// The thumbnail, or why there is none. `Expired` and `Cancelled` mean
    /// this stopped waiting; the decode may still finish on its own.
    async fn normalize(
        &self,
        bytes: Vec<u8>,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Thumbnail, Failure> {
        let slots = self.slots.clone();
        let run = self.run.clone();
        let work = async move {
            let slot = slots
                .acquire_owned()
                .await
                .map_err(|_| Failure::Unavailable)?;
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                run(&bytes)
            })
            .await
            .unwrap_or(Err(Failure::Invalid))
        };
        tokio::select! {
            result = tokio::time::timeout_at(deadline, work) => {
                result.unwrap_or(Err(Failure::Expired))
            }
            () = cancel.cancelled() => Err(Failure::Cancelled),
        }
    }
}

/// What a lookup found, every text field already checked against the send
/// contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Preview {
    /// Where the page was finally read from, after redirects.
    pub(crate) url: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) site_name: Option<String>,
    pub(crate) image: Option<Thumbnail>,
    /// The page named an image, and it could not be fetched or decoded.
    pub(crate) image_unavailable: bool,
}

/// The page and its image as fetched, before the image is normalized:
/// `RestrictedPreview.Metadata`.
#[derive(Debug, Default)]
pub(crate) struct Fetched {
    pub(crate) url: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) site_name: Option<String>,
    pub(crate) image: Option<Vec<u8>>,
    pub(crate) image_unavailable: bool,
}

/// `URL.lastPathComponent.removingPercentEncoding`, for a link that is
/// itself an image.
fn last_path_component(url: &url::Url) -> Option<String> {
    let path = url.path();
    let trimmed = path.trim_end_matches('/');
    let component = if trimmed.is_empty() {
        // Foundation's answer for the root path.
        "/"
    } else {
        trimmed.rsplit('/').next().unwrap_or_default()
    };
    percent_decode_str(component)
        .decode_utf8()
        .ok()
        .map(|text| text.into_owned())
}

/// `RestrictedPreview.fetch`, after a lookup slot is held.
pub(crate) async fn fetch<H: Hop>(
    hop: &H,
    url: &str,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Fetched, Failure> {
    let response = net::fetch(hop, url, Resource::Page, deadline, cancel).await?;
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    let content_type = response.content_type();
    if content_type
        .as_deref()
        .is_some_and(|kind| kind.starts_with("image/"))
    {
        return Ok(Fetched {
            url: response.url.to_string(),
            title: preview_field(last_path_component(&response.url).as_deref(), TITLE_LIMIT),
            image: Some(response.body),
            ..Fetched::default()
        });
    }
    let content_type = content_type.unwrap_or_else(|| "text/html".to_owned());
    if !content_type.starts_with("text/html") && !content_type.starts_with("application/xhtml+xml")
    {
        return Err(Failure::PreviewUnavailable);
    }
    let page = page::parse(&response.body, &content_type)?;
    let mut fetched = Fetched {
        url: response.url.to_string(),
        title: page.title,
        description: page.description,
        site_name: page.site_name,
        ..Fetched::default()
    };
    if let Some(target) = page.image.and_then(|image| response.url.join(&image).ok()) {
        fetched.image_unavailable = true;
        match net::fetch(hop, target.as_str(), Resource::Image, deadline, cancel).await {
            Ok(image) if image.body.len() <= THUMBNAIL_BYTES => {
                fetched.image = Some(image.body);
                fetched.image_unavailable = false;
            }
            Ok(_) | Err(_) => {
                if cancel.is_cancelled() {
                    return Err(Failure::Cancelled);
                }
                crate::log::event("device_preview_image_unavailable");
            }
        }
    }
    if fetched.title.is_none()
        && fetched.description.is_none()
        && fetched.site_name.is_none()
        && fetched.image.is_none()
    {
        return Err(Failure::PreviewUnavailable);
    }
    crate::log::event(&format!(
        "device_preview_fetched retained_bytes={}",
        fetched.image.as_ref().map_or(0, Vec::len)
    ));
    Ok(fetched)
}

/// A whole lookup, from the link the user wrote to what the page receives:
/// the static check first, then [`fetch`], then the image is normalized and
/// every field checked again, as `DevicePreview.include` does.
///
/// The image is decoded inside the same deadline and cancellation as the
/// fetch. A deadline that passes while it decodes leaves a card with its text
/// and no image, as an image fetch that runs out of time does; with no text
/// either, the lookup has timed out.
pub(crate) async fn lookup<H: Hop>(
    hop: &H,
    url: &str,
    cancel: &Cancel,
    normalizer: &Normalizer,
) -> Result<Preview, LookupError> {
    if url_check::validate(url).is_err() {
        return Err(Kind::UnsupportedUrl.into());
    }
    let deadline = Instant::now() + DEADLINE;
    let work = fetch(hop, url, deadline, cancel);
    let fetched = tokio::select! {
        result = tokio::time::timeout_at(deadline, work) => match result {
            Ok(result) => result?,
            Err(_) => return Err(Failure::Expired.into()),
        },
        () = cancel.cancelled() => return Err(Failure::Cancelled.into()),
    };
    let mut preview = Preview {
        url: fetched.url,
        title: preview_field(fetched.title.as_deref(), TITLE_LIMIT),
        description: preview_field(fetched.description.as_deref(), DESCRIPTION_LIMIT),
        site_name: preview_field(fetched.site_name.as_deref(), SITE_NAME_LIMIT),
        image: None,
        image_unavailable: fetched.image_unavailable,
    };
    let mut expired = false;
    if let Some(bytes) = fetched.image {
        match normalizer.normalize(bytes, deadline, cancel).await {
            Ok(thumbnail) => {
                crate::log::event(&format!(
                    "device_preview_image retained_bytes={}",
                    thumbnail.bytes.len()
                ));
                preview.image = Some(thumbnail);
            }
            Err(Failure::Cancelled) => return Err(Failure::Cancelled.into()),
            Err(failure) => {
                expired = failure == Failure::Expired;
                crate::log::event(&format!(
                    "device_preview_image_unavailable category={failure:?}"
                ));
                preview.image_unavailable = true;
            }
        }
    }
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled.into());
    }
    if preview.image.is_none()
        && preview.title.is_none()
        && preview.description.is_none()
        && preview.site_name.is_none()
    {
        return Err(if expired {
            Kind::Timeout
        } else {
            Kind::NoMetadata
        }
        .into());
    }
    crate::log::event(&format!(
        "device_preview_included image={}",
        if preview.image.is_some() {
            "ready"
        } else if preview.image_unavailable {
            "unavailable"
        } else {
            "absent"
        }
    ));
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_link_is_titled_by_its_file_name() {
        let title = |url: &str| last_path_component(&url::Url::parse(url).unwrap());
        assert_eq!(
            title("https://h.org/a/Photo%20One.png").as_deref(),
            Some("Photo One.png")
        );
        assert_eq!(title("https://h.org/a/").as_deref(), Some("a"));
        assert_eq!(title("https://h.org/").as_deref(), Some("/"));
        assert_eq!(title("https://h.org/%FF").as_deref(), None);
    }
}
