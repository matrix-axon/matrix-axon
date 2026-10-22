//! Bounded, SDK-independent decoding of Matrix image packs.
//! Emoji and sticker usage is retained independently for each image.

use serde_json::Value;

pub const ROOM_PACK_TYPES: [&str; 2] = ["m.room.image_pack", "im.ponies.room_emotes"];
pub const PERSONAL_PACK_TYPE: &str = "im.ponies.user_emotes";
pub const MAX_PACK_BYTES: usize = 131_072;
pub const MAX_PACK_IMAGES: usize = 512;
pub const MAX_ROOM_PACKS: usize = 64;
/// Non-control ECMAScript String.trim() whitespace, shared with SQL aggregates.
pub const SHORTCODE_TRIM_CHARS: &str = concat!(
    " \u{00a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}",
    "\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}",
);

#[derive(Debug, Clone)]
pub struct ImagePack {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub images: Vec<PackImage>,
    /// Malformed entries were skipped, or a resource limit was reached.
    pub partial: bool,
}

#[derive(Debug, Clone)]
pub struct PackImage {
    pub shortcode: String,
    pub url: String,
    pub body: Option<String>,
    pub emoticon: bool,
    pub sticker: bool,
    pub info: Option<Value>,
}

/// A standalone sticker send. `info` is a bounded ImageInfo object, not
/// arbitrary caller-controlled event content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sticker {
    pub body: String,
    pub url: String,
    pub info: Value,
}

impl Sticker {
    pub fn validate(&self) -> bool {
        self.body.len() <= 1024
            && !self.body.trim().is_empty()
            && !self.body.chars().any(char::is_control)
            && crate::media::parse_mxc(&self.url).is_some()
            && sticker_info(&self.info).is_some()
    }
}

/// Project only bounded Matrix ImageInfo fields. No remote thumbnail URLs,
/// encryption descriptors, or arbitrary extensions are copied into a send.
pub fn sticker_info(value: &Value) -> Option<Value> {
    let source = value.as_object()?;
    let mut info = serde_json::Map::new();
    for (key, limit) in [("w", 16384), ("h", 16384), ("size", 104857600)] {
        if let Some(value) = source.get(key) {
            let number = value.as_u64()?;
            if number > limit || (key != "size" && number == 0) {
                return None;
            }
            info.insert(key.into(), value.clone());
        }
    }
    if let Some(value) = source.get("mimetype") {
        if !matches!(
            value.as_str()?,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            return None;
        }
        info.insert("mimetype".into(), value.clone());
    }
    if let Some(value) = source.get("is_animated") {
        value.as_bool()?;
        info.insert("is_animated".into(), value.clone());
    }
    Some(Value::Object(info))
}

/// Same display-name bounds as reaction aggregates; names remain untrusted text.
pub fn reaction_shortcode(value: &str) -> Option<&str> {
    if value.is_empty() || value.len() > 100 || value.chars().any(char::is_control) {
        return None;
    }
    let trimmed = value.trim_matches(|c| SHORTCODE_TRIM_CHARS.contains(c));
    (!trimmed.is_empty()).then_some(trimmed)
}

fn text(value: Option<&Value>, limit: usize) -> Option<String> {
    let s = value?.as_str()?;
    (s.len() <= limit && !s.chars().any(char::is_control)).then(|| s.to_owned())
}

fn mxc(value: Option<&Value>) -> Option<String> {
    let s = value?.as_str()?;
    crate::media::parse_mxc(s).map(|_| s.to_owned())
}

fn image_usage(value: Option<&Value>) -> Option<(bool, bool)> {
    match value {
        None => Some((true, true)),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => Some((
            items.is_empty() || items.iter().any(|v| v.as_str() == Some("emoticon")),
            items.is_empty() || items.iter().any(|v| v.as_str() == Some("sticker")),
        )),
        _ => None,
    }
}

