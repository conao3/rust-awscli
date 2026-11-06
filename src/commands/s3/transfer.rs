//! `aws s3 cp`, `mv`, `rm`, and `sync` (`awscli/customizations/s3/
//! subcommands.py` `S3TransferCommand`, `CommandArchitecture`,
//! `CommandParameters`; `fileformat.py`, `filegenerator.py`,
//! `filters.py`, `comparator.py`, `syncstrategy/`, `s3handler.py`,
//! `subscribers.py`, `transferconfig.py`, `factory.py`; and the classic
//! transfer client `s3transfer`).

use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::S3Fail;
use super::args::Args;
use super::client::{CallError, Client, map};
use super::engine::{Limiter, Pool};
use super::results::{Loc, Mode, Processor, TResult, now};
use super::utils::{
    Instant, abspath, basename, dirname, find_bucket_key, fnmatch, guess_content_type,
    human_readable_size, human_readable_to_int, is_s3express_bucket, join, mapper, normpath,
    parse_timestamp, relative_path, split_s3_bucket_key, warning_message,
};
use crate::cli::Fail;
use crate::config::ConfigLookup;
use crate::driver::Context;
use crate::error::ExitCode;
use crate::session::url::quote;
use crate::value::{Map, Value};

const MB: i64 = 1024 * 1024;
const MAX_UPLOAD_SIZE: i64 = 5 * 1024 * 1024 * 1024 * 10000;
const MAX_PARTS: i64 = 10000;
const MAX_SINGLE_UPLOAD_SIZE: i64 = 5 * 1024 * 1024 * 1024;
const MIN_UPLOAD_CHUNKSIZE: i64 = 5 * MB;
const FULL_OBJECT_CHECKSUM_ARGS: &[&str] = &[
    "ChecksumCRC32",
    "ChecksumCRC32C",
    "ChecksumCRC64NVME",
    "ChecksumMD5",
    "ChecksumSHA1",
    "ChecksumSHA256",
    "ChecksumSHA512",
    "ChecksumXXHASH3",
    "ChecksumXXHASH64",
    "ChecksumXXHASH128",
];

fn os_error(e: &std::io::Error, path: &str) -> String {
    crate::cli::paramfile::os_error(e, path)
}

// ---------------------------------------------------------------------
// Parameters (`CommandParameters`).

struct Params {
    cmd: String,
    a: Args,
}

impl Params {
    fn s(&self, k: &str) -> Option<&str> {
        self.a.str(k)
    }
    fn t(&self, k: &str) -> bool {
        self.a.truthy(k)
    }
    fn set(&mut self, k: &str, v: Value) {
        self.a.set(k, v);
    }
    fn src(&self) -> &str {
        self.s("src").unwrap_or_default()
    }
    fn dest(&self) -> &str {
        self.s("dest").unwrap_or_default()
    }
    fn paths_type(&self) -> &str {
        self.s("paths_type").unwrap_or_default()
    }
}

fn usage(cmd: &str) -> &'static str {
    match cmd {
        "rm" => "<S3Uri>",
        _ => "<LocalPath> <S3Uri> or <S3Uri> <LocalPath> or <S3Uri> <S3Uri>",
    }
}

fn pv(msg: impl Into<String>) -> S3Fail {
    S3Fail::Fail(Fail::ParamValidation(msg.into()))
}

/// `ensure_boolean`.
fn ensure_boolean(v: Option<&str>) -> bool {
    v.is_some_and(|s| s.to_lowercase() == "true")
}

fn build_params(ctx: &Context, cmd: &str, args: Args) -> Result<Params, S3Fail> {
    let mut p = Params {
        cmd: cmd.to_string(),
        a: args,
    };
    if p.a.0.get("dir_op").is_none() {
        p.set("dir_op", Value::Bool(false));
    }
    if p.a.0.get("follow_symlinks").is_none() {
        p.set("follow_symlinks", Value::Bool(true));
    }
    if p.a.0.get("source_region").is_none() {
        p.set("source_region", Value::Null);
    }
    if matches!(cmd, "sync" | "mb" | "rb") {
        p.set("dir_op", Value::Bool(true));
    }
    p.set("is_move", Value::Bool(cmd == "mv"));
    let _ = ctx;
    let mut paths: Vec<String> = match p.a.get("paths") {
        Some(Value::List(l)) => l
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect(),
        Some(Value::Str(s)) => vec![s.clone()],
        _ => vec![],
    };
    // `check_path_type`.
    let mut paths_type = String::new();
    for path in &paths {
        paths_type.push_str(if path.starts_with("s3://") {
            "s3"
        } else {
            "local"
        });
    }
    let allowed = match paths_type.as_str() {
        "s3s3" | "s3local" | "locals3" => ["cp", "sync", "mv"].contains(&cmd),
        "s3" => ["mb", "rb", "rm"].contains(&cmd),
        _ => false,
    };
    if !allowed {
        return Err(pv(format!(
            "usage: awsr s3 {cmd} {}\nError: Invalid argument type",
            usage(cmd)
        )));
    }
    p.set("paths_type", Value::str(&paths_type));
    // `_normalize_s3_trailing_slash`.
    for path in &mut paths {
        if let Some(rest) = path.strip_prefix("s3://") {
            let (_, key) = find_bucket_key(rest)?;
            if key.is_empty() && !path.ends_with('/') {
                path.push('/');
            }
        }
    }
    p.set("src", Value::str(&paths[0]));
    p.set("dest", Value::str(paths.get(1).unwrap_or(&paths[0])));
    // `_validate_streaming_paths`.
    p.set("is_stream", Value::Bool(false));
    if p.src() == "-" || p.dest() == "-" {
        if cmd != "cp" || p.t("dir_op") {
            return Err(pv(
                "Streaming currently is only compatible with non-recursive cp commands",
            ));
        }
        p.set("is_stream", Value::Bool(true));
        p.set("dir_op", Value::Bool(false));
        p.set("only_show_errors", Value::Bool(true));
    }
    // `_validate_no_overwrite_for_download_streaming`.
    if p.t("is_stream") && p.t("no_overwrite") && p.dest() == "-" {
        return Err(pv(
            "--no-overwrite parameter is not supported for streaming downloads",
        ));
    }
    validate_path_args(&mut p)?;
    // `_validate_sse_c_args`.
    for (t, k) in [
        ("sse_c", "sse_c_key"),
        ("sse_c_copy_source", "sse_c_copy_source_key"),
    ] {
        let tp = format!("--{}", t.replace('_', "-"));
        let kp = format!("--{}", k.replace('_', "-"));
        if p.t(t) && !p.t(k) {
            return Err(pv(format!(
                "If {tp} is specified, {kp} must be specified as well."
            )));
        }
        if p.t(k) && !p.t(t) {
            return Err(pv(format!(
                "If {kp} is specified, {tp} must be specified as well."
            )));
        }
    }
    if p.t("sse_c_copy_source") && p.paths_type() != "s3s3" {
        return Err(pv(
            "--sse-c-copy-source is only supported for copy operations.",
        ));
    }
    if cmd == "sync" {
        for path in [p.src().to_string(), p.dest().to_string()] {
            if path.starts_with("s3://") && is_s3express_bucket(&split_s3_bucket_key(&path)?.0) {
                return Err(pv("Cannot use sync command with a directory bucket."));
            }
        }
    }
    Ok(p)
}

fn same_path(p: &Params, src: &str, dest: &str) -> bool {
    if p.paths_type() != "s3s3" {
        return false;
    }
    if src == dest {
        return true;
    }
    if dest.ends_with('/') {
        return src == join(dest, basename(src));
    }
    false
}

/// `S3PathResolver.has_underlying_s3_path`.
fn has_underlying_s3_path(path: &str) -> bool {
    let Ok((bucket, _)) = split_s3_bucket_key(path) else {
        return false;
    };
    let ap = regex::Regex::new(
        r"^arn:aws.*:s3:[a-z0-9\-]+:(?P<account>[0-9]{12}):accesspoint[:/](?P<name>[a-z0-9\-]{3,50})$",
    )
    .expect("regex");
    let op = regex::Regex::new(
        r"^arn:aws.*:s3-outposts:[a-z0-9\-]+:(?P<account>[0-9]{12}):outpost/op-[a-zA-Z0-9]+/accesspoint[:/][a-z0-9\-]{3,50}$",
    )
    .expect("regex");
    let mrap = regex::Regex::new(
        r"^arn:aws:s3::(?P<account>[0-9]{12}):accesspoint[:/](?P<alias>[a-zA-Z0-9]+\.mrap)$",
    )
    .expect("regex");
    ap.is_match(&bucket)
        || op.is_match(&bucket)
        || mrap.is_match(&bucket)
        || bucket.ends_with("-s3alias")
        || bucket.ends_with("--op-s3")
}

