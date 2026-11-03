//! Writing a zip archive the way Python's `zipfile.ZipFile(f, 'w')` with
//! `ZipFile.write(filename, arcname, ZIP_DEFLATED)` does
//! (`zipfile.ZipInfo.from_file`, `_open_to_write`, `FileHeader`,
//! `_write_end_record`).

use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::DeflateEncoder;

/// `ZIP64_LIMIT`.
const ZIP64_LIMIT: u64 = (1 << 31) - 1;

struct Entry {
    name: Vec<u8>,
    flags: u16,
    dostime: u16,
    dosdate: u16,
    crc: u32,
    compress_size: u64,
    file_size: u64,
    external_attr: u32,
    offset: u64,
    zip64: bool,
}

/// An archive being written into memory.
#[derive(Default)]
pub(crate) struct ZipWriter {
    out: Vec<u8>,
    entries: Vec<Entry>,
}

/// `time.localtime(secs)[0:6]`.
fn localtime(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    (
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
    )
}

/// The `arcname` normalization of `ZipInfo.from_file`
/// (`os.path.normpath`, leading separators removed).
pub(crate) fn normalize_arcname(arcname: &str) -> String {
    let absolute = arcname.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for p in arcname.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    if joined.is_empty() && !absolute {
        ".".to_string()
    } else {
        joined
    }
}

impl ZipWriter {
    pub(crate) fn new() -> ZipWriter {
        ZipWriter::default()
    }

    /// `ZipFile.write(path, arcname, ZIP_DEFLATED)` for a regular file.
    pub(crate) fn write_file(&mut self, path: &Path, arcname: &str) -> std::io::Result<()> {
        let meta = std::fs::metadata(path)?;
        let data = std::fs::read(path)?;
        let (year, month, day, hour, minute, second) = localtime(meta.mtime());
        if year < 1980 {
            return Err(std::io::Error::other(
                "ZIP does not support timestamps before 1980",
            ));
        }
        let name = normalize_arcname(arcname);
        let flags: u16 = if name.is_ascii() { 0 } else { 0x800 };
        let dostime = ((hour << 11) | (minute << 5) | (second / 2)) as u16;
        let dosdate = ((((year - 1980) as u32) << 9) | (month << 5) | day) as u16;
        let file_size = meta.len();
        let zip64 = file_size as f64 * 1.05 > ZIP64_LIMIT as f64;
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        for chunk in data.chunks(8192) {
            enc.write_all(chunk)?;
        }
        let compressed = enc.finish()?;
        let crc = crc32fast::hash(&data);
        let entry = Entry {
            name: name.into_bytes(),
            flags,
            dostime,
            dosdate,
            crc,
            compress_size: compressed.len() as u64,
            file_size: data.len() as u64,
            external_attr: (meta.mode() & 0xFFFF) << 16,
            offset: self.out.len() as u64,
            zip64,
        };
        self.local_header(&entry);
        self.out.extend_from_slice(&compressed);
        self.entries.push(entry);
        Ok(())
    }

    fn local_header(&mut self, e: &Entry) {
        let mut extra = Vec::new();
        let (csize, usize_) = if e.zip64 {
            extra.extend_from_slice(&1u16.to_le_bytes());
            extra.extend_from_slice(&16u16.to_le_bytes());
            extra.extend_from_slice(&e.file_size.to_le_bytes());
            extra.extend_from_slice(&e.compress_size.to_le_bytes());
            (0xFFFF_FFFFu32, 0xFFFF_FFFFu32)
        } else {
            (e.compress_size as u32, e.file_size as u32)
        };
        let version: u8 = if e.zip64 { 45 } else { 20 };
        let o = &mut self.out;
        o.extend_from_slice(b"PK\x03\x04");
        o.push(version);
        o.push(0);
        o.extend_from_slice(&e.flags.to_le_bytes());
        o.extend_from_slice(&8u16.to_le_bytes());
        o.extend_from_slice(&e.dostime.to_le_bytes());
        o.extend_from_slice(&e.dosdate.to_le_bytes());
        o.extend_from_slice(&e.crc.to_le_bytes());
        o.extend_from_slice(&csize.to_le_bytes());
        o.extend_from_slice(&usize_.to_le_bytes());
        o.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        o.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        o.extend_from_slice(&e.name);
        o.extend_from_slice(&extra);
    }

