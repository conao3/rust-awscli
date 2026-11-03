//! `artifact_exporter.make_zip`: the archive Python's `zipfile.ZipFile(f,
//! 'w', ZIP_DEFLATED)` writes for every file under a folder
//! (`os.walk(followlinks=True)` order, `ZipInfo.from_file` metadata, raw
//! deflate at the default level, no data descriptors).

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::DeflateEncoder;

struct Entry {
    name: Vec<u8>,
    flags: u16,
    time: u16,
    date: u16,
    crc: u32,
    compressed: Vec<u8>,
    size: u32,
    external: u32,
    offset: u32,
}

/// `os.walk(root, followlinks=True)`: the files, top-down.
fn walk(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
            dirs.push(path);
        } else {
            out.push(path);
        }
    }
    for d in dirs {
        walk(&d, out)?;
    }
    Ok(())
}

/// `time.localtime(mtime)[:6]` as DOS time and date.
fn dos_time(mtime: i64) -> (u16, u16) {
    let t = mtime as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes into `tm`.
    unsafe { libc::localtime_r(&t, &mut tm) };
    let year = (tm.tm_year + 1900).max(1980);
    let time = ((tm.tm_hour as u16) << 11) | ((tm.tm_min as u16) << 5) | ((tm.tm_sec as u16) / 2);
    let date = (((year - 1980) as u16) << 9) | (((tm.tm_mon + 1) as u16) << 5) | tm.tm_mday as u16;
    (time, date)
}

/// Write `zip_path` with every file under `source_root`.
pub(super) fn make_zip(zip_path: &Path, source_root: &Path) -> std::io::Result<()> {
    let mut files = Vec::new();
    walk(source_root, &mut files)?;
    let mut out: Vec<u8> = Vec::new();
    let mut entries = Vec::new();
    for full in files {
        let rel = full.strip_prefix(source_root).unwrap_or(&full);
        let name = rel.to_string_lossy().into_owned();
        let meta = std::fs::metadata(&full)?;
        let data = std::fs::read(&full)?;
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        enc.write_all(&data)?;
        let compressed = enc.finish()?;
        let (time, date) = dos_time(meta.mtime());
        let flags = if name.is_ascii() { 0 } else { 0x800 };
        let e = Entry {
            name: name.into_bytes(),
            flags,
            time,
            date,
            crc: crc32fast::hash(&data),
            compressed,
            size: data.len() as u32,
            external: (meta.mode() & 0xFFFF) << 16,
            offset: out.len() as u32,
        };
        out.extend_from_slice(b"PK\x03\x04");
        for v in [20u16, e.flags, 8, e.time, e.date] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [e.crc, e.compressed.len() as u32, e.size] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&e.compressed);
        entries.push(e);
    }
    let central_start = out.len() as u32;
    for e in &entries {
        out.extend_from_slice(b"PK\x01\x02");
        for v in [0x0314u16, 20, e.flags, 8, e.time, e.date] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [e.crc, e.compressed.len() as u32, e.size] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [e.name.len() as u16, 0, 0, 0, 0] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&e.external.to_le_bytes());
        out.extend_from_slice(&e.offset.to_le_bytes());
        out.extend_from_slice(&e.name);
    }
    let central_size = out.len() as u32 - central_start;
    out.extend_from_slice(b"PK\x05\x06");
    for v in [0u16, 0, entries.len() as u16, entries.len() as u16] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    std::fs::write(zip_path, out)
}

/// `zipfile.is_zipfile`: an end of central directory record near the end.
pub(super) fn is_zipfile(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    let start = data.len().saturating_sub(22 + 65_535);
    data.len() >= 22 && data[start..].windows(4).any(|w| w == b"PK\x05\x06")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The archive the reference wrote for the same file (the DOS
    /// timestamps blanked).
    #[test]
    fn matches_cpython_for_a_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("code.txt"), "print('hi')\n").unwrap();
        std::fs::set_permissions(
            src.join("code.txt"),
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
        let zip = dir.path().join("a.zip");
        make_zip(&zip, &src).unwrap();
        let blank = |mut b: Vec<u8>| {
            b[10..14].copy_from_slice(&[0, 0, 0, 0]);
            b[52 + 12..52 + 16].copy_from_slice(&[0, 0, 0, 0]);
            b
        };
        let reference = hex::decode("504b0304140000000800888b635dafa69ef80e0000000c00000008000000636f64652e7478742b28cacc2bd150cfc854d7e40200504b01021403140000000800888b635dafa69ef80e0000000c000000080000000000000000000000a48100000000636f64652e747874504b0506000000000100010036000000340000000000").unwrap();
        assert_eq!(
            hex::encode(blank(std::fs::read(&zip).unwrap())),
            hex::encode(blank(reference))
        );
        assert!(is_zipfile(&zip));
    }
}