fn validate_path_args(p: &mut Params) -> Result<(), S3Fail> {
    if p.cmd == "mv" && p.paths_type() == "s3s3" {
        let (src, dest) = (p.src().to_string(), p.dest().to_string());
        if same_path(p, &src, &dest) {
            return Err(pv(format!("Cannot mv a file onto itself: {src} - {dest}")));
        }
        let (_, sk) = split_s3_bucket_key(&src)?;
        let (_, dk) = split_s3_bucket_key(&dest)?;
        let same_key = same_path(p, &format!("/{sk}"), &format!("/{dk}"));
        let enabled = p.t("validate_same_s3_paths")
            || ensure_boolean(
                std::env::var("AWS_CLI_S3_MV_VALIDATE_SAME_S3_PATHS")
                    .ok()
                    .as_deref(),
            );
        let underlying = has_underlying_s3_path(&src) || has_underlying_s3_path(&dest);
        if same_key && enabled && underlying {
            return Err(S3Fail::Other(anyhow::anyhow!(
                "awsr: resolving access point paths for --validate-same-s3-paths is not implemented"
            )));
        }
        if same_key && !enabled && underlying {
            super::err(b"warning: Provided s3 paths may resolve to same underlying s3 object(s) and result in deletion instead of being moved. To resolve and validate underlying s3 paths are not the same, specify the --validate-same-s3-paths flag or set the AWS_CLI_S3_MV_VALIDATE_SAME_S3_PATHS environment variable to true. To resolve s3 outposts access point path, the arn must be used instead of the alias.\n");
        }
    }
    let usage_of = |t: &str| match t {
        "locals3" => "<LocalPath> <S3Uri>",
        "s3s3" => "<S3Uri> <S3Uri>",
        "s3local" => "<S3Uri> <LocalPath>",
        _ => "<S3Uri>",
    };
    let check = |param: &str, allowed: &[&str], pt: &str| -> Result<(), S3Fail> {
        if !allowed.contains(&pt) {
            return Err(pv(format!(
                "Expected {param} parameter to be used with one of following path formats: {}. Instead, received {}.",
                allowed
                    .iter()
                    .map(|a| usage_of(a))
                    .collect::<Vec<_>>()
                    .join(", "),
                usage_of(pt)
            )));
        }
        Ok(())
    };
    let pt = p.paths_type().to_string();
    if p.t("checksum_algorithm") {
        check("checksum-algorithm", &["locals3", "s3s3"], &pt)?;
    }
    if p.t("checksum_mode") {
        check("checksum-mode", &["s3local"], &pt)?;
    }
    if p.t("tags") {
        check("tags", &["locals3"], &pt)?;
    }
    if pt == "locals3" && !p.t("is_stream") {
        if std::fs::metadata(p.src()).is_err() {
            return Err(S3Fail::Other(anyhow::anyhow!(
                "The user-provided path {} does not exist.",
                p.src()
            )));
        }
    } else if pt == "s3local" && p.t("dir_op") && std::fs::metadata(p.dest()).is_err() {
        std::fs::create_dir_all(p.dest()).map_err(|e| S3Fail::Other(e.into()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Runtime configuration (`transferconfig.py`, `factory.py`).

#[derive(Debug, Clone)]
pub(crate) struct TransferConfig {
    pub multipart_threshold: i64,
    pub multipart_chunksize: i64,
    pub max_concurrent_requests: i64,
    pub max_queue_size: i64,
    pub max_bandwidth: Option<i64>,
}

fn runtime_config(ctx: &Context, p: &Params) -> Result<TransferConfig, S3Fail> {
    let scoped = ctx
        .session
        .config()
        .scoped_config()
        .ok()
        .and_then(|s| s.get("s3"))
        .and_then(|v| v.as_nested())
        .cloned()
        .unwrap_or_default();
    let invalid = |msg: String| S3Fail::Other(anyhow::anyhow!(msg));
    let get = |k: &str| scoped.get(k).cloned();
    let size = |k: &str, default: i64| -> Result<i64, S3Fail> {
        match get(k) {
            None => Ok(default),
            Some(v) => human_readable_to_int(&v).map_err(invalid),
        }
    };
    let rate = |k: &str| -> Result<Option<i64>, S3Fail> {
        let Some(v) = get(k) else {
            return Ok(None);
        };
        let is_int = |s: &str| crate::cli::pynum::py_int(s).is_some();
        if v.ends_with("B/s") {
            let body = &v[..v.len() - 3];
            let n = if is_int(body) {
                human_readable_to_int(body)
            } else {
                human_readable_to_int(&v[..v.len() - 2])
            };
            n.map(Some).map_err(invalid)
        } else if v.ends_with("b/s") {
            let body = &v[..v.len() - 3];
            let n = if is_int(body) {
                human_readable_to_int(body)
            } else {
                human_readable_to_int(&v[..v.len() - 2])
            };
            n.map(|b| Some(b / 8)).map_err(invalid)
        } else if is_int(&v) {
            Ok(Some(human_readable_to_int(&v).map_err(invalid)?))
        } else {
            Err(invalid(format!(
                "Invalid rate: {v}. The value must be expressed as an integer in terms of bytes per second (e.g. 10485760) or a rate in terms of bytes per second (e.g. 10MB/s or 800KB/s) or bits per second (e.g. 10Mb/s or 800Kb/s)"
            )))
        }
    };
    let multipart_chunksize = size("multipart_chunksize", 8 * MB)?;
    let multipart_threshold = size("multipart_threshold", 8 * MB)?;
    let io_chunksize = size("io_chunksize", 256 * 1024)?;
    let max_bandwidth = rate("max_bandwidth")?;
    let target_bandwidth = rate("target_bandwidth")?;
    let disk_throughput = rate("disk_throughput")?;
    let positive = |k: &str, v: Option<String>| -> Result<Option<i64>, S3Fail> {
        let Some(v) = v else {
            return Ok(None);
        };
        match crate::cli::pynum::py_int(&v).and_then(|b| i64::try_from(b).ok()) {
            Some(n) if n > 0 => Ok(Some(n)),
            _ => Err(invalid(format!(
                "Value for {k} must be a positive integer: {v}"
            ))),
        }
    };
    let mcr = positive("max_concurrent_requests", get("max_concurrent_requests"))?.unwrap_or(10);
    let mqs = positive("max_queue_size", get("max_queue_size"))?.unwrap_or(1000);
    for (k, v) in [
        ("multipart_chunksize", Some(multipart_chunksize)),
        ("multipart_threshold", Some(multipart_threshold)),
        ("max_bandwidth", max_bandwidth),
        ("target_bandwidth", target_bandwidth),
        ("io_chunksize", Some(io_chunksize)),
        ("disk_throughput", disk_throughput),
    ] {
        if let Some(v) = v
            && v <= 0
        {
            return Err(invalid(format!(
                "Value for {k} must be a positive integer: {v}"
            )));
        }
    }
    for k in ["should_stream", "direct_io"] {
        if let Some(v) = get(k) {
            let _ = ensure_boolean(Some(&v));
        }
    }
    let preferred = match get("preferred_transfer_client").as_deref() {
        None => "auto".to_string(),
        Some("default") => "classic".to_string(),
        Some(v) => v.to_string(),
    };
    if !["auto", "classic", "crt"].contains(&preferred.as_str()) {
        return Err(invalid(format!(
            "Invalid value: \"{preferred}\" for configuration option: \"preferred_transfer_client\". Supported values are: auto, classic, crt"
        )));
    }
    // `TransferManagerFactory`: the classic client is used; report the
    // settings the reference reports while choosing.
    let explicit = |k: &str| scoped.contains_key(k);
    if preferred == "auto" && p.paths_type() != "s3s3" {
        let classic_only: Vec<&str> = if explicit("max_bandwidth") {
            vec!["max_bandwidth"]
        } else {
            vec![]
        };
        let mut unsupported: Vec<String> = classic_only.iter().map(|s| s.to_string()).collect();
        if ctx.session.config_variable("retry_mode").as_deref() == Some("adaptive") {
            unsupported.push("retry_mode = adaptive".into());
        }
        if p.t("is_stream") && p.paths_type() == "locals3" {
            unsupported.push("uploads from a non-seekable stream".into());
        }
        if let Some(n) = ctx
            .session
            .config_variable("max_attempts")
            .and_then(|v| v.trim().parse::<i64>().ok())
            && !(2..=64).contains(&n)
        {
            unsupported.push(format!("max_attempts = {n} (must be between 2 and 64)"));
        }
        if !unsupported.is_empty() && !classic_only.is_empty() {
            super::err(format!("warning: Using the 'classic' s3 transfer client because the 'crt' s3 transfer client does not support: {}. A future version of the AWS CLI will use the 'crt' s3 transfer client by default, at which point these values will be ignored. Set the preferred_transfer_client configuration value to 'classic' to continue using the 'classic' s3 transfer client.\n", classic_only.join(", ")).as_bytes());
        }
    }
    let ignored: Vec<&str> = [
        "target_bandwidth",
        "should_stream",
        "disk_throughput",
        "direct_io",
    ]
    .into_iter()
    .filter(|k| explicit(k))
    .collect();
    if !ignored.is_empty() {
        super::err(format!("warning: The following configuration values are not supported by the 'classic' s3 transfer client and will be ignored: {}.\n", ignored.join(", ")).as_bytes());
    }
    Ok(TransferConfig {
        multipart_threshold,
        multipart_chunksize,
        max_concurrent_requests: mcr,
        max_queue_size: mqs,
        max_bandwidth,
    })
}

// ---------------------------------------------------------------------
// Files (`FileFormat`, `FileGenerator`, `FileStat`).

#[derive(Debug, Clone)]
struct Files {
    src_path: String,
    src_type: &'static str,
    dest_path: String,
    dir_op: bool,
    use_src_name: bool,
}

fn identify(path: &str) -> (&'static str, String) {
    match path.strip_prefix("s3://") {
        Some(rest) => ("s3", rest.to_string()),
        None => ("local", path.to_string()),
    }
}

fn local_format(path: &str, dir_op: bool) -> (String, bool) {
    let full = abspath(path);
    if std::fs::metadata(&full).is_ok_and(|m| m.is_dir()) || dir_op || path.ends_with('/') {
        (format!("{full}/"), true)
    } else {
        (full, false)
    }
}

fn s3_format(path: &str, dir_op: bool) -> (String, bool) {
    if dir_op {
        if path.ends_with('/') {
            (path.to_string(), true)
        } else {
            (format!("{path}/"), true)
        }
    } else {
        let ends = path.ends_with('/');
        (path.to_string(), ends)
    }
}

/// `FileFormat.format`.
fn format_files(src: &str, dest: &str, dir_op: bool) -> Files {
    let (st, sp) = identify(src);
    let (dt, dp) = identify(dest);
    let fmt = |t: &str, p: &str| {
        if t == "s3" {
            s3_format(p, dir_op)
        } else {
            local_format(p, dir_op)
        }
    };
    let (src_path, _) = fmt(st, &sp);
    let (dest_path, use_src_name) = fmt(dt, &dp);
    Files {
        src_path,
        src_type: st,
        dest_path,
        dir_op,
        use_src_name,
    }
}

/// `find_dest_path_comp_key`.
fn dest_path_comp_key(files: &Files, src_path: Option<&str>) -> (String, String) {
    let src_path = src_path.unwrap_or(&files.src_path);
    let rel = if files.dir_op {
        src_path
            .get(files.src_path.len()..)
            .unwrap_or_default()
            .to_string()
    } else {
        src_path.rsplit('/').next().unwrap_or(src_path).to_string()
    };
    let dest = if files.use_src_name {
        format!("{}{rel}", files.dest_path)
    } else {
        files.dest_path.clone()
    };
    (dest, rel)
}

/// `FileStat` / `FileInfo`.
#[derive(Debug, Clone)]
struct FileInfo {
    src: String,
    dest: String,
    compare_key: String,
    size: Option<i64>,
    last_update: Option<Instant>,
    src_type: &'static str,
    operation_name: String,
    response_data: Option<Map>,
    etag: Option<String>,
    case_conflict: Option<(Arc<Mutex<HashSet<String>>>, String)>,
}

type Item = Result<FileInfo, S3Fail>;

/// The size, `LastModified`, ETag, and response of a `HeadObject`.
type SingleObject = (Option<i64>, Option<Instant>, Option<String>, Option<Map>);

struct Generator<'a> {
    client: &'a Client<'a>,
    operation_name: String,
    follow_symlinks: bool,
    page_size: Option<i64>,
    results: Sender<TResult>,
    head_params: Map,
    list_params: Map,
    response_checksum_when_supported: bool,
}

fn warn(results: &Sender<TResult>, path: &str, msg: &str, skip: bool) {
    let _ = results.send(TResult::Warning(warning_message(path, msg, skip)));
}

fn is_link(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

impl Generator<'_> {
    /// `triggers_warning`.
    fn triggers_warning(&self, path: &str) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            warn(&self.results, path, "File does not exist.", true);
            return true;
        };
        let ft = meta.file_type();
        if ft.is_char_device() || ft.is_block_device() || ft.is_fifo() || ft.is_socket() {
            warn(
                &self.results,
                path,
                "File is character special device, block special device, FIFO, or socket.",
                true,
            );
            return true;
        }
        let readable = if meta.is_dir() {
            std::fs::read_dir(path).is_ok()
        } else {
            std::fs::File::open(path).is_ok()
        };
        if !readable {
            warn(&self.results, path, "File/Directory is not readable.", true);
            return true;
        }
        false
    }

    /// `should_ignore_file`.
    fn should_ignore_file(&self, path: &str) -> bool {
        if !self.follow_symlinks {
            let p = if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) && path.ends_with('/') {
                &path[..path.len() - 1]
            } else {
                path
            };
            if is_link(p) {
                return true;
            }
        }
        self.triggers_warning(path)
    }

    fn file_stats(&self, path: &str) -> Option<(String, i64, Instant)> {
        match std::fs::metadata(path) {
            Ok(m) => {
                let micros =
                    m.mtime() * 1_000_000 + (m.mtime_nsec() as f64 / 1000.0).round() as i64;
                Some((path.to_string(), m.len() as i64, Instant { micros }))
            }
            Err(_) => {
                self.triggers_warning(path);
                None
            }
        }
    }

    /// `list_files`, collected eagerly per directory level.
    fn list_files(&self, path: &str, dir_op: bool, out: &mut Vec<(String, i64, Instant)>) {
        if self.should_ignore_file(path) {
            return;
        }
        if !dir_op {
            if let Some(s) = self.file_stats(path) {
                out.push(s);
            }
            return;
        }
        let Ok(rd) = std::fs::read_dir(path) else {
            return;
        };
        let mut names: Vec<String> = vec![];
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let file_path = join(path, &name);
            if self.should_ignore_file(&file_path) {
                continue;
            }
            if std::fs::metadata(&file_path).is_ok_and(|m| m.is_dir()) {
                names.push(format!("{name}/"));
            } else {
                names.push(name);
            }
        }
        names.sort();
        for name in names {
            let file_path = join(path, &name);
            if std::fs::metadata(&file_path).is_ok_and(|m| m.is_dir()) {
                self.list_files(&file_path, dir_op, out);
            } else if let Some(s) = self.file_stats(&file_path) {
                out.push(s);
            }
        }
    }

    /// `FileGenerator.call`.
    fn call(&self, files: &Files) -> Box<dyn Iterator<Item = Item> + '_> {
        let files = files.clone();
        if files.src_type == "local" {
            let mut found = vec![];
            self.list_files(&files.src_path, files.dir_op, &mut found);
            let op = self.operation_name.clone();
            return Box::new(found.into_iter().map(move |(src, size, mtime)| {
                let (dest, compare_key) = dest_path_comp_key(&files, Some(&src));
                Ok(FileInfo {
                    src,
                    dest,
                    compare_key,
                    size: Some(size),
                    last_update: Some(mtime),
                    src_type: "local",
                    operation_name: op.clone(),
                    response_data: None,
                    etag: None,
                    case_conflict: None,
                })
            }));
        }
        let s3_path = files.src_path.clone();
        let (bucket, prefix) = match find_bucket_key(&s3_path) {
            Ok(v) => v,
            Err(e) => return Box::new(std::iter::once(Err(e.into()))),
        };
        if !files.dir_op && !prefix.is_empty() {
            let item =
                self.single_object(&s3_path, &bucket, &prefix)
                    .map(|(size, lu, etag, data)| {
                        let (dest, compare_key) = dest_path_comp_key(&files, Some(&s3_path));
                        FileInfo {
                            src: s3_path.clone(),
                            dest,
                            compare_key,
                            size,
                            last_update: lu,
                            src_type: "s3",
                            operation_name: self.operation_name.clone(),
                            response_data: data,
                            etag,
                            case_conflict: None,
                        }
                    });
            return Box::new(std::iter::once(item));
        }
        Box::new(S3Lister::new(self, files, bucket, prefix))
    }

    fn single_object(
        &self,
        s3_path: &str,
        bucket: &str,
        key: &str,
    ) -> Result<SingleObject, S3Fail> {
        if self.operation_name == "delete" {
            return Ok((None, None, None, None));
        }
        let mut params = map(&[("Bucket", Value::str(bucket)), ("Key", Value::str(key))]);
        for (k, v) in &self.head_params {
            params.insert(k.clone(), v.clone());
        }
        if self.response_checksum_when_supported && !params.contains_key("ChecksumMode") {
            params.insert("ChecksumMode".into(), Value::str("ENABLED"));
        }
        let _ = s3_path;
        match self.client.call("HeadObject", params) {
            Ok(mut resp) => {
                let size = resp.shift_remove("ContentLength").and_then(|v| v.as_i64());
                let lu = resp
                    .get("LastModified")
                    .and_then(Value::as_str)
                    .and_then(parse_timestamp);
                let etag = resp.get("ETag").and_then(Value::as_str).map(str::to_string);
                if let Some(s) = size {
                    resp.insert("Size".into(), Value::int(s));
                }
                Ok((size, lu, etag, Some(resp)))
            }
            Err(CallError::Service(mut info)) if info.code == "404" => {
                info.message = format!("Key \"{key}\" does not exist");
                Err(S3Fail::Call(CallError::Service(info)))
            }
            Err(e) => Err(S3Fail::Call(e)),
        }
    }
}

