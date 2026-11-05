pub mod encode;

use std::cmp::Ordering;
use std::fmt;
use std::ops::Index;

use encode::{ARRAY, FALSE, NULL, NUMBER, OBJECT, STRING, TRUE};

#[repr(transparent)]
pub struct Node([u8]);

#[repr(transparent)]
pub struct Obj(Node);

#[repr(transparent)]
pub struct Arr(Node);

static NULL_NODE: [u8; 1] = [NULL];
static EMPTY_ARRAY: [u8; 5] = [ARRAY, 0, 0, 0, 0];

const LINEAR_LOOKUP: usize = 8;

impl Node {
    pub fn from_bytes(bytes: &[u8]) -> &Node {
        assert!(!bytes.is_empty());
        unsafe { &*(bytes as *const [u8] as *const Node) }
    }

    pub fn null() -> &'static Node {
        Node::from_bytes(&NULL_NODE)
    }

    pub fn empty_array() -> &'static Arr {
        Node::from_bytes(&EMPTY_ARRAY).as_array().unwrap()
    }

    pub fn leak(value: &serde_json::Value) -> &'static Node {
        Node::from_bytes(Box::leak(encode::encode(value).into_boxed_slice()))
    }

    fn tag(&self) -> u8 {
        self.0[0]
    }

    fn u32_at(&self, at: usize) -> usize {
        u32::from_le_bytes(self.0[at..at + 4].try_into().unwrap()) as usize
    }

    fn child(&self, rel: usize) -> &Node {
        Node::from_bytes(&self.0[rel..])
    }

    fn str_at(&self, rel: usize) -> &str {
        let len = self.u32_at(rel);
        let bytes = &self.0[rel + 4..rel + 4 + len];
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    fn text(&self) -> &str {
        self.str_at(self.u32_at(1))
    }

    pub fn is_null(&self) -> bool {
        self.tag() == NULL
    }

    pub fn is_boolean(&self) -> bool {
        matches!(self.tag(), FALSE | TRUE)
    }

    pub fn is_string(&self) -> bool {
        self.tag() == STRING
    }

    pub fn is_number(&self) -> bool {
        self.tag() == NUMBER
    }

    pub fn is_array(&self) -> bool {
        self.tag() == ARRAY
    }

    pub fn is_object(&self) -> bool {
        self.tag() == OBJECT
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.tag() {
            FALSE => Some(false),
            TRUE => Some(true),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        (self.tag() == STRING).then(|| self.text())
    }

    pub fn as_number_text(&self) -> Option<&str> {
        (self.tag() == NUMBER).then(|| self.text())
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_number_text()?.parse().ok()
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_number_text()?.parse().ok()
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.as_number_text()?.parse().ok()
    }

    pub fn is_i64(&self) -> bool {
        self.as_i64().is_some()
    }

    pub fn is_u64(&self) -> bool {
        self.as_u64().is_some()
    }

    pub fn is_f64(&self) -> bool {
        self.as_number_text()
            .is_some_and(|t| t.contains(['.', 'e', 'E']))
    }

    pub fn as_array(&self) -> Option<&Arr> {
        (self.tag() == ARRAY).then(|| unsafe { &*(self as *const Node as *const Arr) })
    }

    pub fn as_object(&self) -> Option<&Obj> {
        (self.tag() == OBJECT).then(|| unsafe { &*(self as *const Node as *const Obj) })
    }

    pub fn get(&self, key: &str) -> Option<&Node> {
        self.as_object()?.get(key)
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self.tag() {
            NULL => serde_json::Value::Null,
            FALSE => serde_json::Value::Bool(false),
            TRUE => serde_json::Value::Bool(true),
            NUMBER => serde_json::from_str(self.text()).expect("number"),
            STRING => serde_json::Value::String(self.text().to_string()),
            ARRAY => serde_json::Value::Array(
                self.as_array().unwrap().iter().map(Node::to_json).collect(),
            ),
            _ => serde_json::Value::Object(
                self.as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_json()))
                    .collect(),
            ),
        }
    }
}