/// The caller must enforce MAX_PACK_BYTES before transferring/decoding JSON.
/// Unknown fields are discarded. Invalid images cannot authorize media fetches.
pub fn parse_image_pack(content: &Value) -> Option<ImagePack> {
    // An empty object is the protocol's normal way to clear a pack.
    let empty = serde_json::Map::new();
    let images = if content.as_object().is_some_and(|object| object.is_empty()) {
        &empty
    } else {
        content.get("images")?.as_object()?
    };
    let mut names = std::collections::HashSet::new();
    let metadata = content.get("pack");
    let pack_usage = metadata.and_then(|p| p.get("usage"));
    let mut pack = ImagePack {
        display_name: text(metadata.and_then(|p| p.get("display_name")), 256),
        avatar_url: mxc(metadata.and_then(|p| p.get("avatar_url"))),
        images: Vec::new(),
        partial: images.len() > MAX_PACK_IMAGES,
    };
    // serde_json's default object representation orders keys deterministically.
    // Bound entries examined as well as entries returned, including bad entries.
    for (name, image) in images.iter().take(MAX_PACK_IMAGES) {
        let Some(name) = reaction_shortcode(name) else {
            pack.partial = true;
            continue;
        };
        let Some(image) = image.as_object() else {
            pack.partial = true;
            continue;
        };
        let (emoticon, sticker) = match image_usage(image.get("usage").or(pack_usage)) {
            Some((false, false)) => continue,
            Some(usage) => usage,
            None => {
                pack.partial = true;
                continue;
            }
        };
        let Some(url) = mxc(image.get("url")) else {
            pack.partial = true;
            continue;
        };
        // Keep the first usable image in deterministic source-key order.
        // Never return ambiguous normalized keys within one pack.
        if !names.insert(name) {
            pack.partial = true;
            continue;
        }
        let info = image.get("info").and_then(sticker_info);
        if image.get("info").is_some() && info.is_none() {
            pack.partial = true;
        }
        pack.images.push(PackImage {
            shortcode: name.to_owned(),
            url,
            body: text(image.get("body"), 1024),
            emoticon,
            sticker,
            info,
        });
    }
    Some(pack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sticker_metadata_is_optional_bounded_and_projected() {
        assert_eq!(
            sticker_info(&json!({"w":278,"h":185,"mimetype":"image/png","unknown":"ignored"})),
            Some(json!({"w":278,"h":185,"mimetype":"image/png"}))
        );
        for invalid in [
            json!({"w":0}),
            json!({"h":16385}),
            json!({"size":104857601}),
            json!({"mimetype":"text/html"}),
            json!({"is_animated":"yes"}),
        ] {
            assert!(sticker_info(&invalid).is_none());
        }
        let pack = parse_image_pack(&json!({"images":{"wave":{"url":"mxc://hs/wave"}}})).unwrap();
        assert!(pack.images[0].emoticon && pack.images[0].sticker);
        assert!(pack.images[0].info.is_none());
        let sticker = Sticker {
            body: "wave".into(),
            url: "mxc://hs/wave".into(),
            info: json!({}),
        };
        assert!(sticker.validate());
    }

    #[test]
    fn parses_personal_pack_shape_seen_in_production() {
        let pack = parse_image_pack(&json!({
            "pack": {"display_name": "Adam"},
            "images": {
                "adam": {"body": "adam", "url": "mxc://example.org/adam", "usage": ["sticker", "emoticon"]},
                "allthethings": {"url": "mxc://example.org/allthethings"},
                "iswydt": {"url": "mxc://example.org/iswydt", "info": {"h": 229, "w": 256}}
            }
        })).unwrap();
        assert_eq!(pack.display_name.as_deref(), Some("Adam"));
        assert_eq!(
            pack.images
                .iter()
                .map(|i| i.shortcode.as_str())
                .collect::<Vec<_>>(),
            ["adam", "allthethings", "iswydt"]
        );
        assert!(!pack.partial);
    }

    #[test]
    fn usage_overrides_and_invalid_entries_are_isolated() {
        let pack = parse_image_pack(&json!({
            "pack": {"usage": ["sticker"]},
            "images": {
                "sticker": {"url": "mxc://example.org/sticker"},
                "emote": {"url": "mxc://example.org/emote", "usage": ["emoticon"]},
                "bad-url": {"url": "https://example.org/tracker", "usage": []},
                "bad-usage": {"url": "mxc://example.org/x", "usage": 1},
                "bad-shape": []
            }
        }))
        .unwrap();
        assert_eq!(pack.images.len(), 2);
        assert_eq!(pack.images[0].shortcode, "emote");
        assert!(pack.images[0].emoticon);
        assert!(!pack.images[0].sticker);
        assert!(!pack.images[1].emoticon);
        assert!(pack.images[1].sticker);
        assert!(pack.partial);
        assert!(parse_image_pack(&json!({"images": []})).is_none());
    }

    #[test]
    fn limits_names_and_entries_before_building_output() {
        assert_eq!(reaction_shortcode("\u{00a0}adam\u{feff}"), Some("adam"));
        assert!(reaction_shortcode("bad\u{85}name").is_none());
        assert!(reaction_shortcode(&"é".repeat(51)).is_none());
        let images = (0..MAX_PACK_IMAGES + 1)
            .map(|i| {
                (
                    format!("image{i:04}"),
                    json!({"url": "mxc://example.org/x"}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let pack = parse_image_pack(&json!({"images": images})).unwrap();
        assert!(pack.partial);
        assert_eq!(pack.images.len(), MAX_PACK_IMAGES);
    }

    #[test]
    fn clears_and_normalized_name_collisions() {
        let cleared = parse_image_pack(&json!({})).unwrap();
        assert!(cleared.images.is_empty());
        assert!(!cleared.partial);
        assert!(parse_image_pack(&json!({"pack": {}})).is_none());
        let pack = parse_image_pack(&json!({"images": {
            "wave": {"url": "mxc://example.org/first"},
            "\u{a0}wave": {"url": "mxc://example.org/second"}
        }}))
        .unwrap();
        assert!(pack.partial);
        assert_eq!(pack.images.len(), 1);
        assert_eq!(pack.images[0].shortcode, "wave");
        assert_eq!(pack.images[0].url, "mxc://example.org/first");
    }
}
