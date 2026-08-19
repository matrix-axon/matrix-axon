//! Source-level contract guards for handwritten smoke wire types.
//!
//! The smoke package cannot depend on `axon-tui`, because doing so would cross
//! the black-box boundary this harness exists to test. Parsing the two source
//! declarations keeps that boundary intact while making a newly required TUI
//! field fail during tests instead of silently emptying the rendered timeline.

use std::collections::BTreeSet;

use syn::{Attribute, Expr, Fields, Item, LitStr, Type};

const SMOKE_WIRE: &str = include_str!("../src/wire.rs");
const TUI_API: &str = include_str!("../../../clients/tui/src/api.rs");

#[test]
fn smoke_event_dto_covers_tui_required_fields() {
    let smoke_fields = event_fields(SMOKE_WIRE, false);
    let tui_required_fields = event_fields(TUI_API, true);
    let missing = tui_required_fields
        .difference(&smoke_fields)
        .cloned()
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "smoke EventDto is missing required axon-tui wire field(s): {}",
        missing.join(", ")
    );
}

fn event_fields(source: &str, required_only: bool) -> BTreeSet<String> {
    let file = syn::parse_file(source).expect("Rust source must parse");
    let event = file
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == "EventDto" => Some(item),
            _ => None,
        })
        .expect("source must declare EventDto");
    let Fields::Named(fields) = &event.fields else {
        panic!("EventDto must use named fields");
    };

    fields
        .named
        .iter()
        .filter(|field| {
            !required_only || (!is_option(&field.ty) && !has_serde_default(&field.attrs))
        })
        .map(|field| {
            serde_rename(&field.attrs).unwrap_or_else(|| {
                field
                    .ident
                    .as_ref()
                    .expect("named field must have an identifier")
                    .to_string()
            })
        })
        .collect()
}

fn is_option(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == "Option")
    )
}

fn has_serde_default(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("serde") {
            return false;
        }
        let mut found = false;
        attr.parse_nested_meta(|meta| {
            found |= meta.path.is_ident("default");
            if meta.input.peek(syn::Token![=]) {
                let _: Expr = meta.value()?.parse()?;
            }
            Ok(())
        })
        .expect("serde attribute must parse");
        found
    })
}

fn serde_rename(attrs: &[Attribute]) -> Option<String> {
    let mut rename = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                rename = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.input.peek(syn::Token![=]) {
                let _: Expr = meta.value()?.parse()?;
            }
            Ok(())
        })
        .expect("serde attribute must parse");
    }
    rename
}
