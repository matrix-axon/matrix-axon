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

fn golden_path() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/axon-api; the spec lives at the repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../openapi/openapi.json")
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
