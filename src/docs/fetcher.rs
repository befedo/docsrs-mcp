use crate::error::Error;

/// Fetch the raw zstd-compressed rustdoc JSON bytes from docs.rs.
///
/// The URL pattern is: `https://docs.rs/crate/{name}/{version}/json`
/// Returns the raw compressed bytes without any processing.
pub async fn fetch_raw_bytes(
    client: &reqwest::Client,
    crate_name: &str,
    version: &str,
) -> Result<Vec<u8>, Error> {
    let url = format!("https://docs.rs/crate/{crate_name}/{version}/json");
    tracing::info!("Fetching rustdoc JSON from {url}");

    let response = client.get(&url).send().await?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::JsonNotAvailable {
            crate_name: crate_name.to_string(),
            version: version.to_string(),
        });
    }

    let response = response.error_for_status()?;
    let bytes = response.bytes().await?;
    Ok(bytes.to_vec())
}

/// Decode raw zstd-compressed rustdoc JSON bytes into a `rustdoc_types::Crate`.
///
/// Decompresses, normalizes across format versions, and deserializes.
pub fn decode_raw_bytes(
    bytes: &[u8],
    crate_name: &str,
    version: &str,
) -> Result<rustdoc_types::Crate, Error> {
    let decompressed = zstd::stream::decode_all(bytes).map_err(Error::Zstd)?;

    let mut value: serde_json::Value = serde_json::from_slice(&decompressed)?;

    let format_version = value
        .get("format_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    tracing::info!("Rustdoc JSON for {crate_name} v{version} has format_version {format_version}");

    normalize_for_v61(&mut value, format_version);

    let krate: rustdoc_types::Crate = serde_json::from_value(value)?;
    tracing::info!(
        "Parsed rustdoc JSON for {crate_name} v{version}: {} items",
        krate.index.len()
    );
    Ok(krate)
}

/// Normalize a rustdoc JSON value so it deserializes with `rustdoc-types` 0.61.
///
/// docs.rs is still serving format 57 for crates like compio 0.18.0 and compio-runtime 0.11.0,
/// and 0.61 requires `ExternalCrate.path` while older formats omit it. The compatibility shims we
/// need are therefore:
/// - **53 -> 54**: `Item.attrs` changed from strings to tagged `Attribute` values. We ignore attrs.
/// - **55 -> 56**: `Crate.target` was introduced; older JSON omits it, so inject a placeholder.
/// - **56 -> 57**: `ExternalCrate.path` was introduced; older JSON omits it, so inject a dummy
///   `PathBuf` for each external crate before deserializing with 0.61.
fn normalize_for_v61(value: &mut serde_json::Value, format_version: u64) {
    // For all versions: empty attrs arrays (format changed 53->54, we don't use them)
    strip_attrs(value);

    // For format < 56: inject a dummy target (Crate.target was added in format 56)
    if format_version < 56 {
        inject_dummy_target(value);
    }

    // 0.61 requires ExternalCrate.path; older docs.rs JSON (format < 57) omitted it.
    if format_version < 57 {
        inject_dummy_external_crate_paths(value);
    }
}

/// Recursively replace all `"attrs"` arrays with `[]`.
///
/// The `attrs` field changed from `Vec<String>` (format <= 53) to `Vec<Attribute>`
/// (format >= 54). Since we never use attrs, emptying them avoids deserialization
/// errors regardless of format version.
fn strip_attrs(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(attrs) = map.get_mut("attrs")
                && attrs.is_array()
            {
                *attrs = serde_json::Value::Array(Vec::new());
            }
            for v in map.values_mut() {
                strip_attrs(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_attrs(v);
            }
        }
        _ => {}
    }
}

/// Inject a dummy `target` field into the root if not present.
///
/// Format version 56 added `Crate.target: Target` which older formats lack.
/// We inject a minimal placeholder so deserialization succeeds.
fn inject_dummy_target(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = value
        && !map.contains_key("target")
    {
        map.insert(
            "target".to_string(),
            serde_json::json!({
                "triple": "unknown",
                "target_features": []
            }),
        );
    }
}

