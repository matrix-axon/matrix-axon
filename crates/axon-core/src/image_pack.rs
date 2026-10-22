//! Bounded, SDK-independent decoding of Matrix image packs.
//! Only images usable as emoticons are exposed to reaction pickers.

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

fn emoticon_usage(value: Option<&Value>) -> Option<bool> {
    match value {
        None => Some(true),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
            Some(items.is_empty() || items.iter().any(|v| v.as_str() == Some("emoticon")))
        }
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
        match emoticon_usage(image.get("usage").or(pack_usage)) {
            Some(false) => continue,
            None => {
                pack.partial = true;
                continue;
            }
            Some(true) => {}
        }
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
        pack.images.push(PackImage {
            shortcode: name.to_owned(),
            url,
            body: text(image.get("body"), 1024),
        });
    }
    Some(pack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(pack.images.len(), 1);
        assert_eq!(pack.images[0].shortcode, "emote");
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
