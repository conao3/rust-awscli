use std::cmp::Ordering;
use std::collections::HashSet;

use super::HelpRequest;
use super::data::HelpData;
use crate::cli::commands::{
    MainCommand, ServiceEntry, main_commands, service_commands, waiter_commands,
};
use crate::model::ServiceModel;

static PAGES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/help-pages.bin"));

const MAGIC: &[u8; 8] = b"AWSRHELP";
const ENTRY: usize = 24;

fn u32_at(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

struct Table<'a> {
    blob: &'a [u8],
    version: &'a [u8],
    dict: &'a [u8],
    count: usize,
    index: usize,
}

struct Entry<'a> {
    key: &'a [u8],
    data: &'a [u8],
    stderr_len: usize,
    raw_len: usize,
}

impl Table<'_> {
    fn parse(blob: &[u8]) -> Option<Table<'_>> {
        if blob.get(..8)? != MAGIC {
            return None;
        }
        let version_len = u32_at(blob, 8)?;
        let version = blob.get(12..12 + version_len)?;
        let at = 12 + version_len;
        let dict_len = u32_at(blob, at)?;
        let dict = blob.get(at + 4..at + 4 + dict_len)?;
        let at = at + 4 + dict_len;
        Some(Table {
            blob,
            version,
            dict,
            count: u32_at(blob, at)?,
            index: at + 4,
        })
    }

    fn entry(&self, i: usize) -> Option<Entry<'_>> {
        let at = self.index + i * ENTRY;
        let field = |n: usize| u32_at(self.blob, at + 4 * n);
        Some(Entry {
            key: self.blob.get(field(0)?..)?.get(..field(1)?)?,
            data: self.blob.get(field(2)?..)?.get(..field(3)?)?,
            stderr_len: field(4)?,
            raw_len: field(5)?,
        })
    }

    fn find(&self, key: &[u8]) -> Option<Entry<'_>> {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let entry = self.entry(mid)?;
            match entry.key.cmp(key) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(entry),
            }
        }
        None
    }
}

pub(super) fn lookup(req: &HelpRequest) -> Option<(Vec<u8>, Vec<u8>)> {
    let table = Table::parse(PAGES)?;
    let entry = table.find(key(req).as_bytes())?;
    if super::render::formatter()? != "groff"
        || std::env::vars_os().any(|(k, _)| k.as_encoded_bytes().starts_with(b"GROFF"))
        || super::cache::groff_version()?.as_bytes() != table.version
    {
        return None;
    }
    let mut stderr = zstd::bulk::Decompressor::with_dictionary(table.dict)
        .ok()?
        .decompress(entry.data, entry.raw_len)
        .ok()?;
    let output = stderr.split_off(entry.stderr_len);
    Some((stderr, output))
}

pub fn requests() -> Vec<HelpRequest> {
    let line = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut lines = vec![line(&["help"]), line(&["help", "topics"])];
    for topic in super::topic_names() {
        lines.push(line(&["help", &topic]));
    }
    for command in main_commands() {
        match command {
            MainCommand::Service(cli, service) => {
                lines.push(line(&[cli, "help"]));
                let Ok(model) = ServiceModel::load(service) else {
                    continue;
                };
                for (name, entry) in service_commands(cli, &model) {
                    lines.push(line(&[cli, &name, "help"]));
                    if entry == ServiceEntry::Wait {
                        for (waiter, _) in waiter_commands(&model) {
                            lines.push(line(&[cli, "wait", &waiter, "help"]));
                        }
                    }
                }
            }
            MainCommand::Custom(name) => lines.push(line(&[name, "help"])),
        }
    }
    if let Some(custom) = HelpData::load("_custom") {
        for path in custom.command_paths() {
            let mut l: Vec<String> = path.split(' ').map(str::to_string).collect();
            l.push("help".to_string());
            lines.push(l);
        }
    }
    let mut seen = HashSet::new();
    lines
        .iter()
        .filter_map(|l| crate::driver::help_request(l))
        .filter(|r| seen.insert(key(r)))
        .collect()
}

pub fn key(req: &HelpRequest) -> String {
    format!("{req:?}")
}

pub fn render(req: &HelpRequest) -> Result<(Vec<u8>, Vec<u8>), String> {
    let published = super::docutils::publish_man(&super::document(req)?)?;
    let output = super::render::groff(published.man.as_bytes(), "ascii")?;
    Ok((published.stderr.into_bytes(), output))
}

pub struct Page {
    pub key: String,
    pub data: Vec<u8>,
    pub stderr_len: usize,
    pub raw_len: usize,
}

pub fn encode(version: &str, dict: &[u8], pages: &mut [Page]) -> Vec<u8> {
    pages.sort_by(|a, b| a.key.cmp(&b.key));
    let mut out = MAGIC.to_vec();
    for part in [version.as_bytes(), dict] {
        out.extend_from_slice(&(part.len() as u32).to_le_bytes());
        out.extend_from_slice(part);
    }
    out.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    let mut at = out.len() + pages.len() * ENTRY;
    let mut index = Vec::with_capacity(pages.len() * ENTRY);
    for page in pages.iter() {
        let key_at = at;
        at += page.key.len();
        let data_at = at;
        at += page.data.len();
        for v in [
            key_at,
            page.key.len(),
            data_at,
            page.data.len(),
            page.stderr_len,
            page.raw_len,
        ] {
            index.extend_from_slice(
                &u32::try_from(v)
                    .expect("help pages over 4 GiB")
                    .to_le_bytes(),
            );
        }
    }
    out.extend_from_slice(&index);
    for page in pages.iter() {
        out.extend_from_slice(page.key.as_bytes());
        out.extend_from_slice(&page.data);
    }
    out
}

pub fn groff_version() -> Option<String> {
    super::cache::run_groff_version(&super::render::find_on_path("groff")?)
}
