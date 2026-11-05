//! The embedded help inputs: the reference's example and topic documents
//! (`models/examples`, `models/topics.json.zst`, vendored by
//! `scripts/vendor-docs.py`) and the argument tables and commands its
//! customizations build (`models/help`, extracted by
//! `scripts/gen-help-data.py`).

use crate::bjson::{Arr, Node, Obj};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/docs.rs"));
}

fn lookup(table: &'static [(&str, &[u8])], name: &str) -> Option<&'static Node> {
    table
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|i| Node::from_bytes(table[i].1))
}

/// The contents of `awscli/examples/<path>`, if the file exists.
pub(crate) fn example_file(path: &str) -> Option<String> {
    let (dir, rest) = match path.split_once('/') {
        Some((dir, rest)) => (dir, rest),
        None => ("_root", path),
    };
    lookup(embedded::EXAMPLES, dir)?
        .get(rest)
        .and_then(Node::as_str)
        .map(str::to_string)
}

/// `awscli/topics/<name>`.
pub(crate) fn topic_file(name: &str) -> Option<String> {
    Node::from_bytes(embedded::TOPICS)
        .get(name)
        .and_then(Node::as_str)
        .map(str::to_string)
}

/// The extracted help data of a model-driven service (`<cli name>.json`)
/// or of the main command table and custom commands (`_custom`).
pub(crate) struct HelpData {
    pub json: &'static Node,
}

impl HelpData {
    pub(crate) fn load(name: &str) -> Option<HelpData> {
        lookup(embedded::HELP, name).map(|json| HelpData { json })
    }

    pub(crate) fn undocumented(&self) -> Vec<&str> {
        self.json["undocumented"]
            .as_array()
            .map(|a| a.iter().filter_map(Node::as_str).collect())
            .unwrap_or_default()
    }

    /// The argument records of `operations` / `waiters`.
    pub(crate) fn args(&self, section: &str, name: &str) -> Option<&'static Arr> {
        self.json.get(section)?.get(name)?.as_array()
    }

    /// A `BasicCommand` spec of `_custom`.
    pub(crate) fn command(&self, path: &str) -> Option<&'static Obj> {
        self.json.get("commands")?.get(path)?.as_object()
    }

    pub(crate) fn doc(&self, index: &Node) -> &'static str {
        index
            .as_u64()
            .and_then(|i| self.json["docs"].as_array()?.get(i as usize))
            .and_then(Node::as_str)
            .unwrap_or_default()
    }

    pub(crate) fn model(&self, index: &Node) -> Option<&'static Obj> {
        index
            .as_u64()
            .and_then(|i| self.json["models"].as_array()?.get(i as usize))
            .and_then(Node::as_object)
    }
}