/// Inject a dummy `path` field into each entry in `"external_crates"` when absent.
///
/// `rustdoc-types` 0.61 requires `ExternalCrate.path: PathBuf` for all external crates. Older
/// docs.rs JSON omits the field, so we synthesize a harmless placeholder before deserializing.
fn inject_dummy_external_crate_paths(value: &mut serde_json::Value) {
    if let Some(serde_json::Value::Object(crates_map)) = value.get_mut("external_crates") {
        for crate_value in crates_map.values_mut() {
            if let serde_json::Value::Object(crate_obj) = crate_value
                && !crate_obj.contains_key("path")
            {
                crate_obj.insert(
                    "path".to_string(),
                    serde_json::Value::String("".to_string()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ========== strip_attrs tests ==========

    #[test]
    fn strip_attrs_empties_top_level_array() {
        let mut value = json!({
            "attrs": ["#[derive(Debug)]", "#[allow(unused)]"]
        });
        strip_attrs(&mut value);
        assert_eq!(value["attrs"], json!([]));
    }

    #[test]
    fn strip_attrs_empties_nested_in_items() {
        // Simulates the real rustdoc JSON structure: items inside the index have attrs
        let mut value = json!({
            "index": {
                "0:3": {
                    "name": "MyStruct",
                    "attrs": ["#[derive(Debug)]"],
                    "inner": { "kind": "struct" }
                },
                "0:5": {
                    "name": "my_fn",
                    "attrs": ["#[inline]"],
                    "inner": { "kind": "function" }
                }
            }
        });
        strip_attrs(&mut value);
        assert_eq!(value["index"]["0:3"]["attrs"], json!([]));
        assert_eq!(value["index"]["0:5"]["attrs"], json!([]));
    }

    #[test]
    fn strip_attrs_handles_format_54_tagged_enum_attrs() {
        // Format 54+ uses tagged enum attrs like {"Attribute": "derive"}
        // These are still arrays, so strip_attrs should empty them
        let mut value = json!({
            "attrs": [
                {"Derive": "Debug"},
                {"Other": {"value": "#[serde(rename)]"}}
            ]
        });
        strip_attrs(&mut value);
        assert_eq!(value["attrs"], json!([]));
    }

    #[test]
    fn strip_attrs_leaves_non_array_attrs_alone() {
        // If "attrs" is not an array (hypothetical), don't touch it
        let mut value = json!({ "attrs": "not-an-array" });
        strip_attrs(&mut value);
        assert_eq!(value["attrs"], json!("not-an-array"));
    }

    #[test]
    fn strip_attrs_recurses_into_arrays() {
        // Items can appear inside arrays (e.g., in some JSON structures)
        let mut value = json!([
            { "attrs": ["a"] },
            { "attrs": ["b", "c"] }
        ]);
        strip_attrs(&mut value);
        assert_eq!(value[0]["attrs"], json!([]));
        assert_eq!(value[1]["attrs"], json!([]));
    }

    #[test]
    fn strip_attrs_no_attrs_key_is_noop() {
        let mut value = json!({"name": "foo", "inner": {}});
        let original = value.clone();
        strip_attrs(&mut value);
        assert_eq!(value, original);
    }

    // ========== strip_external_crate_paths tests ==========

    #[test]
    fn inject_dummy_external_crate_paths_adds_missing_path_field() {
        let mut value = json!({
            "external_crates": {
                "0": { "name": "std" },
                "1": { "name": "core", "html_root_url": "https://docs.rs/core" }
            }
        });
        inject_dummy_external_crate_paths(&mut value);

        let crate_0 = &value["external_crates"]["0"];
        assert_eq!(crate_0["name"], json!("std"));
        assert_eq!(crate_0["path"], json!(""));

        let crate_1 = &value["external_crates"]["1"];
        assert_eq!(crate_1["name"], json!("core"));
        assert_eq!(crate_1["path"], json!(""));
        assert_eq!(crate_1["html_root_url"], json!("https://docs.rs/core"));
    }

    #[test]
    fn inject_dummy_external_crate_paths_noop_when_present() {
        let mut value = json!({
            "external_crates": {
                "0": { "name": "serde", "path": "/some/path" }
            }
        });
        let original = value.clone();
        inject_dummy_external_crate_paths(&mut value);
        assert_eq!(value, original);
    }

    #[test]
    fn inject_dummy_external_crate_paths_noop_when_no_external_crates() {
        let mut value = json!({"index": {}, "paths": {}});
        let original = value.clone();
        inject_dummy_external_crate_paths(&mut value);
        assert_eq!(value, original);
    }

    // ========== inject_dummy_target tests ==========

    #[test]
    fn inject_dummy_target_adds_when_missing() {
        let mut value = json!({"root": 0});
        inject_dummy_target(&mut value);
        assert!(value.get("target").is_some());
        assert_eq!(value["target"]["triple"], json!("unknown"));
        assert_eq!(value["target"]["target_features"], json!([]));
    }

    #[test]
    fn inject_dummy_target_noop_when_present() {
        let mut value = json!({
            "target": { "triple": "x86_64-unknown-linux-gnu", "target_features": [] }
        });
        let original = value.clone();
        inject_dummy_target(&mut value);
        assert_eq!(value, original);
    }

    // ========== normalize_for_v61 integration tests ==========

    #[test]
    fn normalize_v53_strips_attrs_and_injects_target() {
        let mut value = json!({
            "format_version": 53,
            "index": {
                "0": { "attrs": ["#[derive(Debug)]"], "name": "Foo" }
            },
            "external_crates": {
                "1": { "name": "std" }
            }
        });
        normalize_for_v61(&mut value, 53);

        // attrs should be emptied
        assert_eq!(value["index"]["0"]["attrs"], json!([]));
        // external_crates should be untouched (no path to strip, and version < 57)
        assert_eq!(value["external_crates"]["1"]["name"], json!("std"));
        // target should be injected
        assert_eq!(value["target"]["triple"], json!("unknown"));
    }

    #[test]
    fn normalize_v56_strips_attrs_only() {
        let mut value = json!({
            "format_version": 56,
            "index": {
                "0:1": { "attrs": [{"Other": "#[cfg(test)]"}], "name": "Bar" }
            },
            "external_crates": {
                "1": { "name": "core" }
            }
        });
        normalize_for_v61(&mut value, 56);

        assert_eq!(value["index"]["0:1"]["attrs"], json!([]));
        assert_eq!(value["external_crates"]["1"]["name"], json!("core"));
    }

    #[test]
    fn normalize_v57_keeps_external_crate_paths() {
        let mut value = json!({
            "format_version": 57,
            "index": {
                "0:1": { "attrs": [{"MacroExport": null}], "name": "Baz" }
            },
            "external_crates": {
                "1": { "name": "std", "path": "/rustc/library/std" },
                "2": { "name": "alloc", "path": "/rustc/library/alloc" }
            }
        });
        normalize_for_v61(&mut value, 57);

        assert_eq!(value["index"]["0:1"]["attrs"], json!([]));
        assert_eq!(
            value["external_crates"]["1"]["path"],
            json!("/rustc/library/std")
        );
        assert_eq!(
            value["external_crates"]["2"]["path"],
            json!("/rustc/library/alloc")
        );
    }

    #[test]
    fn normalize_v58_keeps_external_crate_paths() {
        let mut value = json!({
            "format_version": 58,
            "external_crates": {
                "0": { "name": "foo", "path": "/some/path" }
            }
        });
        normalize_for_v61(&mut value, 58);
        assert_eq!(value["external_crates"]["0"]["path"], json!("/some/path"));
    }

    // ========== Deserialization roundtrip tests ==========

    /// Build a minimal but valid rustdoc JSON value that rustdoc-types 0.56 can parse.
    /// Uses Id(u32) format: map keys are plain numbers like "0", "1".
    ///
    /// When `format_version < 56`, the `target` field is omitted to simulate
    /// older format JSON (the normalizer should inject a dummy target).
    fn minimal_rustdoc_json(format_version: u64) -> serde_json::Value {
        let mut value = json!({
            "root": 0,
            "crate_version": "1.0.0",
            "includes_private": false,
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "test_crate",
                    "span": null,
                    "visibility": "public",
                    "docs": "A test crate",
                    "links": {},
                    "attrs": [],
                    "deprecation": null,
                    "inner": {
                        "module": {
                            "is_crate": true,
                            "items": [1],
                            "is_stripped": false
                        }
                    }
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "MyStruct",
                    "span": null,
                    "visibility": "public",
                    "docs": "A test struct",
                    "links": {},
                    "attrs": [],
                    "deprecation": null,
                    "inner": {
                        "struct": {
                            "kind": "unit",
                            "generics": {
                                "params": [],
                                "where_predicates": []
                            },
                            "impls": []
                        }
                    }
                }
            },
            "paths": {
                "0": {
                    "crate_id": 0,
                    "path": ["test_crate"],
                    "kind": "module"
                },
                "1": {
                    "crate_id": 0,
                    "path": ["test_crate", "MyStruct"],
                    "kind": "struct"
                }
            },
            "external_crates": {
                "2": { "name": "std", "html_root_url": null }
            },
            "format_version": format_version
        });

        // Format 56+ includes the target field; older versions don't
        if format_version >= 56 {
            value.as_object_mut().unwrap().insert(
                "target".to_string(),
                json!({
                    "triple": "x86_64-unknown-linux-gnu",
                    "target_features": []
                }),
            );
        }

        value
    }

    #[test]
    fn roundtrip_v56_deserializes_successfully() {
        let mut value = minimal_rustdoc_json(56);
        normalize_for_v61(&mut value, 56);
        let krate: rustdoc_types::Crate =
            serde_json::from_value(value).expect("v56 JSON should deserialize after normalization");
        assert_eq!(krate.index.len(), 2);
    }

    #[test]
    fn roundtrip_v53_with_string_attrs_deserializes() {
        // Format 53 used plain string attrs -- the tagged enum Attribute in 0.56
        // would fail to deserialize these, but strip_attrs empties them first
        let mut value = minimal_rustdoc_json(53);
        value["index"]["1"]["attrs"] = json!(["#[derive(Debug)]", "#[allow(unused)]"]);

        normalize_for_v61(&mut value, 53);
        let krate: rustdoc_types::Crate = serde_json::from_value(value)
            .expect("v53 JSON with string attrs should deserialize after normalization");
        assert_eq!(krate.index.len(), 2);
    }

    #[test]
    fn roundtrip_v57_with_external_crate_path_deserializes() {
        // Format 57 adds ExternalCrate.path; 0.61 expects it, and a dummy is injected only for
        // older format versions.
        let mut value = minimal_rustdoc_json(57);
        value["external_crates"]["2"]
            .as_object_mut()
            .unwrap()
            .insert("path".to_string(), json!("/rustc/library/std"));

        normalize_for_v61(&mut value, 57);
        let krate: rustdoc_types::Crate = serde_json::from_value(value)
            .expect("v57 JSON with ExternalCrate.path should deserialize after normalization");
        assert_eq!(krate.index.len(), 2);
        assert!(krate.external_crates.values().any(|c| c.name == "std"));
    }

    #[test]
    fn roundtrip_v53_without_normalization_fails() {
        // v53 JSON with string attrs and no target field should fail without normalization
        let mut value = minimal_rustdoc_json(53);
        value["index"]["1"]["attrs"] = json!(["#[derive(Debug)]"]);

        let result: Result<rustdoc_types::Crate, _> = serde_json::from_value(value);
        assert!(
            result.is_err(),
            "v53 JSON should fail without normalization (missing target, string attrs)"
        );
    }

    #[test]
    fn roundtrip_v57_extra_fields_ignored_by_serde() {
        // serde's default behavior ignores unknown fields, so additional external-crate fields are
        // still accepted. 0.61 also accepts the live docs.rs path field.
        let mut value = minimal_rustdoc_json(57);
        value["external_crates"]["2"]
            .as_object_mut()
            .unwrap()
            .insert("path".to_string(), json!("/rustc/library/std"));

        let result: Result<rustdoc_types::Crate, _> = serde_json::from_value(value);
        assert!(result.is_ok(), "serde ignores unknown fields by default");
    }

    // ========== decode_raw_bytes tests ==========

    /// Helper: zstd-compress a JSON value to simulate raw bytes from docs.rs.
    fn zstd_compress_json(value: &serde_json::Value) -> Vec<u8> {
        let json_bytes = serde_json::to_vec(value).unwrap();
        zstd::stream::encode_all(json_bytes.as_slice(), 3).unwrap()
    }

    #[test]
    fn decode_raw_bytes_roundtrip_v56() {
        let value = minimal_rustdoc_json(56);
        let compressed = zstd_compress_json(&value);

        let krate = decode_raw_bytes(&compressed, "test_crate", "1.0.0")
            .expect("should decode valid zstd-compressed rustdoc JSON");
        assert_eq!(krate.index.len(), 2);
    }

    #[test]
    fn decode_raw_bytes_normalizes_v53() {
        // v53 JSON needs normalization (target injection, attr stripping)
        let mut value = minimal_rustdoc_json(53);
        value["index"]["1"]["attrs"] = json!(["#[derive(Debug)]"]);
        let compressed = zstd_compress_json(&value);

        let krate = decode_raw_bytes(&compressed, "test_crate", "1.0.0")
            .expect("should normalize and decode v53 JSON");
        assert_eq!(krate.index.len(), 2);
    }

    #[test]
    fn decode_raw_bytes_rejects_invalid_zstd() {
        let result = decode_raw_bytes(b"not valid zstd", "test_crate", "1.0.0");
        assert!(result.is_err());
    }

    #[test]
    fn decode_raw_bytes_rejects_invalid_json() {
        // Valid zstd but not valid JSON inside
        let compressed = zstd::stream::encode_all(b"not json".as_slice(), 3).unwrap();
        let result = decode_raw_bytes(&compressed, "test_crate", "1.0.0");
        assert!(result.is_err());
    }
}