/// `BucketLister` over `list_objects_v2` pages, yielding as pages come.
struct S3Lister<'g, 'a> {
    g: &'g Generator<'a>,
    files: Files,
    bucket: String,
    params: Map,
    buffer: std::collections::VecDeque<Item>,
    done: bool,
    seen: Vec<String>,
}

impl<'g, 'a> S3Lister<'g, 'a> {
    fn new(g: &'g Generator<'a>, files: Files, bucket: String, prefix: String) -> Self {
        let mut params = map(&[
            ("Bucket", Value::str(&bucket)),
            ("Prefix", Value::str(prefix)),
        ]);
        if let Some(n) = g.page_size {
            params.insert("MaxKeys".into(), Value::int(n));
        }
        for (k, v) in &g.list_params {
            params.insert(k.clone(), v.clone());
        }
        S3Lister {
            g,
            files,
            bucket,
            params,
            buffer: Default::default(),
            done: false,
            seen: vec![],
        }
    }

    fn fetch(&mut self) {
        let page = match self.g.client.call("ListObjectsV2", self.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                self.buffer.push_back(Err(S3Fail::Call(e)));
                self.done = true;
                return;
            }
        };
        let s3_path = self.files.src_path.clone();
        for c in page
            .get("Contents")
            .and_then(Value::as_list)
            .cloned()
            .unwrap_or_default()
        {
            let Value::Map(mut content) = c else {
                continue;
            };
            let key = content
                .get("Key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let source_path = format!("{}/{key}", self.bucket);
            let size = content.get("Size").and_then(Value::as_i64).unwrap_or(0);
            let lu = content
                .get("LastModified")
                .and_then(Value::as_str)
                .and_then(parse_timestamp);
            if size == 0 && source_path.ends_with('/') {
                if self.g.operation_name != "delete" {
                    continue;
                }
            } else if !self.files.dir_op && s3_path != source_path {
                continue;
            }
            let etag = content
                .get("ETag")
                .and_then(Value::as_str)
                .map(str::to_string);
            content.insert("Size".into(), Value::int(size));
            let (dest, compare_key) = dest_path_comp_key(&self.files, Some(&source_path));
            self.buffer.push_back(Ok(FileInfo {
                src: source_path,
                dest,
                compare_key,
                size: Some(size),
                last_update: lu,
                src_type: "s3",
                operation_name: self.g.operation_name.clone(),
                response_data: Some(content),
                etag,
                case_conflict: None,
            }));
        }
        match page
            .get("NextContinuationToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            Some(t) => {
                if self.seen.iter().any(|s| s == t) {
                    self.buffer.push_back(Err(S3Fail::Other(anyhow::anyhow!(
                        "The same next token was received twice: {{'ContinuationToken': '{t}'}}"
                    ))));
                    self.done = true;
                    return;
                }
                self.seen.push(t.to_string());
                self.params
                    .insert("ContinuationToken".into(), Value::str(t));
            }
            None => self.done = true,
        }
    }
}

impl Iterator for S3Lister<'_, '_> {
    type Item = Item;
    fn next(&mut self) -> Option<Item> {
        loop {
            if let Some(i) = self.buffer.pop_front() {
                return Some(i);
            }
            if self.done {
                return None;
            }
            self.fetch();
        }
    }
}

// ---------------------------------------------------------------------
// Filters (`filters.py`).

struct Filter {
    patterns: Vec<(String, String)>,
    dst_patterns: Vec<(String, String)>,
}

fn create_filter(p: &Params) -> Result<Filter, S3Fail> {
    let raw: Vec<(String, String)> = match p.a.get("filters") {
        Some(Value::List(l)) if !l.is_empty() => l
            .iter()
            .filter_map(|f| {
                let f = f.as_list()?;
                Some((
                    f.first()?.as_str()?.trim_start_matches('-').to_string(),
                    f.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect(),
        _ => {
            return Ok(Filter {
                patterns: vec![],
                dst_patterns: vec![],
            });
        }
    };
    let dir_op = p.t("dir_op");
    let root = |loc: &str| -> Result<String, S3Fail> {
        if loc.starts_with("s3://") {
            let (bucket, mut key) = split_s3_bucket_key(loc)?;
            if !dir_op && !key.ends_with('/') {
                let parts: Vec<&str> = key.split('/').collect();
                key = parts[..parts.len() - 1].join("/");
            }
            Ok(format!("{bucket}/{key}"))
        } else if dir_op {
            Ok(abspath(loc))
        } else {
            Ok(abspath(&dirname(loc)))
        }
    };
    let src_root = root(p.src())?;
    let dst_root = root(p.dest())?;
    let full = |r: &str| {
        raw.iter()
            .map(|(t, pat)| (t.clone(), join(r, pat)))
            .collect::<Vec<_>>()
    };
    Ok(Filter {
        patterns: full(&src_root),
        dst_patterns: full(&dst_root),
    })
}

impl Filter {
    fn include(&self, f: &FileInfo) -> bool {
        let mut status = true;
        for (pat, dst) in self.patterns.iter().zip(&self.dst_patterns) {
            for (t, pattern) in [pat, dst] {
                if fnmatch(&f.src, pattern) {
                    status = t == "include";
                }
            }
        }
        status
    }

    fn call<'i>(
        &'i self,
        it: Box<dyn Iterator<Item = Item> + 'i>,
    ) -> Box<dyn Iterator<Item = Item> + 'i> {
        Box::new(it.filter(move |r| match r {
            Ok(f) => self.include(f),
            Err(_) => true,
        }))
    }
}

// ---------------------------------------------------------------------
// Sync strategies and the comparator.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strategy {
    SizeAndLastModified,
    SizeOnly,
    ExactTimestamps,
    NoOverwrite,
    Always,
    Never,
    Missing,
    CaseConflict,
    Delete,
}

struct Strategies {
    at_both: Strategy,
    not_at_dest: Strategy,
    not_at_src: Strategy,
    case_conflict: String,
    submitted: Arc<Mutex<HashSet<String>>>,
    results: Sender<TResult>,
}

fn total_seconds(a: Instant, b: Instant) -> f64 {
    (b.micros - a.micros) as f64 / 1_000_000.0
}

impl Strategies {
    fn compare_time(src: &FileInfo, dest: &FileInfo, exact: bool) -> bool {
        let (Some(s), Some(d)) = (src.last_update, dest.last_update) else {
            return false;
        };
        let delta = total_seconds(s, d);
        match src.operation_name.as_str() {
            "upload" | "copy" => delta >= 0.0,
            "download" if exact => delta == 0.0,
            "download" => delta <= 0.0,
            _ => false,
        }
    }

    fn should_sync(
        &self,
        s: Strategy,
        src: Option<&mut FileInfo>,
        dest: Option<&mut FileInfo>,
    ) -> Result<bool, S3Fail> {
        Ok(match s {
            Strategy::SizeAndLastModified | Strategy::ExactTimestamps => {
                let (src, dest) = (src.unwrap(), dest.unwrap());
                let same_size = src.size == dest.size;
                let same_time = Self::compare_time(src, dest, s == Strategy::ExactTimestamps);
                !same_size || !same_time
            }
            Strategy::SizeOnly => {
                let (src, dest) = (src.unwrap(), dest.unwrap());
                src.size != dest.size
            }
            Strategy::NoOverwrite | Strategy::Never => false,
            Strategy::Always | Strategy::Missing => true,
            Strategy::Delete => {
                if let Some(d) = dest {
                    d.operation_name = "delete".into();
                }
                true
            }
            Strategy::CaseConflict => {
                let src = src.unwrap();
                let lower = src.compare_key.to_lowercase();
                let mut should = true;
                let conflict = self.submitted.lock().unwrap().contains(&lower)
                    || std::fs::symlink_metadata(&src.dest).is_ok()
                        && std::fs::metadata(&src.dest).is_ok();
                if conflict {
                    match self.case_conflict.as_str() {
                        "skip" => {
                            super::err(format!("warning: Skipping {} -> {} because a file whose name differs only by case either exists or is being downloaded.\n", src.src, src.dest).as_bytes());
                            should = false;
                        }
                        "warn" => {
                            super::err(format!("warning: Downloading {} -> {} despite a file whose name differs only by case either existing or being downloaded. This behavior is not defined on case-insensitive filesystems and may result in overwriting existing files or race conditions between concurrent downloads. For more information, see https://docs.aws.amazon.com/cli/latest/topic/s3-case-insensitivity.html.\n", src.src, src.dest).as_bytes());
                        }
                        "error" => {
                            return Err(S3Fail::Other(anyhow::anyhow!(
                                "Failed to download {} -> {} because a file whose name differs only by case either exists or is being downloaded.",
                                src.src,
                                src.dest
                            )));
                        }
                        _ => {}
                    }
                }
                if should {
                    self.submitted.lock().unwrap().insert(lower.clone());
                    src.case_conflict = Some((self.submitted.clone(), lower));
                }
                let _ = &self.results;
                should
            }
        })
    }
}

/// `Comparator.call`.
struct Comparator<'i> {
    st: Strategies,
    src: Box<dyn Iterator<Item = Item> + 'i>,
    dest: Box<dyn Iterator<Item = Item> + 'i>,
    src_file: Option<FileInfo>,
    dest_file: Option<FileInfo>,
    src_done: bool,
    dest_done: bool,
    src_take: bool,
    dest_take: bool,
    failed: bool,
}

impl Iterator for Comparator<'_> {
    type Item = Item;
    fn next(&mut self) -> Option<Item> {
        if self.failed {
            return None;
        }
        loop {
            if !self.src_done && self.src_take {
                match self.src.next() {
                    Some(Ok(f)) => self.src_file = Some(f),
                    Some(Err(e)) => {
                        self.failed = true;
                        return Some(Err(e));
                    }
                    None => {
                        self.src_file = None;
                        self.src_done = true;
                    }
                }
            }
            if !self.dest_done && self.dest_take {
                match self.dest.next() {
                    Some(Ok(f)) => self.dest_file = Some(f),
                    Some(Err(e)) => {
                        self.failed = true;
                        return Some(Err(e));
                    }
                    None => {
                        self.dest_file = None;
                        self.dest_done = true;
                    }
                }
            }
            let r = if !self.src_done && !self.dest_done {
                self.src_take = true;
                self.dest_take = true;
                let (sk, dk) = (
                    self.src_file.as_ref().unwrap().compare_key.clone(),
                    self.dest_file.as_ref().unwrap().compare_key.clone(),
                );
                if sk == dk {
                    let mut s = self.src_file.clone().unwrap();
                    let mut d = self.dest_file.clone().unwrap();
                    match self
                        .st
                        .should_sync(self.st.at_both, Some(&mut s), Some(&mut d))
                    {
                        Ok(true) => Some(Ok(s)),
                        Ok(false) => None,
                        Err(e) => Some(Err(e)),
                    }
                } else if sk < dk {
                    self.dest_take = false;
                    let mut s = self.src_file.clone().unwrap();
                    match self.st.should_sync(self.st.not_at_dest, Some(&mut s), None) {
                        Ok(true) => Some(Ok(s)),
                        Ok(false) => None,
                        Err(e) => Some(Err(e)),
                    }
                } else {
                    self.src_take = false;
                    let mut d = self.dest_file.clone().unwrap();
                    match self.st.should_sync(self.st.not_at_src, None, Some(&mut d)) {
                        Ok(true) => Some(Ok(d)),
                        Ok(false) => None,
                        Err(e) => Some(Err(e)),
                    }
                }
            } else if !self.src_done && self.dest_done {
                self.src_take = true;
                let mut s = self.src_file.clone().unwrap();
                match self.st.should_sync(self.st.not_at_dest, Some(&mut s), None) {
                    Ok(true) => Some(Ok(s)),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                }
            } else if self.src_done && !self.dest_done {
                self.dest_take = true;
                let mut d = self.dest_file.clone().unwrap();
                match self.st.should_sync(self.st.not_at_src, None, Some(&mut d)) {
                    Ok(true) => Some(Ok(d)),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                }
            } else {
                return None;
            };
            if let Some(r) = r {
                if r.is_err() {
                    self.failed = true;
                }
                return Some(r);
            }
        }
    }
}

