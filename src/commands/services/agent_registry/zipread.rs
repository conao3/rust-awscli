//! Reading a zip archive the way CPython 3.14's `zipfile.ZipFile(path)`
//! does (`_EndRecData`, `_EndRecData64`, `_RealGetContents`,
//! `ZipInfo._decodeExtra`, `ZipFile.open`, `ZipExtFile`), with the
//! exceptions that `install-skill` catches as a damaged archive
//! (`BadZipFile`, `zlib.error`, `EOFError`, `NotImplementedError`).

use std::io::Read as _;

/// `install_skill._ZIP_ERRORS`: `str(e)` of the exception.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZipError(pub String);

fn bad(msg: impl Into<String>) -> ZipError {
    ZipError(msg.into())
}

/// The parts of a `ZipInfo` the extractor reads.
#[derive(Debug, Clone)]
pub(crate) struct ZipInfo {
    /// `filename` (decoded, cut at a NUL).
    pub filename: String,
    /// `orig_filename`: the name as decoded, before the unicode path
    /// extra field.
    orig_filename: String,
    pub flag_bits: u16,
    pub compress_type: u16,
    pub crc: u32,
    pub compress_size: u64,
    pub file_size: u64,
    pub external_attr: u32,
    header_offset: u64,
    end_offset: u64,
}

impl ZipInfo {
    /// `ZipInfo.is_dir()`.
    pub(crate) fn is_dir(&self) -> bool {
        self.filename.ends_with('/')
    }
}

/// An open archive (`zipfile.ZipFile`), read into memory.
pub(crate) struct ZipFile {
    data: Vec<u8>,
    infos: Vec<ZipInfo>,
}

const CP437_HIGH: &str = "\u{c7}\u{fc}\u{e9}\u{e2}\u{e4}\u{e0}\u{e5}\u{e7}\u{ea}\u{eb}\u{e8}\u{ef}\u{ee}\u{ec}\u{c4}\u{c5}\u{c9}\u{e6}\u{c6}\u{f4}\u{f6}\u{f2}\u{fb}\u{f9}\u{ff}\u{d6}\u{dc}\u{a2}\u{a3}\u{a5}\u{20a7}\u{192}\u{e1}\u{ed}\u{f3}\u{fa}\u{f1}\u{d1}\u{aa}\u{ba}\u{bf}\u{2310}\u{ac}\u{bd}\u{bc}\u{a1}\u{ab}\u{bb}\u{2591}\u{2592}\u{2593}\u{2502}\u{2524}\u{2561}\u{2562}\u{2556}\u{2555}\u{2563}\u{2551}\u{2557}\u{255d}\u{255c}\u{255b}\u{2510}\u{2514}\u{2534}\u{252c}\u{251c}\u{2500}\u{253c}\u{255e}\u{255f}\u{255a}\u{2554}\u{2569}\u{2566}\u{2560}\u{2550}\u{256c}\u{2567}\u{2568}\u{2564}\u{2565}\u{2559}\u{2558}\u{2552}\u{2553}\u{256b}\u{256a}\u{2518}\u{250c}\u{2588}\u{2584}\u{258c}\u{2590}\u{2580}\u{3b1}\u{df}\u{393}\u{3c0}\u{3a3}\u{3c3}\u{b5}\u{3c4}\u{3a6}\u{398}\u{3a9}\u{3b4}\u{221e}\u{3c6}\u{3b5}\u{2229}\u{2261}\u{b1}\u{2265}\u{2264}\u{2320}\u{2321}\u{f7}\u{2248}\u{b0}\u{2219}\u{b7}\u{221a}\u{207f}\u{b2}\u{25a0}\u{a0}";

/// A name decoded as UTF-8 (flag bit 11) or cp437. `Err` is the
/// `UnicodeDecodeError`, which is not a damaged-archive error.
fn decode_name(bytes: &[u8], utf8: bool) -> Result<String, String> {
    if utf8 {
        return super::utils::decode_utf8(bytes);
    }
    let high: Vec<char> = CP437_HIGH.chars().collect();
    Ok(bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                high[(b - 0x80) as usize]
            }
        })
        .collect())
}

/// `_sanitize_filename`.
fn sanitize(name: &str) -> String {
    match name.find('\0') {
        Some(i) => name[..i].to_string(),
        None => name.to_string(),
    }
}

fn u16_at(d: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([d[i], d[i + 1]])
}