impl Arr {
    fn node(&self) -> &Node {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.node().u32_at(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn at(&self, i: usize) -> &Node {
        let node = self.node();
        node.child(node.u32_at(5 + 4 * i))
    }

    pub fn get(&self, i: usize) -> Option<&Node> {
        (i < self.len()).then(|| self.at(i))
    }

    pub fn first(&self) -> Option<&Node> {
        self.get(0)
    }

    pub fn last(&self) -> Option<&Node> {
        self.len().checked_sub(1).map(|i| self.at(i))
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Node> + ExactSizeIterator + Clone + '_ {
        (0..self.len()).map(|i| self.at(i))
    }
}

impl Obj {
    fn node(&self) -> &Node {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.node().u32_at(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn key(&self, i: usize) -> &str {
        let node = self.node();
        node.str_at(node.u32_at(5 + 8 * i))
    }

    fn value(&self, i: usize) -> &Node {
        let node = self.node();
        node.child(node.u32_at(9 + 8 * i))
    }

    fn find(&self, key: &str) -> Option<usize> {
        let n = self.len();
        if n <= LINEAR_LOOKUP {
            return (0..n).find(|&i| self.key(i) == key);
        }
        let node = self.node();
        let sorted = 5 + 8 * n;
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let i = node.u32_at(sorted + 4 * mid);
            match self.key(i).cmp(key) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(i),
            }
        }
        None
    }

    pub fn get(&self, key: &str) -> Option<&Node> {
        self.find(key).map(|i| self.value(i))
    }

    pub fn get_key_value(&self, key: &str) -> Option<(&str, &Node)> {
        self.find(key).map(|i| (self.key(i), self.value(i)))
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.find(key).is_some()
    }

    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&str, &Node)> + ExactSizeIterator + Clone + '_ {
        (0..self.len()).map(|i| (self.key(i), self.value(i)))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator + Clone + '_ {
        (0..self.len()).map(|i| self.key(i))
    }

    pub fn values(
        &self,
    ) -> impl DoubleEndedIterator<Item = &Node> + ExactSizeIterator + Clone + '_ {
        (0..self.len()).map(|i| self.value(i))
    }

    pub fn as_node(&self) -> &Node {
        self.node()
    }

    pub fn to_json(&self) -> serde_json::Value {
        self.node().to_json()
    }

    pub fn to_map(&self) -> serde_json::Map<String, serde_json::Value> {
        self.iter()
            .map(|(k, v)| (k.to_string(), v.to_json()))
            .collect()
    }
}

impl Index<&str> for Node {
    type Output = Node;

    fn index(&self, key: &str) -> &Node {
        self.get(key).unwrap_or(Node::null())
    }
}

impl Index<usize> for Node {
    type Output = Node;

    fn index(&self, i: usize) -> &Node {
        self.as_array()
            .and_then(|a| a.get(i))
            .unwrap_or(Node::null())
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Node) -> bool {
        match (self.tag(), other.tag()) {
            (STRING, STRING) | (NUMBER, NUMBER) => self.text() == other.text(),
            (ARRAY, ARRAY) => {
                let (a, b) = (self.as_array().unwrap(), other.as_array().unwrap());
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
            }
            (OBJECT, OBJECT) => {
                let (a, b) = (self.as_object().unwrap(), other.as_object().unwrap());
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k) == Some(v))
            }
            (x, y) => x == y,
        }
    }
}

impl PartialEq<str> for Node {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == Some(other)
    }
}

impl PartialEq<&str> for Node {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == Some(*other)
    }
}

impl PartialEq<bool> for Node {
    fn eq(&self, other: &bool) -> bool {
        self.as_bool() == Some(*other)
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

impl fmt::Debug for Obj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.node(), f)
    }
}

impl fmt::Debug for Arr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.node(), f)
    }
}

impl<'a> IntoIterator for &'a Arr {
    type Item = &'a Node;
    type IntoIter = Box<dyn Iterator<Item = &'a Node> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<'a> IntoIterator for &'a Obj {
    type Item = (&'a str, &'a Node);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a Node)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_looks_up() {
        let mut map = serde_json::Map::new();
        for i in 0..20 {
            map.insert(format!("k{i:02}"), serde_json::json!(i));
        }
        let mut value: serde_json::Value = serde_json::from_str(
            r#"{"s": "x", "n": 1.50, "big": 123456789012345678901234567890,
                "a": [null, true, false, "x", {"z": 1}], "m": null, "e": {}, "ea": []}"#,
        )
        .unwrap();
        value["m"] = serde_json::Value::Object(map);
        let bytes = encode::encode(&value);
        let node = Node::from_bytes(&bytes);
        assert_eq!(node.to_json(), value);
        assert_eq!(node["m"]["k13"].as_i64(), Some(13));
        assert!(node["m"]["k20"].is_null());
        assert_eq!(node["a"][4]["z"].as_u64(), Some(1));
        assert_eq!(node["s"], "x");
        assert_eq!(node["n"].as_number_text(), Some("1.50"));
        let keys: Vec<_> = node.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["s", "n", "big", "a", "m", "e", "ea"]);
    }
}