// ---------------------------------------------------------------------
// The command (`S3TransferCommand._run_main`, `CommandArchitecture`).

pub(crate) fn run(ctx: &Context, cmd: &str, args: Args) -> Result<ExitCode, S3Fail> {
    let page_size = args.int("page_size");
    let mut p = build_params(ctx, cmd, args)?;
    // `S3TransferCommand._get_source_and_transfer_clients`
    // (`ClientFactory.create_client`): a `--source-region` copy reads the
    // source with a client of that region and without `--endpoint-url`.
    let source_region = p
        .s("source_region")
        .filter(|_| p.paths_type() == "s3s3")
        .map(str::to_string);
    if let Some(r) = &source_region {
        crate::session::validate_region_name(r).map_err(|e| S3Fail::Other(e.into()))?;
    }
    let src_client = Client::with_config(
        ctx,
        crate::driver::ClientConfig {
            region: source_region.clone().map(Some),
            without_endpoint_url: source_region.is_some(),
            id: 1,
        },
    )
    .map_err(S3Fail::Other)?;
    let client = Client::with_config(
        ctx,
        crate::driver::ClientConfig {
            id: 2,
            ..Default::default()
        },
    )
    .map_err(S3Fail::Other)?;
    p.set("page_size", page_size.map_or(Value::Null, Value::int));
    let cfg = runtime_config(ctx, &p)?;
    let cmd = p.cmd.clone();
    let dir_op = p.t("dir_op");
    let files = format_files(p.src(), p.dest(), dir_op);
    let rev_files = format_files(p.dest(), p.src(), dir_op);
    let paths_type = p.paths_type().to_string();
    let operation_name = match paths_type.as_str() {
        "locals3" => "upload",
        "s3s3" => "copy",
        "s3local" => "download",
        _ => "delete",
    };
    let mode = if p.t("quiet") {
        Mode::Quiet
    } else if p.t("only_show_errors") || p.t("is_stream") {
        Mode::OnlyErrors
    } else if !p.t("progress") {
        Mode::NoProgress
    } else {
        Mode::Progress
    };
    let processor = Processor::start(
        mode,
        p.a.int("progress_frequency").unwrap_or(0),
        !p.t("progress_multiline"),
    );
    let results = processor.tx.clone();
    let when_supported = |k: &str| {
        ctx.session
            .config_variable(k)
            .is_none_or(|v| v == "when_supported")
    };
    let mut fgen_head = Map::new();
    let mut fgen_list = Map::new();
    mapper::request_payer_only(&mut fgen_list, &p.a);
    if paths_type == "s3s3" {
        mapper::head_object_with_copy_source_sse(&mut fgen_head, &p.a);
    } else {
        mapper::head_object(&mut fgen_head, &p.a);
    }
    let mut rgen_head = Map::new();
    mapper::request_payer_only(&mut rgen_head, &p.a);
    let fgen = Generator {
        client: &src_client,
        operation_name: operation_name.to_string(),
        follow_symlinks: p.t("follow_symlinks"),
        page_size,
        results: results.clone(),
        head_params: fgen_head,
        list_params: fgen_list.clone(),
        response_checksum_when_supported: when_supported("response_checksum_validation"),
    };
    let rgen = Generator {
        client: &client,
        operation_name: String::new(),
        follow_symlinks: p.t("follow_symlinks"),
        page_size,
        results: results.clone(),
        head_params: rgen_head,
        list_params: fgen_list,
        response_checksum_when_supported: when_supported("response_checksum_validation"),
    };
    let handle_case_conflicts = ["sync", "cp", "mv"].contains(&cmd.as_str())
        && paths_type == "s3local"
        && p.s("case_conflict") != Some("ignore")
        && dir_op;
    let submitted = Arc::new(Mutex::new(HashSet::new()));
    let make_strategies = |at_both: Strategy| Strategies {
        at_both,
        not_at_dest: if handle_case_conflicts {
            Strategy::CaseConflict
        } else {
            Strategy::Missing
        },
        not_at_src: Strategy::Never,
        case_conflict: p.s("case_conflict").unwrap_or("ignore").to_string(),
        submitted: submitted.clone(),
        results: results.clone(),
    };
    let filter1 = create_filter(&p)?;
    let filter2 = create_filter(&p)?;
    let is_stream = p.t("is_stream");
    let mut express_case_warning = false;
    let items: Box<dyn Iterator<Item = Item> + '_> = if is_stream {
        let (dest, compare_key) = dest_path_comp_key(&files, None);
        Box::new(std::iter::once(Ok(FileInfo {
            src: files.src_path.clone(),
            dest,
            compare_key,
            size: None,
            last_update: None,
            src_type: files.src_type,
            operation_name: operation_name.to_string(),
            response_data: None,
            etag: None,
            case_conflict: None,
        })))
    } else if cmd == "sync" {
        let mut st = make_strategies(Strategy::SizeAndLastModified);
        if p.t("size_only") {
            st.at_both = Strategy::SizeOnly;
        }
        if p.t("exact_timestamps") {
            st.at_both = Strategy::ExactTimestamps;
        }
        if p.t("delete") {
            st.not_at_src = Strategy::Delete;
        }
        if p.t("no_overwrite") {
            st.at_both = Strategy::NoOverwrite;
        }
        let src = filter1.call(fgen.call(&files));
        let dest = filter2.call(rgen.call(&rev_files));
        Box::new(Comparator {
            st,
            src,
            dest,
            src_file: None,
            dest_file: None,
            src_done: false,
            dest_done: false,
            src_take: true,
            dest_take: true,
            failed: false,
        })
    } else {
        let src = filter1.call(fgen.call(&files));
        if handle_case_conflicts && (cmd == "cp" || cmd == "mv") {
            let (src_bucket, _) = split_s3_bucket_key(p.src())?;
            if !is_s3express_bucket(&src_bucket) {
                let mut st = make_strategies(Strategy::Always);
                st.not_at_dest = Strategy::CaseConflict;
                let dest = filter2.call(rgen.call(&rev_files));
                Box::new(Comparator {
                    st,
                    src,
                    dest,
                    src_file: None,
                    dest_file: None,
                    src_done: false,
                    dest_done: false,
                    src_take: true,
                    dest_take: true,
                    failed: false,
                })
            } else {
                let cc = p.s("case_conflict").unwrap_or("ignore");
                if !["ignore", "warn"].contains(&cc) {
                    return Err(pv(format!(
                        "`{cc}` is not a valid value for `--case-conflict` when operating on S3 Express directory buckets. Valid values: `warn`, `ignore`."
                    )));
                }
                express_case_warning = true;
                src
            }
        } else {
            src
        }
    };
    if express_case_warning {
        super::err(b"warning: Recursive copies/moves from an S3 Express directory bucket to a case-insensitive local filesystem may result in undefined behavior if there are S3 object key names that differ only by case. To disable this warning, set the `--case-conflict` parameter to `ignore`. For more information, see https://docs.aws.amazon.com/cli/latest/topic/s3-case-insensitivity.html.")
    }
    let mut handler_params = p.a.clone();
    if cmd == "sync" {
        handler_params.0.shift_remove("no_overwrite");
    }
    let handler = Handler {
        client: &client,
        src_client: &src_client,
        cfg: cfg.clone(),
        p: handler_params,
        results: results.clone(),
        cancelled: AtomicBool::new(false),
        limiter: cfg.max_bandwidth.map(Limiter::new),
        request_checksum_when_supported: when_supported("request_checksum_calculation"),
        dest_root: abspath(p.dest()),
    };
    handler.run(items);
    drop(handler);
    drop(results);
    drop(fgen);
    drop(rgen);
    let (failed, warned) = processor.finish();
    Ok(if failed > 0 {
        ExitCode::S3TransferFailed
    } else if warned > 0 {
        ExitCode::S3Skipped
    } else {
        ExitCode::Success
    })
}

// ---------------------------------------------------------------------
// The transfer handler (`S3TransferHandler` and the request submitters).

struct Handler<'a> {
    client: &'a Client<'a>,
    /// `fileinfo.source_client`.
    src_client: &'a Client<'a>,
    cfg: TransferConfig,
    p: Args,
    results: Sender<TResult>,
    cancelled: AtomicBool,
    limiter: Option<Limiter>,
    request_checksum_when_supported: bool,
    dest_root: String,
}

/// A transfer's identity in the results.
#[derive(Clone)]
struct Tx {
    loc: Loc,
    if_none_match: bool,
    size: Option<i64>,
}

/// The annotations a multipart copy writes to the new object.
type Annotations = Arc<Mutex<Option<Vec<(String, Vec<u8>)>>>>;

/// What a job needs to finish a transfer.
type Done<'s> = Box<dyn FnOnce(Result<(), TransferError>) + Send + 's>;

enum TransferError {
    Call(CallError),
    Message(String),
    /// The transfer was cancelled by Ctrl-C before its request was sent.
    Interrupted,
}

/// Set by SIGINT during a transfer command (`KeyboardInterrupt`).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// SIGINT stops the submissions and cancels the transfers whose requests
/// are not in flight instead of ending the process.
fn catch_interrupt() {
    extern "C" fn on_sigint(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::Call(e) => write!(f, "{e}"),
            TransferError::Message(m) => f.write_str(m),
            TransferError::Interrupted => f.write_str("KeyboardInterrupt()"),
        }
    }
}

impl TransferError {
    fn precondition_failed(&self) -> bool {
        matches!(self, TransferError::Call(e) if e.code() == Some("PreconditionFailed"))
    }
}

fn copy_source_string(bucket: &str, key: &str) -> String {
    quote(&format!("{bucket}/{key}"), "-._~/")
}

/// `ChunksizeAdjuster.adjust_chunksize`.
fn adjust_chunksize(chunksize: i64, size: Option<i64>) -> i64 {
    let mut c = chunksize;
    if let Some(size) = size {
        let parts = |c: i64| (size as f64 / c as f64).ceil() as i64;
        while parts(c) > MAX_PARTS {
            c *= 2;
        }
    }
    c.clamp(MIN_UPLOAD_CHUNKSIZE, MAX_SINGLE_UPLOAD_SIZE)
}

/// `calculate_range_parameter`.
fn range_param(part_size: i64, index: i64, num_parts: i64, total: Option<i64>) -> String {
    let start = index * part_size;
    let end = if index == num_parts - 1 {
        total.map(|t| (t - 1).to_string()).unwrap_or_default()
    } else {
        (start + part_size - 1).to_string()
    };
    format!("bytes={start}-{end}")
}

