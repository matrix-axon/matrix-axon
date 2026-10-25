//! Golden-file test for the emitted OpenAPI spec.
//!
//! The spec is the source of truth for generated clients, so handler/spec drift
//! must be a failing test rather than a silent mismatch. This test serializes
//! the utoipa-built document and compares it to the checked-in
//! `openapi/openapi.json`. It needs no database.
//!
//! Regenerate after an intentional API change:
//!
//! ```sh
//! UPDATE_OPENAPI=1 cargo test -p axon-api --test openapi
//! ```

use std::path::PathBuf;

use axon_api::ApiDoc;
use utoipa::OpenApi;

#[test]
fn bundled_preview_contract_is_additive_and_rejects_arbitrary_metadata() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let schemas = &spec["components"]["schemas"];
    assert_eq!(schemas["LinkPreviewRequest"]["additionalProperties"], false);
    assert_eq!(
        schemas["LinkPreviewRequest"]["required"],
        serde_json::json!(["matched_url"])
    );
    for (dto, required) in [
        ("SendMessageRequest", "body"),
        ("SendMediaRequest", "upload_id"),
    ] {
        assert_eq!(schemas[dto]["required"], serde_json::json!([required]));
        assert_eq!(schemas[dto]["properties"]["url_previews"]["maxItems"], 8);
    }
    for path in [
        "/v1/accounts/{account_id}/rooms/{room_id}/send",
        "/v1/accounts/{account_id}/rooms/{room_id}/send-media",
    ] {
        assert!(spec["paths"][path]["post"]["responses"]
            .get("413")
            .is_some());
    }
}

fn golden_path() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/axon-api; the spec lives at the repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../openapi/openapi.json")
}

/// Redoc (and every other docs renderer) titles an operation with its
/// `summary`, falling back to the Rust function name when there is none. A
/// summary that is a whole sentence of prose reads as badly in the sidebar, so
/// the first line of each handler's doc comment must be a short title.
#[test]
fn every_operation_has_a_short_title_summary() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    const METHODS: [&str; 8] = [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];
    let mut bad = Vec::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        // A path item may also carry `parameters`, `summary`, `description` or
        // `servers`; only the HTTP methods are operations.
        for (method, op) in item.as_object().unwrap() {
            if !METHODS.contains(&method.as_str()) {
                continue;
            }
            match op["summary"].as_str() {
                Some(s) if s.chars().count() <= 60 && !s.ends_with('.') => {}
                other => bad.push(format!("{} {path}: {other:?}", method.to_uppercase())),
            }
        }
    }
    assert!(
        bad.is_empty(),
        "operations need a short title (no trailing period, at most 60 characters) as the \
         first doc-comment paragraph, followed by a blank `///` line before any detail:\n{}",
        bad.join("\n")
    );
}

#[test]
fn native_apple_contract_is_additive_and_form_encoded() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    for path in [
        "/v1/oauth/apple/native/challenge",
        "/v1/oauth/apple/native/token",
    ] {
        let post = &spec["paths"][path]["post"];
        assert!(post["requestBody"]["content"]["application/x-www-form-urlencoded"].is_object());
        assert_eq!(post["security"], serde_json::json!([]));
        assert!(post["responses"]["200"]["content"]["application/json"].is_object());
        assert!(post["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Authorization"
                && p["in"] == "header"
                && p["required"] == false));
        for status in ["413", "429"] {
            assert_eq!(
                post["responses"][status]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/ErrorResponse"
            );
        }
    }
    assert!(spec["paths"]["/v1/oauth/apple/native/token"]["post"]["responses"]["403"].is_null());
    assert!(spec["paths"]["/v1/oauth/providers"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "flow" && p["required"] == false));
}

#[test]
fn callbacks_document_query_get_form_post_and_html_failures() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let path = &spec["paths"]["/v1/oauth/{provider}/callback"];
    assert!(path["get"]["requestBody"].is_null());
    assert!(path["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "state" && p["in"] == "query"));
    assert!(
        path["post"]["requestBody"]["content"]["application/x-www-form-urlencoded"].is_object()
    );
    for method in ["get", "post"] {
        for status in ["400", "403", "404", "413", "429"] {
            assert!(path[method]["responses"][status]["content"]["text/html"].is_object());
        }
    }
}

#[test]
fn openapi_spec_is_current() {
    let spec = ApiDoc::openapi()
        .to_pretty_json()
        .expect("serialize OpenAPI spec");
    let spec = format!("{spec}\n"); // trailing newline so the file is POSIX-clean
    let path = golden_path();

    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create openapi dir");
        std::fs::write(&path, &spec).expect("write golden spec");
        return;
    }

    let golden = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing {}; generate it with `UPDATE_OPENAPI=1 cargo test -p axon-api --test openapi`",
            path.display()
        )
    });
    assert_eq!(
        spec, golden,
        "OpenAPI spec drift — regenerate with `UPDATE_OPENAPI=1 cargo test -p axon-api --test openapi`"
    );
}

#[test]
fn room_metadata_content_is_nullable_in_every_snapshot_schema() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    let snapshots: Vec<_> = schemas
        .iter()
        .filter(|(name, _)| name.starts_with("CachedRoomMetadata_"))
        .collect();
    assert_eq!(snapshots.len(), 8);
    for (name, schema) in snapshots {
        let redacted = &schema["properties"]["redacted"]["type"];
        assert_eq!(redacted, &serde_json::json!(["boolean", "null"]), "{name}");
        let redaction_id = &schema["properties"]["redaction_event_id"]["type"];
        assert_eq!(
            redaction_id,
            &serde_json::json!(["string", "null"]),
            "{name}"
        );
        let content = &schema["properties"]["content"];
        assert!(
            content["oneOf"]
                .as_array()
                .is_some_and(|branches| { branches.iter().any(|branch| branch["type"] == "null") }),
            "{name} must permit the null content returned for unknown/unavailable state: {content}"
        );
    }
}

#[test]
fn all_local_schema_references_resolve() {
    fn visit(value: &serde_json::Value, document: &serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(serde_json::Value::as_str) {
                    if let Some(pointer) = reference.strip_prefix('#') {
                        assert!(
                            document.pointer(pointer).is_some(),
                            "unresolved reference {reference}"
                        );
                    }
                }
                for child in object.values() {
                    visit(child, document);
                }
            }
            serde_json::Value::Array(array) => {
                for child in array {
                    visit(child, document);
                }
            }
            _ => {}
        }
    }
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    visit(&spec, &spec);
}

#[test]
fn room_metadata_documents_typed_authentication_and_database_errors() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let responses =
        &spec["paths"]["/v1/accounts/{account_id}/rooms/{room_id}/metadata"]["get"]["responses"];
    for status in ["401", "500"] {
        assert_eq!(
            responses[status]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ErrorResponse"
        );
    }
}