fn u32_at(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

fn u64_at(d: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(d[i..i + 8].try_into().unwrap())
}

const SIZE_END_CENT_DIR: usize = 22;
const SIZE_CENTRAL_DIR: usize = 46;
const SIZE_FILE_HEADER: usize = 30;
const SIZE_END_64_LOCATOR: usize = 20;
const SIZE_END_64: usize = 56;
const MAX_EXTRACT_VERSION: u8 = 63;

/// The end record: (size of the central directory, its offset, the
/// record's location).
struct EndRec {
    size: u64,
    offset: u64,
    location: u64,
}

/// What a read of the archive fails with: a damaged archive, or another
/// exception (`OSError`, `UnicodeDecodeError`) by `str(e)`.
#[derive(Debug)]
pub(crate) enum OpenError {
    Zip(ZipError),
    Other(String),
}

impl From<ZipError> for OpenError {
    fn from(e: ZipError) -> Self {
        OpenError::Zip(e)
    }
}

/// `_EndRecData64`.
fn end_rec64(d: &[u8], offset: usize, rec: EndRec) -> Result<EndRec, OpenError> {
    let Some(offset) = offset.checked_sub(SIZE_END_64_LOCATOR) else {
        return Ok(rec);
    };
    let loc = &d[offset..offset + SIZE_END_64_LOCATOR];
    if &loc[0..4] != b"PK\x06\x07" {
        return Ok(rec);
    }
    let diskno = u32_at(loc, 4);
    let reloff = u64_at(loc, 8);
    let disks = u32_at(loc, 16);
    if diskno != 0 || disks > 1 {
        return Err(bad("zipfiles that span multiple disks are not supported").into());
    }
    let Some(offset) = offset.checked_sub(SIZE_END_64) else {
        return Err(bad("Corrupt zip64 end of central directory locator").into());
    };
    if reloff > offset as u64 {
        return Err(bad("Corrupt zip64 end of central directory locator").into());
    }
    let mut extrasz = offset as u64 - reloff;
    let read = |at: usize| -> Result<&[u8], OpenError> {
        d.get(at..at + SIZE_END_64)
            .ok_or_else(|| OpenError::Other("Unknown I/O error".to_string()))
    };
    let mut data = read(reloff as usize)?;
    if !data.starts_with(b"PK\x06\x06") && reloff != offset as u64 {
        extrasz = 0;
        data = read(offset)?;
    }
    if !data.starts_with(b"PK\x06\x06") {
        return Err(bad("Zip64 end of central directory record not found").into());
    }
    let sz = u64_at(data, 4);
    let dirsize = u64_at(data, 40);
    let diroffset = u64_at(data, 48);
    if diroffset + dirsize != reloff || sz + 12 != SIZE_END_64 as u64 + extrasz {
        return Err(bad("Corrupt zip64 end of central directory record").into());
    }
    let _ = rec;
    Ok(EndRec {
        size: dirsize,
        offset: diroffset,
        location: offset as u64 - extrasz,
    })
}

/// `_EndRecData`.
fn end_rec(d: &[u8]) -> Result<Option<EndRec>, OpenError> {
    let n = d.len();
    if n >= SIZE_END_CENT_DIR {
        let data = &d[n - SIZE_END_CENT_DIR..];
        if &data[0..4] == b"PK\x05\x06" && data[20..22] == [0, 0] {
            let rec = EndRec {
                size: u32_at(data, 12) as u64,
                offset: u32_at(data, 16) as u64,
                location: (n - SIZE_END_CENT_DIR) as u64,
            };
            return end_rec64(d, n - SIZE_END_CENT_DIR, rec).map(Some);
        }
    } else {
        return Ok(None);
    }
    let max_comment_start = n.saturating_sub(65535 + SIZE_END_CENT_DIR);
    let data = &d[max_comment_start..];
    let Some(start) = data.windows(4).rposition(|w| w == b"PK\x05\x06") else {
        return Ok(None);
    };
    let Some(rec) = data.get(start..start + SIZE_END_CENT_DIR) else {
        return Ok(None);
    };
    let rec = EndRec {
        size: u32_at(rec, 12) as u64,
        offset: u32_at(rec, 16) as u64,
        location: (max_comment_start + start) as u64,
    };
    end_rec64(d, max_comment_start + start, rec).map(Some)
}

impl ZipFile {
    /// `zipfile.ZipFile(path)`.
    pub(crate) fn open(path: &str) -> Result<ZipFile, OpenError> {
        let data = std::fs::read(path)
            .map_err(|e| OpenError::Other(crate::cli::paramfile::os_error(&e, path)))?;
        let Some(rec) = end_rec(&data)? else {
            return Err(bad("File is not a zip file").into());
        };
        let concat = rec.location as i128 - rec.size as i128 - rec.offset as i128;
        let start_dir = rec.offset as i128 + concat;
        if start_dir < 0 {
            return Err(bad("Bad offset for central directory").into());
        }
        let start = (start_dir as usize).min(data.len());
        let end = start.saturating_add(rec.size as usize).min(data.len());
        let cd = &data[start..end];
        let size_cd = rec.size as usize;
        let mut pos = 0usize;
        let mut infos = Vec::new();
        while pos < size_cd {
            let Some(centdir) = cd.get(pos..pos + SIZE_CENTRAL_DIR) else {
                return Err(bad("Truncated central directory").into());
            };
            if &centdir[0..4] != b"PK\x01\x02" {
                return Err(bad("Bad magic number for central directory").into());
            }
            let name_len = u16_at(centdir, 28) as usize;
            let extra_len = u16_at(centdir, 30) as usize;
            let comment_len = u16_at(centdir, 32) as usize;
            let after = pos + SIZE_CENTRAL_DIR;
            let take = |from: usize, len: usize| -> &[u8] {
                let a = from.min(cd.len());
                let b = from.saturating_add(len).min(cd.len());
                &cd[a..b]
            };
            let name_bytes = take(after, name_len);
            let extra = take(after + name_len, extra_len).to_vec();
            let flags = u16_at(centdir, 8);
            let decoded = decode_name(name_bytes, flags & 0x800 != 0).map_err(OpenError::Other)?;
            let filename = sanitize(&decoded);
            let extract_version = centdir[6];
            if extract_version > MAX_EXTRACT_VERSION {
                return Err(OpenError::Zip(bad(format!(
                    "zip file version {:.1}",
                    extract_version as f64 / 10.0
                ))));
            }
            let mut info = ZipInfo {
                orig_filename: decoded.clone(),
                filename,
                flag_bits: flags,
                compress_type: u16_at(centdir, 10),
                crc: u32_at(centdir, 16),
                compress_size: u32_at(centdir, 20) as u64,
                file_size: u32_at(centdir, 24) as u64,
                external_attr: u32_at(centdir, 38),
                header_offset: u32_at(centdir, 42) as u64,
                end_offset: 0,
            };
            decode_extra(&mut info, &extra, crc32fast::hash(name_bytes))?;
            info.header_offset = (info.header_offset as i128 + concat).max(0) as u64;
            infos.push(info);
            pos = pos + SIZE_CENTRAL_DIR + name_len + extra_len + comment_len;
        }
        let mut order: Vec<usize> = (0..infos.len()).collect();
        order.sort_by_key(|&i| infos[i].header_offset);
        let mut end_offset = start_dir as u64;
        for &i in order.iter().rev() {
            infos[i].end_offset = end_offset;
            end_offset = infos[i].header_offset;
        }
        Ok(ZipFile { data, infos })
    }

    /// `infolist()`.
    pub(crate) fn infolist(&self) -> &[ZipInfo] {
        &self.infos
    }

    /// `zf.open(info).read(limit)` (all of it without a limit): the
    /// member's bytes; the CRC is checked once the end is read.
    pub(crate) fn read(&self, info: &ZipInfo, limit: Option<usize>) -> Result<Vec<u8>, OpenError> {
        let d = &self.data;
        let at = info.header_offset as usize;
        let Some(fheader) = d.get(at..at + SIZE_FILE_HEADER) else {
            return Err(bad("Truncated file header").into());
        };
        if &fheader[0..4] != b"PK\x03\x04" {
            return Err(bad("Bad magic number for file header").into());
        }
        let name_len = u16_at(fheader, 26) as usize;
        let extra_len = u16_at(fheader, 28) as usize;
        let name_start = at + SIZE_FILE_HEADER;
        let fname = &d[name_start.min(d.len())..(name_start + name_len).min(d.len())];
        if info.flag_bits & (1 << 5) != 0 {
            return Err(OpenError::Zip(bad("compressed patched data (flag bit 5)")));
        }
        if info.flag_bits & (1 << 6) != 0 {
            return Err(OpenError::Zip(bad("strong encryption (flag bit 6)")));
        }
        let fname_str =
            decode_name(fname, u16_at(fheader, 6) & 0x800 != 0).map_err(OpenError::Other)?;
        if fname_str != info.orig_filename {
            return Err(bad(format!(
                "File name in directory {} and header {} differ.",
                crate::pyfmt::str_repr(&info.orig_filename),
                crate::pyfmt::bytes_repr(fname)
            ))
            .into());
        }
        let data_start = name_start + name_len + extra_len;
        if data_start as u64 + info.compress_size > info.end_offset
            && info.end_offset != info.header_offset
        {
            return Err(bad(format!(
                "Overlapped entries: {} (possible zip bomb)",
                crate::pyfmt::str_repr(&info.orig_filename)
            ))
            .into());
        }
        if info.flag_bits & 1 != 0 {
            return Err(OpenError::Other(format!(
                "File {} is encrypted, password required for extraction",
                crate::pyfmt::str_repr(&info.filename)
            )));
        }
        let compressed_end = (data_start as u64 + info.compress_size).min(d.len() as u64) as usize;
        let compressed = &d[data_start.min(d.len())..compressed_end.max(data_start.min(d.len()))];
        let truncated = (compressed.len() as u64) < info.compress_size;
        let file_size = info.file_size as usize;
        let want = limit.map_or(file_size, |l| l.min(file_size));
        let (mut out, finished) = match info.compress_type {
            0 => {
                if truncated && compressed.len() < want {
                    return Err(OpenError::Zip(ZipError(String::new())));
                }
                (compressed.to_vec(), true)
            }
            8 => inflate(compressed, want, truncated)?,
            93 => {
                let mut decoder = zstd::stream::read::Decoder::new(compressed)
                    .map_err(|e| OpenError::Zip(bad(e.to_string())))?;
                let mut out = Vec::new();
                (&mut decoder)
                    .take(want as u64)
                    .read_to_end(&mut out)
                    .map_err(|e| OpenError::Zip(bad(e.to_string())))?;
                (out, true)
            }
            12 | 14 => {
                return Err(OpenError::Other(format!(
                    "compression type {} ({}) is not supported by this build",
                    info.compress_type,
                    if info.compress_type == 12 {
                        "bzip2"
                    } else {
                        "lzma"
                    }
                )));
            }
            _ => {
                return Err(bad("That compression method is not supported").into());
            }
        };
        out.truncate(file_size);
        let at_end = finished || out.len() >= file_size;
        let checked = limit.is_none_or(|l| out.len() <= l || at_end);
        if at_end && checked && crc32fast::hash(&out) != info.crc {
            return Err(bad(format!(
                "Bad CRC-32 for file {}",
                crate::pyfmt::str_repr(&info.filename)
            ))
            .into());
        }
        if let Some(l) = limit {
            out.truncate(l);
        }
        Ok(out)
    }
}

/// Raw deflate (`zlib.decompressobj(-15)`): the output (at most `want`
/// bytes more than needed are not produced) and whether the stream ended.
fn inflate(compressed: &[u8], want: usize, truncated: bool) -> Result<(Vec<u8>, bool), OpenError> {
    let mut d = flate2::Decompress::new(false);
    let mut out = Vec::with_capacity(want.min(1 << 20));
    let mut buf = vec![0u8; 65536];
    loop {
        let before_in = d.total_in() as usize;
        let before_out = d.total_out();
        let status = d
            .decompress(
                &compressed[before_in..],
                &mut buf,
                flate2::FlushDecompress::None,
            )
            .map_err(|e| {
                OpenError::Zip(bad(format!(
                    "Error -3 while decompressing data: {}",
                    e.message().unwrap_or("invalid data")
                )))
            })?;
        let produced = (d.total_out() - before_out) as usize;
        out.extend_from_slice(&buf[..produced]);
        if status == flate2::Status::StreamEnd {
            return Ok((out, true));
        }
        if out.len() > want {
            return Ok((out, false));
        }
        let consumed_all = d.total_in() as usize >= compressed.len();
        if consumed_all && produced == 0 {
            if truncated {
                return Err(OpenError::Zip(ZipError(String::new())));
            }
            return Ok((out, true));
        }
    }
}

/// `ZipInfo._decodeExtra`.
fn decode_extra(info: &mut ZipInfo, extra: &[u8], filename_crc: u32) -> Result<(), ZipError> {
    let mut extra = extra;
    while extra.len() >= 4 {
        let tp = u16_at(extra, 0);
        let ln = u16_at(extra, 2) as usize;
        if ln + 4 > extra.len() {
            return Err(bad(format!("Corrupt extra field {tp:04x} (size={ln})")));
        }
        let data = &extra[4..ln + 4];
        if tp == 0x0001 {
            let mut data = data;
            let mut take = |field: &str| -> Result<u64, ZipError> {
                if data.len() < 8 {
                    return Err(bad(format!(
                        "Corrupt zip64 extra field. {field} not found."
                    )));
                }
                let v = u64_at(data, 0);
                data = &data[8..];
                Ok(v)
            };
            if info.file_size == 0xFFFF_FFFF {
                info.file_size = take("File size")?;
            }
            if info.compress_size == 0xFFFF_FFFF {
                info.compress_size = take("Compress size")?;
            }
            if info.header_offset == 0xFFFF_FFFF {
                info.header_offset = take("Header offset")?;
            }
        } else if tp == 0x7075 {
            if data.len() < 5 {
                return Err(bad("Corrupt unicode path extra field (0x7075)"));
            }
            let up_version = data[0];
            let up_crc = u32_at(data, 1);
            if up_version == 1 && up_crc == filename_crc {
                let name = std::str::from_utf8(&data[5..]).map_err(|_| {
                    bad("Corrupt unicode path extra field (0x7075): invalid utf-8 bytes")
                })?;
                if !name.is_empty() {
                    info.filename = sanitize(name);
                }
            }
        }
        extra = &extra[ln + 4..];
    }
    Ok(())
}