fn filtered(extra: &Map, allow: &[&str]) -> Map {
    extra
        .iter()
        .filter(|(k, _)| allow.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn blocked(extra: &Map, block: &[&str]) -> Map {
    extra
        .iter()
        .filter(|(k, _)| !block.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn with(mut base: Map, extra: &Map) -> Map {
    for (k, v) in extra {
        base.insert(k.clone(), v.clone());
    }
    base
}

fn s3_fmt(path: &str) -> String {
    if path.starts_with("s3://") {
        path.to_string()
    } else {
        format!("s3://{path}")
    }
}

/// The input of an upload.
enum Source {
    File(String),
    Stdin,
}

impl<'a> Handler<'a> {
    fn send(&self, r: TResult) {
        let _ = self.results.send(r);
    }

    fn transfer_type(&self, f: &FileInfo) -> String {
        if self.p.truthy("is_move") {
            "move".into()
        } else {
            f.operation_name.clone()
        }
    }

    fn run(&self, items: Box<dyn Iterator<Item = Item> + '_>) {
        catch_interrupt();
        let pool = Pool::new(self.cfg.max_queue_size as usize);
        let workers = self.cfg.max_concurrent_requests.max(1) as usize;
        let total = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| pool.worker());
            }
            let mut items = items.peekable();
            while let Some(item) = items.next() {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    break;
                }
                // Look one item ahead so that an error raised while
                // generating the next file stops the command before the
                // current one is submitted.
                let item = match (item, items.peek()) {
                    (Ok(_), Some(Err(_))) => items.next().expect("peeked"),
                    (item, _) => item,
                };
                match item {
                    Ok(f) => match self.submit(&pool, f) {
                        Ok(true) => {
                            total.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(false) => {}
                        Err(e) => {
                            self.fatal(&pool, &e.to_string());
                            return;
                        }
                    },
                    Err(e) => {
                        let msg = match &e {
                            S3Fail::Fail(Fail::ParamValidation(m))
                            | S3Fail::Fail(Fail::General(m))
                            | S3Fail::Fail(Fail::Python(m))
                            | S3Fail::Fail(Fail::Unknown(m)) => m.clone(),
                            S3Fail::Call(c) => c.to_string(),
                            S3Fail::Other(o) => o.to_string(),
                        };
                        self.fatal(&pool, &msg);
                        return;
                    }
                }
            }
            if !INTERRUPTED.load(Ordering::SeqCst) {
                self.send(TResult::FinalTotal(total.load(Ordering::SeqCst)));
            }
            pool.close();
            pool.wait_idle();
            // `KeyboardInterrupt` out of the `TransferManager` block: the
            // transfers not in flight were cancelled, then
            // `CommandResultRecorder.__exit__` records the exception.
            if INTERRUPTED.load(Ordering::SeqCst) {
                self.send(TResult::Error(String::new()));
            }
        });
    }

    fn fatal(&self, pool: &Pool<'_>, msg: &str) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.send(TResult::Error(msg.to_string()));
        pool.close();
        pool.wait_idle();
    }

    /// The submitters in order: the first that can submit the file does.
    /// `Ok(true)` when a transfer was submitted.
    fn submit<'s>(&'s self, pool: &Pool<'s>, f: FileInfo) -> Result<bool, TransferError> {
        let is_stream = self.p.truthy("is_stream");
        match f.operation_name.as_str() {
            "upload" => self.submit_upload(pool, f, is_stream),
            "download" => self.submit_download(pool, f, is_stream),
            "copy" => self.submit_copy(pool, f),
            "delete" if f.src_type == "s3" => self.submit_delete(pool, f),
            "delete" => {
                self.local_delete(f);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn dryrun(&self, f: &FileInfo, src: String, dest: Option<String>) {
        self.send(TResult::DryRun(Loc {
            transfer_type: self.transfer_type(f),
            src,
            dest,
        }));
    }

    fn queued(&self, tx: &Tx) {
        self.send(TResult::Queued(tx.loc.clone(), tx.size));
    }

    fn progress(&self, tx: &Tx, bytes: i64) {
        self.send(TResult::Progress(tx.loc.clone(), bytes, tx.size, now()));
    }

    /// `DoneResultSubscriber`.
    fn done(&self, tx: &Tx, r: Result<(), TransferError>) {
        if self.cancelled.load(Ordering::SeqCst) {
            return;
        }
        match r {
            Ok(()) => self.send(TResult::Success(tx.loc.clone())),
            Err(TransferError::Interrupted) => self.send(TResult::CtrlC),
            Err(e) if tx.if_none_match && e.precondition_failed() => self.send(TResult::Skip),
            Err(e) => self.send(TResult::Failure(tx.loc.clone(), e.to_string())),
        }
    }

    fn call(&self, op: &str, params: Map) -> Result<Map, TransferError> {
        self.call_with(self.client, op, params)
    }

    /// A call of the source client.
    fn src_call(&self, op: &str, params: Map) -> Result<Map, TransferError> {
        self.call_with(self.src_client, op, params)
    }

    fn call_with(&self, client: &Client<'_>, op: &str, params: Map) -> Result<Map, TransferError> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(TransferError::Message("cancelled".into()));
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Err(TransferError::Interrupted);
        }
        client.call(op, params).map_err(TransferError::Call)
    }

    fn throttle(&self, n: usize) {
        if let Some(l) = &self.limiter {
            l.consume(n);
        }
    }

    // ---- upload -------------------------------------------------------

    fn submit_upload<'s>(
        &'s self,
        pool: &Pool<'s>,
        f: FileInfo,
        is_stream: bool,
    ) -> Result<bool, TransferError> {
        if !is_stream
            && let Some(size) = f.size
            && size > MAX_UPLOAD_SIZE
        {
            let path = relative_path(&f.src);
            self.send(TResult::Warning(warning_message(
                &path,
                &format!(
                    "File {path} exceeds s3 upload limit of {}.",
                    human_readable_size(MAX_UPLOAD_SIZE as f64)
                ),
                false,
            )));
        }
        let mut extra = Map::new();
        mapper::put_object(&mut extra, &self.p).map_err(|e| match e {
            Fail::ParamValidation(m) => TransferError::Message(m),
            other => TransferError::Message(format!("{other:?}")),
        })?;
        let src = if is_stream {
            "-".to_string()
        } else {
            relative_path(&f.src)
        };
        let dest = s3_fmt(&f.dest);
        if self.p.truthy("dryrun") {
            self.dryrun(&f, src, Some(dest));
            return Ok(false);
        }
        let (bucket, key) =
            find_bucket_key(&f.dest).map_err(|e| TransferError::Message(format!("{e:?}")))?;
        if self.request_checksum_when_supported
            && !FULL_OBJECT_CHECKSUM_ARGS
                .iter()
                .any(|c| extra.contains_key(*c))
            && !extra.contains_key("ChecksumAlgorithm")
        {
            extra.insert("ChecksumAlgorithm".into(), Value::str("CRC64NVME"));
        }
        let size = if is_stream {
            // `ProvideSizeSubscriber(int(expected_size))`.
            match self.p.str("expected_size") {
                None => None,
                Some(s) => Some(
                    crate::cli::pynum::py_int(s)
                        .and_then(|b| i64::try_from(b).ok())
                        .ok_or_else(|| {
                            TransferError::Message(format!(
                                "invalid literal for int() with base 10: {}",
                                crate::pyfmt::str_repr(s)
                            ))
                        })?,
                ),
            }
        } else {
            f.size
        };
        let tx = Tx {
            loc: Loc {
                transfer_type: self.transfer_type(&f),
                src,
                dest: Some(dest),
            },
            if_none_match: extra.contains_key("IfNoneMatch"),
            size,
        };
        // on_queued: the queued result, then the content type.
        self.queued(&tx);
        if !is_stream
            && self.p.truthy("guess_mime_type")
            && !self.p.truthy("content_type")
            && let Some(t) = guess_content_type(&f.src)
        {
            extra.insert("ContentType".into(), Value::str(t));
        }
        let is_move = !is_stream && self.p.truthy("is_move");
        let src_path = f.src.clone();
        let source = if is_stream {
            Source::Stdin
        } else {
            Source::File(f.src.clone())
        };
        let txc = tx.clone();
        let done: Done<'s> = Box::new(move |r| {
            let r = r.and_then(|()| {
                if is_move {
                    std::fs::remove_file(&src_path)
                        .map_err(|e| TransferError::Message(os_error(&e, &src_path)))?;
                }
                Ok(())
            });
            self.done(&txc, r);
        });
        let size = match (&source, tx.size) {
            (Source::File(path), _) => match std::fs::metadata(path) {
                Ok(m) => Some(m.len() as i64),
                Err(e) => {
                    done(Err(TransferError::Message(os_error(&e, path))));
                    return Ok(true);
                }
            },
            (Source::Stdin, s) => s,
        };
        let threshold = self.cfg.multipart_threshold;
        match source {
            Source::File(path) => {
                let size = size.unwrap_or(0);
                if size < threshold {
                    let put_extra = blocked(&extra, &["ChecksumType", "MpuObjectSize"]);
                    pool.submit(Box::new(move |_| {
                        let r = (|| {
                            let probe = with(
                                map(&[("Bucket", Value::str(&bucket)), ("Key", Value::str(&key))]),
                                &put_extra,
                            );
                            self.client
                                .validate("PutObject", &Value::Map(probe))
                                .map_err(TransferError::Call)?;
                            let body = std::fs::read(&path)
                                .map_err(|e| TransferError::Message(os_error(&e, &path)))?;
                            self.throttle(body.len());
                            let params = with(
                                map(&[
                                    ("Bucket", Value::str(&bucket)),
                                    ("Key", Value::str(&key)),
                                    ("Body", Value::Bytes(body)),
                                ]),
                                &put_extra,
                            );
                            self.call("PutObject", params)?;
                            self.progress(&tx, size);
                            Ok(())
                        })();
                        done(r);
                    }));
                } else {
                    let chunksize = adjust_chunksize(self.cfg.multipart_chunksize, Some(size));
                    let num_parts = (size as f64 / chunksize as f64).ceil() as i64;
                    let spec = upload_spec(tx, &bucket, &key, extra);
                    pool.submit(Box::new(move |pool: &Pool<'s>| {
                        let Some(mpu) = self.create_mpu(spec, done) else {
                            return;
                        };
                        for part in 1..=num_parts {
                            let path = path.clone();
                            mpu.submit_part(
                                pool,
                                false,
                                part,
                                Box::new(move |mpu: &Mpu<'s, 'a>| {
                                    let start = (part - 1) * chunksize;
                                    let len = chunksize.min(size - start).max(0) as usize;
                                    let file = std::fs::File::open(&path)
                                        .map_err(|e| TransferError::Message(os_error(&e, &path)))?;
                                    let mut buf = vec![0u8; len];
                                    file.read_exact_at(&mut buf, start as u64)
                                        .map_err(|e| TransferError::Message(os_error(&e, &path)))?;
                                    mpu.upload_part(part, buf)
                                }),
                            );
                        }
                        mpu.seal(pool);
                    }));
                }
            }
            Source::Stdin => {
                let mut stdin = std::io::stdin().lock();
                let mut initial = vec![];
                let multipart = match size {
                    Some(s) => s >= threshold,
                    None => {
                        let _ = (&mut stdin)
                            .take(threshold as u64)
                            .read_to_end(&mut initial);
                        initial.len() as i64 >= threshold
                    }
                };
                if !multipart {
                    let mut body = initial;
                    let _ = stdin.read_to_end(&mut body);
                    let put_extra = blocked(&extra, &["ChecksumType", "MpuObjectSize"]);
                    pool.submit(Box::new(move |_| {
                        let r = (|| {
                            let n = body.len() as i64;
                            self.throttle(body.len());
                            let params = with(
                                map(&[
                                    ("Bucket", Value::str(&bucket)),
                                    ("Key", Value::str(&key)),
                                    ("Body", Value::Bytes(body)),
                                ]),
                                &put_extra,
                            );
                            self.call("PutObject", params)?;
                            self.progress(&tx, n);
                            Ok(())
                        })();
                        done(r);
                    }));
                } else {
                    // `UploadNonSeekableInputManager`: parts are read in
                    // order from the stream, at most
                    // `max_in_memory_upload_chunks` at a time.
                    let chunksize = adjust_chunksize(self.cfg.multipart_chunksize, size);
                    let spec = upload_spec(tx, &bucket, &key, extra);
                    let Some(mpu) = self.create_mpu(spec, done) else {
                        return Ok(true);
                    };
                    let slots = Arc::new(Slots::new(MAX_IN_MEMORY_CHUNKS));
                    let mut pending = initial;
                    let mut part = 0;
                    loop {
                        while (pending.len() as i64) < chunksize {
                            let mut buf =
                                vec![0u8; (chunksize as usize - pending.len()).min(1 << 20)];
                            match stdin.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => pending.extend_from_slice(&buf[..n]),
                            }
                        }
                        if pending.is_empty() {
                            break;
                        }
                        let rest = if pending.len() as i64 > chunksize {
                            pending.split_off(chunksize as usize)
                        } else {
                            vec![]
                        };
                        let body = std::mem::replace(&mut pending, rest);
                        let last = (body.len() as i64) < chunksize;
                        part += 1;
                        slots.acquire();
                        let slots = slots.clone();
                        let n = part;
                        mpu.submit_part(
                            pool,
                            true,
                            n,
                            Box::new(move |mpu: &Mpu<'s, 'a>| {
                                let r = mpu.upload_part(n, body);
                                slots.release();
                                r
                            }),
                        );
                        if last {
                            break;
                        }
                    }
                    mpu.seal(pool);
                }
            }
        }
        Ok(true)
    }

    /// `CreateMultipartUploadTask`: the upload, or `None` after reporting
    /// the failure.
    fn create_mpu<'s>(&'s self, spec: UploadSpec, done: Done<'s>) -> Option<Arc<Mpu<'s, 'a>>> {
        let resp = match self.call(
            "CreateMultipartUpload",
            with(spec.base.clone(), &spec.create_extra),
        ) {
            Ok(r) => r,
            Err(e) => {
                done(Err(e));
                return None;
            }
        };
        let upload_id = resp
            .get("UploadId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Some(Arc::new(Mpu {
            h: self,
            spec,
            upload_id,
            inflight: AtomicUsize::new(0),
            sealed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            parts: Mutex::new(std::collections::BTreeMap::new()),
            failure: Mutex::new(None),
            done: Mutex::new(Some(done)),
        }))
    }

    // ---- download -----------------------------------------------------

    fn find_link_in_dest_path(&self, dest: &str) -> Option<String> {
        let root = self.dest_root.clone();
        if is_link(&root) {
            return Some(root);
        }
        let prefix = if root.ends_with('/') {
            root.clone()
        } else {
            format!("{root}/")
        };
        if !dest.starts_with(&prefix) {
            return None;
        }
        let mut parents = vec![];
        let mut path = dirname(dest);
        while path != root {
            parents.push(path.clone());
            let parent = dirname(&path);
            if parent == path {
                break;
            }
            path = parent;
        }
        for path in parents.iter().rev() {
            if basename(path) == ".." || is_link(path) {
                return Some(path.clone());
            }
        }
        if is_link(dest) {
            Some(dest.to_string())
        } else {
            None
        }
    }

    fn warn_glacier(&self, f: &FileInfo) -> bool {
        if self.p.truthy("force_glacier_transfer") {
            return false;
        }
        let Some(data) = &f.response_data else {
            return false;
        };
        let class = data.get("StorageClass").and_then(Value::as_str);
        let restored = data
            .get("Restore")
            .and_then(Value::as_str)
            .is_some_and(|r| r.contains("ongoing-request=\"false\""));
        let glacier = matches!(class, Some("GLACIER") | Some("DEEP_ARCHIVE")) && !restored;
        let incompatible = glacier
            && (matches!(f.operation_name.as_str(), "copy" | "download")
                || (f.operation_name == "move" && f.src_type == "s3"));
        if !incompatible {
            return false;
        }
        if !self.p.truthy("ignore_glacier_warnings") {
            let op = &f.operation_name;
            self.send(TResult::Warning(warning_message(
                &format!("s3://{}", f.src),
                &format!("Object is of storage class GLACIER. Unable to perform {op} operations on GLACIER objects. You must restore the object to be able to perform the operation. See aws s3 {op} help for additional parameter options to ignore or force these transfers."),
                true,
            )));
        }
        true
    }

    fn submit_download<'s>(
        &'s self,
        pool: &Pool<'s>,
        f: FileInfo,
        is_stream: bool,
    ) -> Result<bool, TransferError> {
        if self.warn_glacier(&f) {
            return Ok(false);
        }
        let normalized = normpath(&format!("./{}", f.compare_key));
        if normalized.starts_with("../") {
            self.send(TResult::Warning(warning_message(
                &f.compare_key,
                "File references a parent directory.",
                true,
            )));
            return Ok(false);
        }
        if !is_stream
            && !self.p.truthy("follow_symlinks")
            && self.find_link_in_dest_path(&f.dest).is_some()
        {
            return Ok(false);
        }
        if self.p.truthy("no_overwrite") && !is_stream && std::fs::metadata(&f.dest).is_ok() {
            return Ok(false);
        }
        let mut extra = Map::new();
        let _ = mapper::get_object(&mut extra, &self.p);
        let src = s3_fmt(&f.src);
        let dest = if is_stream {
            "-".to_string()
        } else {
            relative_path(&f.dest)
        };
        if self.p.truthy("dryrun") {
            self.dryrun(&f, src, Some(dest));
            return Ok(false);
        }
        let (bucket, key) =
            find_bucket_key(&f.src).map_err(|e| TransferError::Message(format!("{e:?}")))?;
        let tx = Tx {
            loc: Loc {
                transfer_type: self.transfer_type(&f),
                src,
                dest: Some(dest),
            },
            if_none_match: extra.contains_key("IfNoneMatch"),
            size: if is_stream { None } else { f.size },
        };
        self.queued(&tx);
        let dest_path = f.dest.clone();
        if !is_stream {
            let d = dirname(&dest_path);
            if !d.is_empty()
                && std::fs::metadata(&d).is_err()
                && let Err(e) = std::fs::create_dir_all(&d)
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                self.done(
                    &tx,
                    Err(TransferError::Message(format!(
                        "Could not create directory {d}: {}",
                        os_error(&e, &d)
                    ))),
                );
                return Ok(true);
            }
        }
        let is_move = self.p.truthy("is_move");
        let last_update = f.last_update;
        let request_payer = extra.get("RequestPayer").cloned();
        let case_conflict = f.case_conflict.clone();
        let txc = tx.clone();
        let (b2, k2) = (bucket.clone(), key.clone());
        let done: Done<'s> = Box::new(move |r| {
            let r = r.and_then(|()| {
                if !is_stream && let Some(lu) = last_update {
                    let secs = lu.secs();
                    let tv = libc::timeval {
                        tv_sec: secs as libc::time_t,
                        tv_usec: 0,
                    };
                    let c = std::ffi::CString::new(dest_path.clone()).unwrap_or_default();
                    let rc = unsafe { libc::utimes(c.as_ptr(), [tv, tv].as_ptr()) };
                    if rc != 0 {
                        self.send(TResult::Warning(warning_message(
                            &dest_path,
                            &format!("Successfully Downloaded {dest_path} but was unable to update the last modified time. {}", std::io::Error::last_os_error()),
                            true,
                        )));
                    }
                }
                if is_move {
                    let mut params = map(&[("Bucket", Value::str(&b2)), ("Key", Value::str(&k2))]);
                    if let Some(rp) = request_payer {
                        params.insert("RequestPayer".into(), rp);
                    }
                    self.src_call("DeleteObject", params)?;
                }
                Ok(())
            });
            if let Some((set, k)) = case_conflict {
                set.lock().unwrap().remove(&k);
            }
            self.done(&txc, r);
        });
        let (size, etag) = (f.size, f.etag.clone());
        let threshold = self.cfg.multipart_threshold;
        let chunksize = self.cfg.multipart_chunksize;
        let dest_file = f.dest.clone();
        pool.submit(Box::new(move |pool: &Pool<'s>| {
            let (size, etag) = if size.is_none() || etag.is_none() {
                match self.call(
                    "HeadObject",
                    with(map(&[("Bucket", Value::str(&bucket)), ("Key", Value::str(&key))]), &extra),
                ) {
                    Ok(r) => (
                        r.get("ContentLength").and_then(Value::as_i64),
                        r.get("ETag").and_then(Value::as_str).map(str::to_string),
                    ),
                    Err(e) => {
                        done(Err(e));
                        return;
                    }
                }
            } else {
                (size, etag)
            };
            let size = size.unwrap_or(0);
            let temp = if is_stream {
                String::new()
            } else {
                format!("{dest_file}.{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
            };
            if size < threshold {
                let r = (|| {
                    let resp = self.call(
                        "GetObject",
                        with(map(&[("Bucket", Value::str(&bucket)), ("Key", Value::str(&key))]), &extra),
                    )?;
                    let body = body_bytes(&resp);
                    self.throttle(body.len());
                    self.progress(&tx, body.len() as i64);
                    if is_stream {
                        super::out(&body);
                    } else {
                        std::fs::write(&temp, &body)
                            .map_err(|e| TransferError::Message(os_error(&e, &temp)))?;
                        std::fs::rename(&temp, &dest_file)
                            .map_err(|e| TransferError::Message(os_error(&e, &dest_file)))?;
                    }
                    Ok(())
                })();
                if r.is_err() && !temp.is_empty() {
                    let _ = std::fs::remove_file(&temp);
                }
                done(r);
                return;
            }
            let num_parts = (size as f64 / chunksize as f64).ceil() as i64;
            if !is_stream
                && let Err(e) = std::fs::File::create(&temp)
            {
                done(Err(TransferError::Message(os_error(&e, &temp))));
                return;
            }
            let state = Arc::new(Ranged {
                remaining: AtomicUsize::new(num_parts as usize),
                failure: Mutex::new(None),
                chunks: Mutex::new(vec![None; num_parts as usize]),
                done: Mutex::new(Some(done)),
            });
            for i in 0..num_parts {
                let mut params = map(&[
                    ("Bucket", Value::str(&bucket)),
                    ("Key", Value::str(&key)),
                    ("Range", Value::str(range_param(chunksize, i, num_parts, None))),
                ]);
                if let Some(e) = &etag {
                    params.insert("IfMatch".into(), Value::str(e));
                }
                let params = with(params, &extra);
                let (state, temp, dest_file, tx, key, bucket) = (
                    state.clone(),
                    temp.clone(),
                    dest_file.clone(),
                    tx.clone(),
                    key.clone(),
                    bucket.clone(),
                );
                pool.push(Box::new(move |_| {
                    let r = (|| -> Result<(), TransferError> {
                        if state.failure.lock().unwrap().is_some() {
                            return Err(TransferError::Message("cancelled".into()));
                        }
                        let resp = self.call("GetObject", params).map_err(|e| match e {
                            TransferError::Call(c) if c.code() == Some("PreconditionFailed") => {
                                TransferError::Message(format!(
                                    "Contents of stored object \"{key}\" in bucket \"{bucket}\" did not match expected ETag."
                                ))
                            }
                            other => other,
                        })?;
                        let body = body_bytes(&resp);
                        self.throttle(body.len());
                        self.progress(&tx, body.len() as i64);
                        if is_stream {
                            state.chunks.lock().unwrap()[i as usize] = Some(body);
                        } else {
                            let file = std::fs::OpenOptions::new()
                                .write(true)
                                .open(&temp)
                                .map_err(|e| TransferError::Message(os_error(&e, &temp)))?;
                            file.write_all_at(&body, (i * chunksize) as u64)
                                .map_err(|e| TransferError::Message(os_error(&e, &temp)))?;
                        }
                        Ok(())
                    })();
                    if let Err(e) = r {
                        let mut f = state.failure.lock().unwrap();
                        if f.is_none() {
                            *f = Some(e);
                        }
                    }
                    if state.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                        let failure = state.failure.lock().unwrap().take();
                        let done = state.done.lock().unwrap().take();
                        let result = match failure {
                            Some(e) => {
                                if !is_stream {
                                    let _ = std::fs::remove_file(&temp);
                                }
                                Err(e)
                            }
                            None if is_stream => {
                                crate::output::stdout::flush();
                                let mut o = std::io::stdout().lock();
                                for c in state.chunks.lock().unwrap().iter().flatten() {
                                    let _ = o.write_all(c);
                                }
                                let _ = o.flush();
                                Ok(())
                            }
                            None => std::fs::rename(&temp, &dest_file)
                                .map_err(|e| TransferError::Message(os_error(&e, &dest_file))),
                        };
                        if let Some(d) = done {
                            d(result);
                        }
                    }
                }));
            }
            let _ = pool;
        }));
        Ok(true)
    }

    // ---- copy ---------------------------------------------------------

    fn submit_copy<'s>(&'s self, pool: &Pool<'s>, f: FileInfo) -> Result<bool, TransferError> {
        if self.warn_glacier(&f) {
            return Ok(false);
        }
        let mut extra = Map::new();
        mapper::copy_object(&mut extra, &self.p).map_err(|e| match e {
            Fail::ParamValidation(m) => TransferError::Message(m),
            other => TransferError::Message(format!("{other:?}")),
        })?;
        let src = s3_fmt(&f.src);
        let dest = s3_fmt(&f.dest);
        if self.p.truthy("dryrun") {
            self.dryrun(&f, src, Some(dest));
            return Ok(false);
        }
        let (bucket, key) =
            find_bucket_key(&f.dest).map_err(|e| TransferError::Message(format!("{e:?}")))?;
        let (sb, sk) =
            find_bucket_key(&f.src).map_err(|e| TransferError::Message(format!("{e:?}")))?;
        let tx = Tx {
            loc: Loc {
                transfer_type: self.transfer_type(&f),
                src,
                dest: Some(dest),
            },
            if_none_match: extra.contains_key("IfNoneMatch"),
            size: f.size,
        };
        let is_move = self.p.truthy("is_move");
        let request_payer = extra.get("RequestPayer").cloned();
        let txc = tx.clone();
        let (sb2, sk2) = (sb.clone(), sk.clone());
        let tags_after: Arc<Mutex<Option<Vec<Value>>>> = Arc::new(Mutex::new(None));
        let tags_after2 = tags_after.clone();
        let annotations: Annotations = Arc::new(Mutex::new(None));
        let annotations2 = annotations.clone();
        let completed: Arc<Mutex<Option<Map>>> = Arc::default();
        let completed2 = completed.clone();
        let (b2, k2) = (bucket.clone(), key.clone());
        let done: Done<'s> = Box::new(move |r| {
            let r = r.and_then(|()| {
                if let Some(tag_set) = tags_after2.lock().unwrap().take() {
                    let mut params = map(&[
                        ("Bucket", Value::str(&b2)),
                        ("Key", Value::str(&k2)),
                        (
                            "Tagging",
                            Value::Map(map(&[("TagSet", Value::List(tag_set))])),
                        ),
                    ]);
                    if let Some(rp) = &request_payer {
                        params.insert("RequestPayer".into(), rp.clone());
                    }
                    if let Err(e) = self.call("PutObjectTagging", params) {
                        let mut del = map(&[("Bucket", Value::str(&b2)), ("Key", Value::str(&k2))]);
                        if let Some(rp) = &request_payer {
                            del.insert("RequestPayer".into(), rp.clone());
                        }
                        let _ = self.client.call("DeleteObject", del);
                        return Err(e);
                    }
                }
                // `SetAnnotationsSubscriber._on_success`.
                if let Some(list) = annotations2.lock().unwrap().take() {
                    let response = completed2.lock().unwrap().clone().unwrap_or_default();
                    let (mut succeeded, mut failed) = (Vec::new(), Vec::new());
                    for (name, payload) in list {
                        let mut params = map(&[
                            ("Bucket", Value::str(&b2)),
                            ("Key", Value::str(&k2)),
                            ("AnnotationName", Value::str(&name)),
                            ("AnnotationPayload", Value::Bytes(payload)),
                        ]);
                        if let Some(rp) = &request_payer {
                            params.insert("RequestPayer".into(), rp.clone());
                        }
                        if let Some(etag) = response.get("ETag") {
                            params.insert("ObjectIfMatch".into(), etag.clone());
                        }
                        if let Some(v) = response.get("VersionId") {
                            params.insert("VersionId".into(), v.clone());
                        }
                        match self.call("PutObjectAnnotation", params) {
                            Ok(_) => succeeded.push(name),
                            Err(e) => failed.push(format!("{name}: {e}")),
                        }
                    }
                    if !failed.is_empty() {
                        let written = if succeeded.is_empty() {
                            "(none)".to_string()
                        } else {
                            succeeded.join(", ")
                        };
                        return Err(TransferError::Message(format!(
                            "Failed to copy all annotations to s3://{b2}/{k2}. The object was copied successfully and was not deleted. Annotations written: {written}. Annotations that failed: {}.",
                            failed.join("; ")
                        )));
                    }
                }
                if is_move {
                    let mut params =
                        map(&[("Bucket", Value::str(&sb2)), ("Key", Value::str(&sk2))]);
                    if let Some(rp) = &request_payer {
                        params.insert("RequestPayer".into(), rp.clone());
                    }
                    self.src_call("DeleteObject", params)?;
                }
                Ok(())
            });
            self.done(&txc, r);
        });
        // on_queued
        self.queued(&tx);
        // `TransferManager.copy` validates the arguments before the
        // subscribers run; `CopySubmissionTask._submit` heads the source
        // when its size or ETag is unknown.
        let (mut size, mut etag) = (f.size, f.etag.clone());
        let head_response = if self.p.truthy("dir_op") {
            None
        } else {
            f.response_data.clone()
        };
        let threshold = self.cfg.multipart_threshold;
        let copy_source = copy_source_string(&sb, &sk);
        let r: Result<(), TransferError> = (|| {
            if !self.p.truthy("metadata_directive") {
                self.copy_props(
                    &mut extra,
                    &sb,
                    &sk,
                    size,
                    head_response,
                    &tags_after,
                    &annotations,
                )?;
            }
            if size.is_none() || etag.is_none() {
                let mut head = map(&[("Bucket", Value::str(&sb)), ("Key", Value::str(&sk))]);
                for (from, to) in [
                    ("CopySourceIfMatch", "IfMatch"),
                    ("CopySourceIfModifiedSince", "IfModifiedSince"),
                    ("CopySourceIfNoneMatch", "IfNoneMatch"),
                    ("CopySourceIfUnmodifiedSince", "IfUnmodifiedSince"),
                    ("CopySourceSSECustomerKey", "SSECustomerKey"),
                    ("CopySourceSSECustomerAlgorithm", "SSECustomerAlgorithm"),
                    ("CopySourceSSECustomerKeyMD5", "SSECustomerKeyMD5"),
                    ("RequestPayer", "RequestPayer"),
                    ("ExpectedBucketOwner", "ExpectedBucketOwner"),
                ] {
                    if let Some(v) = extra.get(from) {
                        head.insert(to.into(), v.clone());
                    }
                }
                let resp = self.src_call("HeadObject", head)?;
                size = resp.get("ContentLength").and_then(Value::as_i64);
                etag = resp.get("ETag").and_then(Value::as_str).map(str::to_string);
            }
            Ok(())
        })();
        if let Err(e) = r {
            done(Err(e));
            return Ok(true);
        }
        let size = size.unwrap_or(0);
        let base = map(&[("Bucket", Value::str(&bucket)), ("Key", Value::str(&key))]);
        if size < threshold {
            pool.submit(Box::new(move |_| {
                let params = with(
                    with(map(&[("CopySource", Value::str(&copy_source))]), &base),
                    &extra,
                );
                let r = self.call("CopyObject", params).map(|_| {
                    self.progress(&tx, size);
                });
                done(r);
            }));
            return Ok(true);
        }
        // Multipart copy.
        let create_extra = blocked(
            &extra,
            &[
                "CopySourceIfMatch",
                "CopySourceIfModifiedSince",
                "CopySourceIfNoneMatch",
                "CopySourceIfUnmodifiedSince",
                "CopySourceSSECustomerKey",
                "CopySourceSSECustomerAlgorithm",
                "CopySourceSSECustomerKeyMD5",
                "MetadataDirective",
                "TaggingDirective",
                "AnnotationDirective",
                "IfNoneMatch",
            ],
        );
        let part_size = adjust_chunksize(self.cfg.multipart_chunksize, Some(size));
        let num_parts = (size as f64 / part_size as f64).ceil() as i64;
        let part_args = filtered(
            &extra,
            &[
                "CopySourceIfMatch",
                "CopySourceIfModifiedSince",
                "CopySourceIfNoneMatch",
                "CopySourceIfUnmodifiedSince",
                "CopySourceSSECustomerKey",
                "CopySourceSSECustomerAlgorithm",
                "CopySourceSSECustomerKeyMD5",
                "SSECustomerKey",
                "SSECustomerAlgorithm",
                "SSECustomerKeyMD5",
                "RequestPayer",
                "ExpectedBucketOwner",
            ],
        );
        let spec = UploadSpec {
            tx,
            base,
            create_extra,
            part_extra: part_args,
            complete_extra: filtered(
                &extra,
                &[
                    "SSECustomerKey",
                    "SSECustomerAlgorithm",
                    "SSECustomerKeyMD5",
                    "RequestPayer",
                    "ExpectedBucketOwner",
                    "IfNoneMatch",
                ],
            ),
            checksum_member: extra
                .get("ChecksumAlgorithm")
                .and_then(Value::as_str)
                .map(|a| format!("Checksum{}", a.to_uppercase())),
            completed: completed.clone(),
        };
        pool.submit(Box::new(move |pool: &Pool<'s>| {
            let Some(mpu) = self.create_mpu(spec, done) else {
                return;
            };
            for part in 1..=num_parts {
                let mut args = map(&[
                    ("CopySource", Value::str(&copy_source)),
                    ("UploadId", Value::str(&mpu.upload_id)),
                    ("PartNumber", Value::int(part)),
                ]);
                args = with(args, &mpu.spec.part_extra);
                args.insert(
                    "CopySourceRange".into(),
                    Value::str(range_param(part_size, part - 1, num_parts, Some(size))),
                );
                if let Some(e) = &etag {
                    args.insert("CopySourceIfMatch".into(), Value::str(e));
                }
                let len = if part == num_parts {
                    size - (part - 1) * part_size
                } else {
                    part_size
                };
                let (sk, sb) = (sk.clone(), sb.clone());
                mpu.submit_part(
                    pool,
                    false,
                    part,
                    Box::new(move |mpu: &Mpu<'s, 'a>| {
                        let params = with(mpu.spec.base.clone(), &args);
                        let resp = mpu.h.call("UploadPartCopy", params).map_err(|e| match e {
                            TransferError::Call(c) if c.code() == Some("PreconditionFailed") => {
                                TransferError::Message(format!(
                                    "Contents of stored object \"{sk}\" in bucket \"{sb}\" did not match expected ETag."
                                ))
                            }
                            other => other,
                        })?;
                        mpu.h.progress(&mpu.spec.tx, len);
                        let result = resp
                            .get("CopyPartResult")
                            .and_then(Value::as_map)
                            .cloned()
                            .unwrap_or_default();
                        Ok(mpu.part_meta(part, &result))
                    }),
                );
            }
            mpu.seal(pool);
        }));
        Ok(true)
    }

    /// `CopyPropsSubscriberFactory` subscribers' `on_queued`.
    #[allow(clippy::too_many_arguments)]
    fn copy_props(
        &self,
        extra: &mut Map,
        sb: &str,
        sk: &str,
        size: Option<i64>,
        head_response: Option<Map>,
        tags_after: &Arc<Mutex<Option<Vec<Value>>>>,
        annotations: &Annotations,
    ) -> Result<(), TransferError> {
        let props = self.p.str("copy_props").unwrap_or("default").to_string();
        let multipart = size.is_some_and(|s| s >= self.cfg.multipart_threshold);
        const PROPS: &[&str] = &[
            "CacheControl",
            "ContentDisposition",
            "ContentEncoding",
            "ContentLanguage",
            "ContentType",
            "Expires",
            "Metadata",
        ];
        let metadata_props = |extra: &mut Map| -> Result<(), TransferError> {
            let mut inject = false;
            if multipart {
                inject = true;
            } else if extra.keys().any(|k| PROPS.contains(&k.as_str())) {
                inject = true;
                extra.insert("MetadataDirective".into(), Value::str("REPLACE"));
            }
            if inject {
                let head = match &head_response {
                    Some(h) => h.clone(),
                    None => {
                        let mut params =
                            map(&[("Bucket", Value::str(sb)), ("Key", Value::str(sk))]);
                        mapper::head_object_with_copy_source_sse(&mut params, &self.p);
                        self.src_call("HeadObject", params)?
                    }
                };
                for p in PROPS {
                    if !extra.contains_key(*p)
                        && let Some(v) = head.get(*p)
                    {
                        extra.insert((*p).to_string(), v.clone());
                    }
                }
            }
            Ok(())
        };
        let set_tags = |extra: &mut Map| -> Result<(), TransferError> {
            if !multipart {
                return Ok(());
            }
            let mut params = map(&[("Bucket", Value::str(sb)), ("Key", Value::str(sk))]);
            mapper::request_payer_only(&mut params, &self.p);
            let resp = self.src_call("GetObjectTagging", params)?;
            let tags = resp
                .get("TagSet")
                .and_then(Value::as_list)
                .cloned()
                .unwrap_or_default();
            if tags.is_empty() {
                return Ok(());
            }
            let pairs: Vec<(String, String)> = tags
                .iter()
                .map(|t| {
                    (
                        t.get("Key")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        t.get("Value")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                })
                .collect();
            let header = crate::session::url::percent_encode_sequence(&pairs);
            if header.len() <= 2 * 1024 {
                extra.insert("Tagging".into(), Value::str(header));
            } else {
                *tags_after.lock().unwrap() = Some(tags);
            }
            Ok(())
        };
        // `SetAnnotationsSubscriber.on_queued`: a multipart copy reads the
        // source's annotations to write them after the copy.
        let copy_annotations = || -> Result<(), TransferError> {
            if !multipart {
                return Ok(());
            }
            let version_id = head_response
                .as_ref()
                .and_then(|h| h.get("VersionId"))
                .cloned();
            let base = |extra: &[(&str, Value)]| {
                let mut params = map(&[("Bucket", Value::str(sb)), ("Key", Value::str(sk))]);
                for (k, v) in extra {
                    params.insert((*k).to_string(), v.clone());
                }
                mapper::request_payer_only(&mut params, &self.p);
                if let Some(v) = &version_id {
                    params.insert("VersionId".into(), v.clone());
                }
                params
            };
            let mut names = Vec::new();
            let mut token: Option<Value> = None;
            loop {
                let mut params = base(&[]);
                if let Some(t) = &token {
                    params.insert("ContinuationToken".into(), t.clone());
                }
                let page = self.src_call("ListObjectAnnotations", params)?;
                for a in page
                    .get("Annotations")
                    .and_then(Value::as_list)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    if let Some(n) = a.get("AnnotationName").and_then(Value::as_str) {
                        names.push(n.to_string());
                    }
                }
                match page.get("NextContinuationToken") {
                    Some(t) if t.is_truthy() && Some(t) != token.as_ref() => {
                        token = Some(t.clone())
                    }
                    _ => break,
                }
            }
            if names.is_empty() {
                return Ok(());
            }
            let mut out = Vec::new();
            for name in names {
                let resp = self.src_call(
                    "GetObjectAnnotation",
                    base(&[("AnnotationName", Value::str(&name))]),
                )?;
                let payload = match resp.get("AnnotationPayload") {
                    Some(Value::Bytes(b)) => b.clone(),
                    Some(Value::Str(s)) => s.clone().into_bytes(),
                    _ => Vec::new(),
                };
                out.push((name, payload));
            }
            *annotations.lock().unwrap() = Some(out);
            Ok(())
        };
        match props.as_str() {
            "none" => {
                extra.insert("MetadataDirective".into(), Value::str("REPLACE"));
                extra.insert("TaggingDirective".into(), Value::str("REPLACE"));
                extra.insert("AnnotationDirective".into(), Value::str("EXCLUDE"));
            }
            "metadata-directive" => {
                metadata_props(extra)?;
                extra.insert("TaggingDirective".into(), Value::str("REPLACE"));
                extra.insert("AnnotationDirective".into(), Value::str("EXCLUDE"));
            }
            "all" => {
                metadata_props(extra)?;
                set_tags(extra)?;
                copy_annotations()?;
            }
            _ => {
                metadata_props(extra)?;
                set_tags(extra)?;
                extra.insert("AnnotationDirective".into(), Value::str("EXCLUDE"));
            }
        }
        Ok(())
    }

    // ---- delete -------------------------------------------------------

    fn submit_delete<'s>(&'s self, pool: &Pool<'s>, f: FileInfo) -> Result<bool, TransferError> {
        let mut extra = Map::new();
        let _ = mapper::delete_object(&mut extra, &self.p);
        let src = s3_fmt(&f.src);
        if self.p.truthy("dryrun") {
            self.dryrun(&f, src, None);
            return Ok(false);
        }
        let (bucket, key) =
            find_bucket_key(&f.src).map_err(|e| TransferError::Message(format!("{e:?}")))?;
        let tx = Tx {
            loc: Loc {
                transfer_type: self.transfer_type(&f),
                src,
                dest: None,
            },
            if_none_match: extra.contains_key("IfNoneMatch"),
            size: f.size,
        };
        self.queued(&Tx {
            size: None,
            ..tx.clone()
        });
        pool.submit(Box::new(move |_| {
            let r = self
                .call(
                    "DeleteObject",
                    with(
                        map(&[("Bucket", Value::str(&bucket)), ("Key", Value::str(&key))]),
                        &extra,
                    ),
                )
                .map(|_| ());
            self.done(&tx, r);
        }));
        Ok(true)
    }

    fn local_delete(&self, f: FileInfo) {
        let src = relative_path(&f.src);
        if self.p.truthy("dryrun") {
            self.dryrun(&f, src, None);
            return;
        }
        let loc = Loc {
            transfer_type: "delete".into(),
            src,
            dest: None,
        };
        self.send(TResult::Queued(loc.clone(), Some(0)));
        match std::fs::remove_file(&f.src) {
            Ok(()) => self.send(TResult::Success(loc)),
            Err(e) => self.send(TResult::Failure(loc, os_error(&e, &f.src))),
        }
    }
}

