use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::Value;

pub const NULL: u8 = 0;
pub const FALSE: u8 = 1;
pub const TRUE: u8 = 2;
pub const NUMBER: u8 = 3;
pub const STRING: u8 = 4;
pub const ARRAY: u8 = 5;
pub const OBJECT: u8 = 6;

struct Encoder<'v> {
    out: Vec<u8>,
    strings: Vec<Cow<'v, str>>,
    ids: HashMap<Cow<'v, str>, usize>,
    refs: Vec<(usize, usize, usize)>,
}

impl<'v> Encoder<'v> {
    fn intern(&mut self, s: Cow<'v, str>) -> usize {
        if let Some(&id) = self.ids.get(s.as_ref()) {
            return id;
        }
        let id = self.strings.len();
        self.strings.push(s.clone());
        self.ids.insert(s, id);
        id
    }

    fn u32(&mut self, n: usize) {
        self.out
            .extend_from_slice(&u32::try_from(n).expect("offset").to_le_bytes());
    }

    fn patch(&mut self, at: usize, n: usize) {
        self.out[at..at + 4].copy_from_slice(&u32::try_from(n).expect("offset").to_le_bytes());
    }

    fn string_ref(&mut self, node: usize, s: Cow<'v, str>) {
        let id = self.intern(s);
        self.refs.push((self.out.len(), node, id));
        self.u32(0);
    }

    fn write(&mut self, v: &'v Value) -> usize {
        let pos = self.out.len();
        match v {
            Value::Null => self.out.push(NULL),
            Value::Bool(false) => self.out.push(FALSE),
            Value::Bool(true) => self.out.push(TRUE),
            Value::Number(n) => {
                self.out.push(NUMBER);
                self.string_ref(pos, Cow::Owned(n.to_string()));
            }
            Value::String(s) => {
                self.out.push(STRING);
                self.string_ref(pos, Cow::Borrowed(s));
            }
            Value::Array(items) => {
                self.out.push(ARRAY);
                self.u32(items.len());
                let slots = self.out.len();
                self.out.resize(slots + 4 * items.len(), 0);
                for (i, item) in items.iter().enumerate() {
                    let child = self.write(item);
                    self.patch(slots + 4 * i, child - pos);
                }
            }
            Value::Object(map) => {
                self.out.push(OBJECT);
                self.u32(map.len());
                let slots = self.out.len();
                for (key, _) in map {
                    self.string_ref(pos, Cow::Borrowed(key));
                    self.u32(0);
                }
                let mut order: Vec<usize> = (0..map.len()).collect();
                let keys: Vec<&str> = map.keys().map(String::as_str).collect();
                order.sort_by(|a, b| keys[*a].cmp(keys[*b]));
                for i in order {
                    self.u32(i);
                }
                for (i, (_, value)) in map.iter().enumerate() {
                    let child = self.write(value);
                    self.patch(slots + 8 * i + 4, child - pos);
                }
            }
        }
        pos
    }
}

pub fn encode(value: &Value) -> Vec<u8> {
    let mut e = Encoder {
        out: Vec::new(),
        strings: Vec::new(),
        ids: HashMap::new(),
        refs: Vec::new(),
    };
    e.write(value);
    let mut table = Vec::with_capacity(e.strings.len());
    for s in std::mem::take(&mut e.strings) {
        table.push(e.out.len());
        e.u32(s.len());
        e.out.extend_from_slice(s.as_bytes());
    }
    for (at, node, id) in std::mem::take(&mut e.refs) {
        e.patch(at, table[id] - node);
    }
    e.out
}