    /// `ZipFile.close()`: the central directory and the end record.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        let start = self.out.len() as u64;
        for e in &self.entries {
            let mut extra_values: Vec<u64> = Vec::new();
            let file_size = if e.file_size > ZIP64_LIMIT {
                extra_values.push(e.file_size);
                0xFFFF_FFFF
            } else {
                e.file_size as u32
            };
            let compress_size = if e.compress_size > ZIP64_LIMIT {
                extra_values.push(e.compress_size);
                0xFFFF_FFFF
            } else {
                e.compress_size as u32
            };
            let offset = if e.offset > ZIP64_LIMIT {
                extra_values.push(e.offset);
                0xFFFF_FFFF
            } else {
                e.offset as u32
            };
            let mut extra = Vec::new();
            let min_version: u8 = if extra_values.is_empty() {
                0
            } else {
                extra.extend_from_slice(&1u16.to_le_bytes());
                extra.extend_from_slice(&((8 * extra_values.len()) as u16).to_le_bytes());
                for v in &extra_values {
                    extra.extend_from_slice(&v.to_le_bytes());
                }
                45
            };
            let base: u8 = if e.zip64 { 45 } else { 20 };
            let version = base.max(min_version);
            let o = &mut self.out;
            o.extend_from_slice(b"PK\x01\x02");
            o.push(version);
            o.push(3);
            o.push(version);
            o.push(0);
            o.extend_from_slice(&e.flags.to_le_bytes());
            o.extend_from_slice(&8u16.to_le_bytes());
            o.extend_from_slice(&e.dostime.to_le_bytes());
            o.extend_from_slice(&e.dosdate.to_le_bytes());
            o.extend_from_slice(&e.crc.to_le_bytes());
            o.extend_from_slice(&compress_size.to_le_bytes());
            o.extend_from_slice(&file_size.to_le_bytes());
            o.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            o.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            o.extend_from_slice(&0u16.to_le_bytes());
            o.extend_from_slice(&0u16.to_le_bytes());
            o.extend_from_slice(&0u16.to_le_bytes());
            o.extend_from_slice(&e.external_attr.to_le_bytes());
            o.extend_from_slice(&offset.to_le_bytes());
            o.extend_from_slice(&e.name);
            o.extend_from_slice(&extra);
        }
        let end = self.out.len() as u64;
        let count = self.entries.len() as u64;
        let size = end - start;
        let o = &mut self.out;
        o.extend_from_slice(b"PK\x05\x06");
        o.extend_from_slice(&0u16.to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes());
        o.extend_from_slice(&(count.min(0xFFFF) as u16).to_le_bytes());
        o.extend_from_slice(&(count.min(0xFFFF) as u16).to_le_bytes());
        o.extend_from_slice(&(size.min(0xFFFF_FFFF) as u32).to_le_bytes());
        o.extend_from_slice(&(start.min(0xFFFF_FFFF) as u32).to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes());
        self.out
    }
}

/// `os.walk(top, topdown=True)`: (root, dirs, files) in `scandir` order;
/// `filter` prunes names (both lists) before recursing.
pub(crate) fn walk(
    top: &Path,
    filter: &dyn Fn(&str) -> bool,
) -> Vec<(PathBuf, Vec<String>, Vec<String>)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(top) else {
        return out;
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir());
        if is_dir {
            dirs.push(name);
        } else {
            files.push(name);
        }
    }
    files.retain(|f| filter(f));
    dirs.retain(|d| filter(d));
    out.push((top.to_path_buf(), dirs.clone(), files));
    for d in dirs {
        let p = top.join(&d);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        out.extend(walk(&p, filter));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arcnames() {
        assert_eq!(normalize_arcname("a/./b//c"), "a/b/c");
        assert_eq!(normalize_arcname("/abs/x"), "abs/x");
        assert_eq!(normalize_arcname("a/../b"), "b");
    }
}