/// `max_in_memory_upload_chunks` of the CLI's transfer manager.
const MAX_IN_MEMORY_CHUNKS: usize = 6;

/// A counting semaphore.
struct Slots {
    free: Mutex<usize>,
    cv: std::sync::Condvar,
}

impl Slots {
    fn new(n: usize) -> Slots {
        Slots {
            free: Mutex::new(n),
            cv: std::sync::Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut f = self.free.lock().unwrap();
        while *f == 0 {
            f = self.cv.wait(f).unwrap();
        }
        *f -= 1;
    }

    fn release(&self) {
        *self.free.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

/// The requests of one multipart upload (`UploadSubmissionTask` /
/// `CopySubmissionTask._submit_multipart_request`).
struct UploadSpec {
    tx: Tx,
    base: Map,
    create_extra: Map,
    part_extra: Map,
    complete_extra: Map,
    /// `Checksum{ALGORITHM}`, copied from part responses into the parts.
    checksum_member: Option<String>,
    /// The `CompleteMultipartUpload` response (`future.result()`).
    completed: Arc<Mutex<Option<Map>>>,
}

/// The upload arguments of `UploadSubmissionTask._submit_multipart_request`.
fn upload_spec(tx: Tx, bucket: &str, key: &str, mut extra: Map) -> UploadSpec {
    for c in FULL_OBJECT_CHECKSUM_ARGS {
        if extra.contains_key(*c) {
            extra.insert("ChecksumType".into(), Value::str("FULL_OBJECT"));
            extra.insert(
                "ChecksumAlgorithm".into(),
                Value::str(c.trim_start_matches("Checksum")),
            );
        }
    }
    let mut create_block: Vec<&str> = FULL_OBJECT_CHECKSUM_ARGS.to_vec();
    create_block.extend(["MpuObjectSize", "IfNoneMatch"]);
    let mut complete_allow = vec![
        "SSECustomerKey",
        "SSECustomerAlgorithm",
        "SSECustomerKeyMD5",
        "RequestPayer",
        "ExpectedBucketOwner",
        "ChecksumType",
        "MpuObjectSize",
        "IfNoneMatch",
    ];
    complete_allow.extend(FULL_OBJECT_CHECKSUM_ARGS);
    UploadSpec {
        tx,
        base: map(&[("Bucket", Value::str(bucket)), ("Key", Value::str(key))]),
        create_extra: blocked(&extra, &create_block),
        part_extra: filtered(
            &extra,
            &[
                "ChecksumAlgorithm",
                "SSECustomerKey",
                "SSECustomerAlgorithm",
                "SSECustomerKeyMD5",
                "RequestPayer",
                "ExpectedBucketOwner",
            ],
        ),
        complete_extra: filtered(&extra, &complete_allow),
        checksum_member: extra
            .get("ChecksumAlgorithm")
            .and_then(Value::as_str)
            .map(|a| format!("Checksum{}", a.to_uppercase())),
        completed: Arc::default(),
    }
}

type PartFn<'s, 'a> = Box<dyn FnOnce(&Mpu<'s, 'a>) -> Result<Map, TransferError> + Send + 's>;

/// A multipart upload in flight: parts are added (possibly while others
/// run), then the upload is sealed; the last part to finish after the
/// seal completes it, or aborts it when a part failed.
struct Mpu<'s, 'a> {
    h: &'s Handler<'a>,
    spec: UploadSpec,
    upload_id: String,
    inflight: AtomicUsize,
    sealed: AtomicBool,
    finished: AtomicBool,
    parts: Mutex<std::collections::BTreeMap<i64, Map>>,
    failure: Mutex<Option<TransferError>>,
    done: Mutex<Option<Done<'s>>>,
}

impl<'s, 'a> Mpu<'s, 'a> {
    fn part_meta(&self, part: i64, resp: &Map) -> Map {
        let mut meta = map(&[
            ("ETag", resp.get("ETag").cloned().unwrap_or(Value::Null)),
            ("PartNumber", Value::int(part)),
        ]);
        if let Some(m) = &self.spec.checksum_member
            && let Some(v) = resp.get(m)
        {
            meta.insert(m.clone(), v.clone());
        }
        meta
    }

    /// `UploadPartTask`.
    fn upload_part(&self, part: i64, body: Vec<u8>) -> Result<Map, TransferError> {
        let len = body.len() as i64;
        self.h.throttle(body.len());
        let params = with(
            with(
                self.spec.base.clone(),
                &map(&[
                    ("UploadId", Value::str(&self.upload_id)),
                    ("PartNumber", Value::int(part)),
                    ("Body", Value::Bytes(body)),
                ]),
            ),
            &self.spec.part_extra,
        );
        let resp = self.h.call("UploadPart", params)?;
        self.h.progress(&self.spec.tx, len);
        Ok(self.part_meta(part, &resp))
    }

    fn submit_part(
        self: &Arc<Self>,
        pool: &Pool<'s>,
        from_submitter: bool,
        part: i64,
        f: PartFn<'s, 'a>,
    ) {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        let me = self.clone();
        let job: super::engine::Job<'s> = Box::new(move |pool: &Pool<'s>| {
            let r = if me.failure.lock().unwrap().is_some() {
                Err(TransferError::Message("cancelled".into()))
            } else {
                f(&me)
            };
            match r {
                Ok(meta) => {
                    me.parts.lock().unwrap().insert(part, meta);
                }
                Err(e) => {
                    let mut failure = me.failure.lock().unwrap();
                    if failure.is_none() {
                        *failure = Some(e);
                    }
                }
            }
            if me.inflight.fetch_sub(1, Ordering::SeqCst) == 1 && me.sealed.load(Ordering::SeqCst) {
                me.finish(pool);
            }
        });
        if from_submitter {
            pool.submit(job);
        } else {
            pool.push(job);
        }
    }

    fn seal(self: &Arc<Self>, pool: &Pool<'s>) {
        self.sealed.store(true, Ordering::SeqCst);
        if self.inflight.load(Ordering::SeqCst) == 0 {
            self.finish(pool);
        }
    }

    /// `CompleteMultipartUploadTask`, or the failure cleanup
    /// (`abort_multipart_upload`).
    fn finish(self: &Arc<Self>, pool: &Pool<'s>) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        pool.push(Box::new(move |_| {
            let abort = || {
                let _ = me.h.client.call(
                    "AbortMultipartUpload",
                    with(
                        me.spec.base.clone(),
                        &map(&[("UploadId", Value::str(&me.upload_id))]),
                    ),
                );
            };
            let failure = me.failure.lock().unwrap().take();
            let r = match failure {
                Some(e) => {
                    abort();
                    Err(e)
                }
                None => {
                    let parts: Vec<Value> = me
                        .parts
                        .lock()
                        .unwrap()
                        .values()
                        .map(|p| Value::Map(p.clone()))
                        .collect();
                    let params = with(
                        with(
                            me.spec.base.clone(),
                            &map(&[
                                ("UploadId", Value::str(&me.upload_id)),
                                (
                                    "MultipartUpload",
                                    Value::Map(map(&[("Parts", Value::List(parts))])),
                                ),
                            ]),
                        ),
                        &me.spec.complete_extra,
                    );
                    match me.h.call("CompleteMultipartUpload", params) {
                        Ok(resp) => {
                            *me.spec.completed.lock().unwrap() = Some(resp);
                            Ok(())
                        }
                        Err(e) => {
                            abort();
                            Err(e)
                        }
                    }
                }
            };
            if let Some(d) = me.done.lock().unwrap().take() {
                d(r);
            }
        }));
    }
}

