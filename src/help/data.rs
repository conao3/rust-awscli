//! The embedded help inputs: the reference's example and topic documents
//! (`models/examples`, `models/topics.json.zst`, vendored by
//! `scripts/vendor-docs.py`) and the argument tables and commands its
//! customizations build (`models/help`, extracted by
//! `scripts/gen-help-data.py`).

use serde_json::{Map, Value as Json};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/docs.rs"));
}

static TOPICS: &[u8] = include_bytes!("../../models/topics.json.zst");

fn decompress(bytes: &[u8]) -> Json {
    let raw = zstd::decode_all(bytes).expect("decompress embedded help data");
    serde_json::from_slice(&raw).expect("parse embedded help data")
}

fn lookup(table: &[(&str, &[u8])], name: &str) -> Option<Json> {
    table
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|i| decompress(table[i].1))
}

/// The contents of `awscli/examples/<path>`, if the file exists.
pub(crate) fn example_file(path: &str) -> Option<String> {
    let (dir, rest) = match path.split_once('/') {
        Some((dir, rest)) => (dir, rest),
        None => ("_root", path),
    };
    lookup(embedded::EXAMPLES, dir)?
        .get(rest)
        .and_then(Json::as_str)
        .map(str::to_string)
}

/// `awscli/topics/<name>`.
pub(crate) fn topic_file(name: &str) -> Option<String> {
    decompress(TOPICS)
        .get(name)
        .and_then(Json::as_str)
        .map(str::to_string)
}

/// The extracted help data of a model-driven service (`<cli name>.json`)
/// or of the main command table and custom commands (`_custom`).
pub(crate) struct HelpData {
    pub json: Json,
}

impl HelpData {
    pub(crate) fn load(name: &str) -> Option<HelpData> {
        lookup(embedded::HELP, name).map(|json| HelpData { json })
    }

    pub(crate) fn undocumented(&self) -> Vec<&str> {
        self.json["undocumented"]
            .as_array()
            .map(|a| a.iter().filter_map(Json::as_str).collect())
            .unwrap_or_default()
    }

    /// The argument records of `operations` / `waiters`.
    pub(crate) fn args(&self, section: &str, name: &str) -> Option<&Vec<Json>> {
        self.json.get(section)?.get(name)?.as_array()
    }

    /// A `BasicCommand` spec of `_custom`.
    pub(crate) fn command(&self, path: &str) -> Option<&Map<String, Json>> {
        self.json.get("commands")?.get(path)?.as_object()
    }

    pub(crate) fn doc(&self, index: &Json) -> &str {
        index
            .as_u64()
            .and_then(|i| self.json["docs"].get(i as usize))
            .and_then(Json::as_str)
            .unwrap_or_default()
    }

    pub(crate) fn model(&self, index: &Json) -> Option<&Map<String, Json>> {
        index
            .as_u64()
            .and_then(|i| self.json["models"].get(i as usize))
            .and_then(Json::as_object)
    }
}
