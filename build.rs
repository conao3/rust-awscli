use std::env;
use std::fs;
use std::path::Path;

use serde_json::Value;

#[path = "src/bjson/encode.rs"]
#[allow(dead_code)]
mod encode;

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".json.zst").map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

/// `(name, include_bytes!(path))` entries for the `<name>.json.zst` files
/// of `dir`, sorted by name.
fn table(dir: &Path, var: &str) -> String {
    let mut code = format!("pub static {var}: &[(&str, &[u8])] = &[\n");
    for name in names(dir) {
        let path = dir.join(format!("{name}.json.zst"));
        code.push_str(&format!(
            "    ({name:?}, include_bytes!({:?})),\n",
            path.display().to_string()
        ));
    }
    code.push_str("];\n");
    code
}

fn read_json(path: &Path) -> Value {
    let raw = zstd::decode_all(fs::File::open(path).unwrap()).unwrap();
    serde_json::from_slice(&raw).unwrap()
}

fn write_node(out: &Path, value: &Value) -> String {
    fs::write(out, encode::encode(value)).unwrap();
    format!("include_bytes!({:?})", out.display().to_string())
}

/// The operation model changes botocore makes before the first call
/// when endpoint rule sets are in effect: s3
/// `remove_bucket_from_url_paths_from_model` and s3control
/// `remove_accid_host_prefix_from_model` (`botocore/handlers.py`).
fn apply_endpoint_ruleset_customizations(name: &str, doc: &mut Value) {
    let Some(ops) = doc["service"]["operations"].as_object_mut() else {
        return;
    };
    for op in ops.values_mut() {
        if name == "s3" {
            if let Some(uri) = op.get_mut("http").and_then(|h| h.get_mut("requestUri"))
                && let Some(rest) = uri.as_str().and_then(|u| u.strip_prefix("/{Bucket}"))
            {
                *uri = Value::String(rest.to_string());
            }
        } else if name == "s3control" {
            let requires_account_id = op
                .get("staticContextParams")
                .and_then(|p| p.get("RequiresAccountId"))
                .and_then(|p| p.get("value"))
                .and_then(Value::as_bool)
                == Some(true);
            if requires_account_id
                && let Some(endpoint) = op.get_mut("endpoint").and_then(Value::as_object_mut)
                && endpoint.get("hostPrefix").and_then(Value::as_str) == Some("{AccountId}.")
            {
                endpoint.remove("hostPrefix");
            }
        }
    }
}

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir);
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let models = Path::new(&manifest_dir).join("models");
    println!("cargo:rerun-if-changed=models");
    println!("cargo:rerun-if-changed=src/bjson/encode.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
    {
        println!("cargo:rustc-link-arg-bins=-no-pie");
    }

    let services_dir = models.join("services");
    let bin_dir = out_dir.join("services");
    fs::create_dir_all(&bin_dir).unwrap();
    let mut code = String::from("pub static SERVICES: &[(&str, &[u8])] = &[\n");
    for name in names(&services_dir) {
        let mut doc = read_json(&services_dir.join(format!("{name}.json.zst")));
        apply_endpoint_ruleset_customizations(&name, &mut doc);
        let bytes = write_node(&bin_dir.join(format!("{name}.bin")), &doc);
        code.push_str(&format!("    ({name:?}, {bytes}),\n"));
    }
    code.push_str("];\n");
    for (var, file) in [
        ("PARTITIONS", "partitions"),
        ("ENDPOINTS", "endpoints"),
        ("CREDENTIAL_SCOPES", "credential-scopes"),
        ("CLI", "cli"),
    ] {
        let doc = read_json(&models.join(format!("{file}.json.zst")));
        let bytes = write_node(&out_dir.join(format!("{file}.bin")), &doc);
        code.push_str(&format!("pub static {var}: &[u8] = {bytes};\n"));
    }
    fs::write(out_dir.join("models.rs"), code).unwrap();

    let mut docs = table(&models.join("examples"), "EXAMPLES");
    docs.push_str(&table(&models.join("help"), "HELP"));
    fs::write(out_dir.join("docs.rs"), docs).unwrap();
}