struct Ranged<'s> {
    remaining: AtomicUsize,
    failure: Mutex<Option<TransferError>>,
    chunks: Mutex<Vec<Option<Vec<u8>>>>,
    done: Mutex<Option<Done<'s>>>,
}

fn body_bytes(resp: &Map) -> Vec<u8> {
    match resp.get("Body") {
        Some(Value::Bytes(b)) => b.clone(),
        Some(Value::Str(s)) => s.clone().into_bytes(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_and_ranges() {
        assert_eq!(adjust_chunksize(8 * MB, Some(11 * MB)), 8 * MB);
        assert_eq!(adjust_chunksize(1024, Some(11 * MB)), 5 * MB);
        assert_eq!(range_param(5, 0, 2, None), "bytes=0-4");
        assert_eq!(range_param(5, 1, 2, None), "bytes=5-");
        assert_eq!(range_param(5, 1, 2, Some(9)), "bytes=5-8");
        assert_eq!(copy_source_string("b", "a b+c/d"), "b/a%20b%2Bc/d");
    }

    #[test]
    fn dest_keys() {
        let files = Files {
            src_path: "/x/dir/".into(),
            src_type: "local",
            dest_path: "b/pre/".into(),
            dir_op: true,
            use_src_name: true,
        };
        assert_eq!(
            dest_path_comp_key(&files, Some("/x/dir/sub/a.txt")),
            ("b/pre/sub/a.txt".to_string(), "sub/a.txt".to_string())
        );
    }
}
