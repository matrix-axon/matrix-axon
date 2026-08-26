//! One message layout per change, instead of two per frame (issues #54, #52).
//!
//! [`message_layout`] runs an HTML parse and a rich-text wrap for *every* event
//! in the selected timeline, not just the visible page. It used to run twice per
//! frame: once in `ui::draw` for the rendered lines, and once in
//! `App::selected_message_ranges` for the nav ranges — each independently
//! rebuilding the same twelve arguments, including a copy-pasted
//! `image_thumb_rows` derivation. On a busy room that is hundreds of
//! parse-and-wrap operations per 100 ms tick, including idle ticks where
//! nothing changed at all.
//!
//! Both callers now read one cached [`MessageLayout`], recomputed only when the
//! digest of its inputs changes.
//!
//! # Why a digest rather than dirty flags
//!
//! ADR 0093 rejected scattered invalidation for `visible_room_indices`, on the
//! grounds that a stale room list is a visible correctness bug and the mutators
//! that would have to remember to invalidate are spread across six modules. A
//! stale *layout* is a scroll desync — the same class of bug — so it gets the
//! same self-validating treatment: hash what the layout reads, and a mutation
//! anybody forgets to announce still changes the hash.
//!
//! The exception is config-level input (colors, density, time format). Those
//! change only on a config reload, and hashing a whole `ColorScheme` on every
//! tick would cost more than it saves, so they are covered by
//! [`App::config_generation`] — one counter, bumped in one place.
//!
//! # Why the selection is *not* hashed
//!
//! It was, and that made every nav keypress a full O(events) re-parse and
//! re-wrap of the timeline — the same redundant work this cache exists to
//! remove, on a hotter path than the idle tick (#235).
//!
//! It no longer needs to be. `"> "` and `"  "` are the same width, so the
//! marker's contribution to `body_prefix_cols` — and therefore the wrap width
//! and `MessageLayout.ranges` — is identical either way. `message_layout` now
//! builds selection-neutral lines and `draw` restyles the selected message's
//! header line through `overlay_selection_on_page`, so one cached layout is
//! correct for *every* selection and moving it is a pure hit.
//!
//! `highlight_selected_line` left the layout at the same time and for the same
//! reason: it only ever decided how the selected line was styled, so it belongs
//! wherever the selection is applied.
//!
//! # Ordering
//!
//! Every map here is hashed through [`hash_sorted_map`]. `HashMap` iteration
//! order is unspecified and varies run to run, so hashing entries in iteration
//! order would produce a different digest for identical data and defeat the
//! cache. Sorting by key first is what makes the digest a function of the
//! content.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use uuid::Uuid;

use crate::api::EventDto;
use crate::app::render::{message_layout, ImageThumbRows, MessageLayout, RelationContext};

use super::App;

/// Feed a map into `state` in key order.
///
/// The length is hashed first so that `{a: 1}` and `{a: 1, b: 2}` cannot
/// collide through a prefix.
fn hash_sorted_map<K, V, S>(map: &HashMap<K, V, S>, state: &mut impl Hasher)
where
    K: Ord + Hash,
    V: Hash,
{
    let mut entries: Vec<(&K, &V)> = map.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    entries.len().hash(state);
    for (key, value) in entries {
        key.hash(state);
        value.hash(state);
    }
}

/// Everything [`message_layout`] reads, gathered once.
///
/// Bundled rather than passed as nine arguments, in the shape `RelationContext`
/// already uses a few lines up: the same values feed the digest and, on a miss,
/// the layout itself, so they belong together.
pub(crate) struct LayoutInputs<'a> {
    pub(crate) events: &'a [&'a EventDto],
    pub(crate) sender_labels: &'a [String],
    pub(crate) width: usize,
    pub(crate) reactions: &'a HashMap<String, Vec<(String, usize)>>,
    pub(crate) own_senders: &'a HashMap<Uuid, String>,
    pub(crate) image_thumb_rows: &'a ImageThumbRows,
    pub(crate) relations: &'a RelationContext,
    pub(crate) config_generation: u64,
}

/// Reduce the layout inputs to one value.
///
/// A miss costs a full re-layout, so this errs towards over-hashing: it is
/// cheaper to recompute a layout that did not need it than to render lines that
/// disagree with the ranges the nav math is using.
fn layout_digest(inputs: &LayoutInputs<'_>) -> u64 {
    let LayoutInputs {
        events,
        sender_labels,
        width,
        reactions,
        own_senders,
        image_thumb_rows,
        relations,
        config_generation,
    } = inputs;
    let mut hasher = DefaultHasher::new();

    events.len().hash(&mut hasher);
    for event in events.iter() {
        // Hash what the renderer *projects*, through the same accessors it
        // uses, rather than a hand-picked subset of raw fields. Two reasons:
        //
        // - `display_body` and `membership_change` derive from `content`, a
        //   `serde_json::Value` that is not `Hash` (it can hold an f64). Going
        //   through the projections covers them without hashing the whole
        //   value.
        // - It makes this list checkable. It should match `render.rs`'s own
        //   `event.*` reads exactly; if the renderer starts reading a new
        //   field, that grep stops agreeing with this block. Over-hashing is
        //   the safe direction — a spurious miss costs one re-layout, a missed
        //   input renders lines that disagree with the ranges nav measures.
        event.event_id.hash(&mut hasher);
        event.account_id.hash(&mut hasher);
        event.sender.hash(&mut hasher);
        event.origin_ts.hash(&mut hasher);
        event.redacted.hash(&mut hasher);
        // Rendered as a glyph that reserves two columns, so it feeds
        // `body_prefix_cols` and therefore the wrap width. A stale verdict
        // leaves the safety glyph wrong *and* wraps against the wrong width,
        // corrupting the ranges nav and scrolling measure against (#229).
        event.sender_trust.hash(&mut hasher);
        event.display_body().hash(&mut hasher);
        event.formatted_body().hash(&mut hasher);
        event.image_mxc().hash(&mut hasher);
        event.membership_change().hash(&mut hasher);
    }
    sender_labels.hash(&mut hasher);
    width.hash(&mut hasher);
    hash_sorted_map(reactions, &mut hasher);
    hash_sorted_map(own_senders, &mut hasher);
    hash_sorted_map(image_thumb_rows, &mut hasher);

    // RelationContext holds maps, so it cannot derive Hash. Its leaf types do,
    // which is what keeps a newly added ReplyPreview/ThreadBadge field covered
    // without an edit here. A new *map* on RelationContext does need adding.
    hash_sorted_map(&relations.replies, &mut hasher);
    hash_sorted_map(&relations.thread_badges, &mut hasher);
    hash_sorted_map(&relations.thread_contexts, &mut hasher);
    relations.thread_root.hash(&mut hasher);

    config_generation.hash(&mut hasher);
    hasher.finish()
}

impl App {
    /// Recompute the message layout if anything it reads has changed.
    ///
    /// Call this in the update step, before anything reads
    /// the cached ranges or lines. It is idempotent and cheap
    /// on a hit — one pass over the inputs to hash them.
    pub(crate) fn ensure_message_layout(&mut self) {
        self.messages.layout_checks = self.messages.layout_checks.saturating_add(1);
        // Derive once and hold the result: on a miss the same values feed the
        // layout, so a miss must not pay for deriving them twice.
        let recomputed = {
            let events = self.selected_events();
            let sender_labels = self.sender_labels(events.as_slice());
            let reactions = self.selected_reactions();
            let image_thumb_rows = self.image_thumb_rows(events.as_slice());
            let relations = self.relation_context(events.as_slice());
            let inputs = LayoutInputs {
                events: events.as_slice(),
                sender_labels: sender_labels.as_slice(),
                width: self.messages.width,
                reactions: &reactions,
                own_senders: &self.live.own_senders,
                image_thumb_rows: &image_thumb_rows,
                relations: &relations,
                config_generation: self.config_generation,
            };
            let key = layout_digest(&inputs);
            if self.messages.layout_key == Some(key) {
                None
            } else {
                let layout = message_layout(
                    inputs.events,
                    inputs.sender_labels,
                    &self.colors,
                    inputs.width,
                    inputs.reactions,
                    inputs.own_senders,
                    inputs.image_thumb_rows,
                    inputs.relations,
                    self.display.message_density,
                    self.display.time_format,
                );
                // Kept alongside the layout it was built from, so `draw` reads
                // it instead of rederiving the same O(events) filter+map every
                // frame — which it did unconditionally, hit or miss.
                Some((key, layout, image_thumb_rows))
            }
        };

        if let Some((key, layout, image_thumb_rows)) = recomputed {
            self.messages.layout_recomputes = self.messages.layout_recomputes.saturating_add(1);
            self.messages.layout_key = Some(key);
            self.messages.layout = Some(layout);
            self.messages.layout_image_thumb_rows = image_thumb_rows;
        }
        // Always, not just on a miss. `set_message_layout` used to run on every
        // frame, so any code reading `messages.scroll` between frames saw a
        // resolved offset. Scroll is deliberately not part of the digest — the
        // layout does not depend on it — so a cache hit must still settle the
        // pin-to-bottom sentinel, or a `scroll = usize::MAX` set after the last
        // recompute would survive into the next keypress.
        self.resolve_message_scroll();
    }

    /// Settle the pin-to-bottom sentinel and clamp the scroll offset against
    /// the current ranges.
    ///
    /// Same arithmetic the old `set_message_layout` performed; split out so it
    /// can run on a cache hit, where no layout is recomputed.
    fn resolve_message_scroll(&mut self) {
        let line_count = self
            .cached_message_ranges()
            .last()
            .map(|range| range.end)
            .unwrap_or_default();
        let max_scroll = line_count.saturating_sub(self.messages.page_size);
        self.messages.scroll = if self.messages.scroll == usize::MAX {
            max_scroll
        } else {
            self.messages.scroll.min(max_scroll)
        };
    }

    /// The cached layout's ranges, or an empty slice before the first layout.
    ///
    /// Nav math and scroll clamping read this rather than a cloned copy: one
    /// `Vec<Range>` per recompute existed only to keep a second field in sync
    /// with the first.
    pub(crate) fn cached_message_ranges(&self) -> &[std::ops::Range<usize>] {
        self.messages
            .layout
            .as_ref()
            .map(|layout| layout.ranges.as_slice())
            .unwrap_or_default()
    }

    /// The cached layout, if one has been computed for the current inputs.
    pub(crate) fn cached_message_layout(&self) -> Option<&MessageLayout> {
        self.messages.layout.as_ref()
    }
}
