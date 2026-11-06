//! The `aws s3` scenarios: local fixtures, S3 state, configuration, and
//! the command line.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::s3run;
use crate::s3stub::S3Stub;

/// The default modification time of local fixtures (2020-09-13).
pub const LOCAL_TIME: i64 = 1_600_000_000;
/// The default `LastModified` of objects in the initial state
/// (2023-11-14).
pub const OBJECT_TIME: i64 = 1_700_000_000;

#[derive(Clone)]
pub struct LocalFile {
    pub path: String,
    pub content: Vec<u8>,
    pub mtime: i64,
    pub symlink: Option<String>,
}

#[derive(Clone)]
pub struct S3Obj {
    pub key: String,
    pub content: Vec<u8>,
    pub mtime: i64,
    pub headers: Vec<(String, String)>,
    pub tags: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct BucketDef {
    pub name: String,
    pub objects: Vec<S3Obj>,
}

#[derive(Clone)]
pub struct Scenario {
    pub name: String,
    pub args: Vec<String>,
    pub local: Vec<LocalFile>,
    pub buckets: Vec<BucketDef>,
    /// Lines appended to the `[default]` profile.
    pub config: String,
    pub stdin: Option<Vec<u8>>,
    pub env: Vec<(String, String)>,
    pub tz: &'static str,
    pub no_endpoint: bool,
    pub compare_requests: bool,
}

/// Deterministic content of `n` bytes.
pub fn blob(n: usize, seed: u8) -> Vec<u8> {
    let mut x: u32 = 0x9e37_79b9 ^ u32::from(seed);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x & 0xff) as u8
        })
        .collect()
}

fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                has = true;
            }
            None if c == ' ' => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                has = false;
            }
            None => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn sc(name: &str, args: &str) -> Scenario {
    Scenario {
        name: name.to_string(),
        args: split_args(args),
        local: vec![],
        buckets: vec![],
        config: String::new(),
        stdin: None,
        env: vec![],
        tz: "UTC",
        no_endpoint: false,
        compare_requests: true,
    }
}

impl Scenario {
    fn file(mut self, path: &str, content: impl Into<Vec<u8>>) -> Self {
        self.local.push(LocalFile {
            path: path.to_string(),
            content: content.into(),
            mtime: LOCAL_TIME,
            symlink: None,
        });
        self
    }

    fn file_at(mut self, path: &str, content: impl Into<Vec<u8>>, mtime: i64) -> Self {
        self.local.push(LocalFile {
            path: path.to_string(),
            content: content.into(),
            mtime,
            symlink: None,
        });
        self
    }

    fn dir(mut self, path: &str) -> Self {
        self.local.push(LocalFile {
            path: format!("{}/", path.trim_end_matches('/')),
            content: vec![],
            mtime: LOCAL_TIME,
            symlink: None,
        });
        self
    }

    fn symlink(mut self, path: &str, target: &str) -> Self {
        self.local.push(LocalFile {
            path: path.to_string(),
            content: vec![],
            mtime: LOCAL_TIME,
            symlink: Some(target.to_string()),
        });
        self
    }

    fn bucket(mut self, name: &str) -> Self {
        self.buckets.push(BucketDef {
            name: name.to_string(),
            objects: vec![],
        });
        self
    }

    fn obj_full(
        mut self,
        bucket: &str,
        key: &str,
        content: impl Into<Vec<u8>>,
        mtime: i64,
        headers: &[(&str, &str)],
    ) -> Self {
        if !self.buckets.iter().any(|b| b.name == bucket) {
            self = self.bucket(bucket);
        }
        let b = self.buckets.iter_mut().find(|b| b.name == bucket).unwrap();
        b.objects.push(S3Obj {
            key: key.to_string(),
            content: content.into(),
            mtime,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            tags: vec![],
        });
        self
    }

    fn obj(self, bucket: &str, key: &str, content: impl Into<Vec<u8>>) -> Self {
        self.obj_full(bucket, key, content, OBJECT_TIME, &[])
    }

    fn obj_at(self, bucket: &str, key: &str, content: impl Into<Vec<u8>>, mtime: i64) -> Self {
        self.obj_full(bucket, key, content, mtime, &[])
    }

    fn tag(mut self, bucket: &str, key: &str, k: &str, v: &str) -> Self {
        let b = self.buckets.iter_mut().find(|b| b.name == bucket).unwrap();
        let o = b.objects.iter_mut().find(|o| o.key == key).unwrap();
        o.tags.push((k.to_string(), v.to_string()));
        self
    }

    fn config(mut self, text: &str) -> Self {
        self.config.push_str(text);
        self
    }

    fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.to_string(), v.to_string()));
        self
    }

    fn tz(mut self, tz: &'static str) -> Self {
        self.tz = tz;
        self
    }

    fn no_requests(mut self) -> Self {
        self.compare_requests = false;
        self
    }

    fn no_endpoint(mut self) -> Self {
        self.no_endpoint = true;
        self
    }
}

const MULTIPART: &str = "s3 =\n    multipart_threshold = 5MB\n    multipart_chunksize = 5MB\n";

fn tree(s: Scenario) -> Scenario {
    s.file("dir/a.txt", "alpha\n")
        .file("dir/b.json", "{\"b\": 1}\n")
        .file("dir/sub/c.html", "<p>c</p>\n")
        .file("dir/sub/deeper/d.bin", vec![0u8, 1, 2, 3])
        .file("dir/e.log", "log\n")
}

fn objects(s: Scenario) -> Scenario {
    s.obj("src", "a.txt", "alpha\n")
        .obj("src", "b.json", "{\"b\": 1}\n")
        .obj("src", "sub/c.html", "<p>c</p>\n")
        .obj("src", "sub/deeper/d.bin", vec![0u8, 1, 2, 3])
        .obj("src", "e.log", "log\n")
        .obj("src", "other/x.txt", "x\n")
}

pub fn all() -> Vec<Scenario> {
    let mut v = vec![
        // ls
        sc("ls-buckets", "s3 ls").bucket("alpha").bucket("beta"),
        sc("ls-buckets-empty", "s3 ls"),
        sc("ls-bucket", "s3 ls s3://src").with(objects),
        sc("ls-bucket-slash", "s3 ls s3://src/").with(objects),
        sc("ls-prefix", "s3 ls s3://src/sub/").with(objects),
        sc("ls-prefix-partial", "s3 ls s3://src/su").with(objects),
        sc("ls-missing-key", "s3 ls s3://src/nothing").with(objects),
        sc("ls-recursive", "s3 ls s3://src --recursive").with(objects),
        sc(
            "ls-recursive-summarize-hr",
            "s3 ls s3://src --recursive --summarize --human-readable",
        )
        .with(objects)
        .obj("src", "big", vec![7u8; 3 * 1024 * 1024 + 17]),
        sc("ls-summarize", "s3 ls s3://src --summarize").with(objects),
        sc("ls-page-size", "s3 ls s3://src --recursive --page-size 2").with(objects),
        sc("ls-no-bucket", "s3 ls s3://nope"),
        sc("ls-tz", "s3 ls s3://src --recursive")
            .with(objects)
            .tz("Asia/Tokyo"),
        sc("ls-bucket-prefix-filter", "s3 ls --bucket-name-prefix al")
            .bucket("alpha")
            .bucket("beta"),
        sc("ls-request-payer", "s3 ls s3://src --request-payer").with(objects),
        sc("ls-page-size-bad", "s3 ls --page-size x"),
        sc("ls-unknown", "s3 ls --bogus"),
        sc("ls-two-paths", "s3 ls s3://a s3://b"),
        // errors and argument handling
        sc("s3-no-subcommand", "s3"),
        sc("s3-bad-subcommand", "s3 foo"),
        sc("s3-unknown-option", "s3 --bogus"),
        sc("cp-missing-arg", "s3 cp a"),
        sc("cp-bad-types", "s3 cp a b"),
        sc("cp-local-missing", "s3 cp nonexist s3://b/"),
        sc("cp-bad-acl", "s3 cp a s3://b/ --acl x"),
        sc("cp-exclude-noarg", "s3 cp --exclude"),
        sc("cp-sse-c-no-key", "s3 cp s3://a/b s3://c/d --sse-c"),
        sc("cp-sse-c-key-no-sse-c", "s3 cp s3://a/b s3://c/d --sse-c-key k"),
        sc("cp-sse-c-copy-source-bad", "s3 cp x.txt s3://c/d --sse-c-copy-source --sse-c-copy-source-key k")
            .file("x.txt", "x"),
        sc("cp-stream-both", "s3 cp - -"),
        sc("sync-stream", "s3 sync - s3://b"),
        sc("cp-stream-recursive", "s3 cp - s3://b/k --recursive"),
        sc("cp-metadata-bad", "s3 cp a s3://b/ --metadata x"),
        sc("cp-checksum-mode-bad", "s3 cp x.txt s3://b/ --checksum-mode ENABLED").file("x.txt", "x"),
        sc("cp-checksum-algorithm-bad", "s3 cp s3://b/k x --checksum-algorithm CRC32"),
        sc("mv-same", "s3 mv s3://b/k s3://b/k"),
        sc("mv-same-dir", "s3 mv s3://b/k s3://b/"),
        sc("rm-local", "s3 rm foo"),
        sc("rm-sse", "s3 rm s3://b --sse"),
        sc("mb-missing", "s3 mb"),
        sc("mb-not-s3", "s3 mb foo"),
        sc("rb-key", "s3 rb s3://b/k"),
        sc("website-extra", "s3 website s3://b x"),
        sc("presign-bad-expires", "s3 presign s3://b/k --expires-in abc"),
        sc("cp-abbrev", "s3 cp x.txt s3://dst/ --quie").file("x.txt", "x").bucket("dst"),
        sc("cp-ambiguous", "s3 cp x.txt s3://dst/ --content").file("x.txt", "x"),
        sc("cp-grants-bad", "s3 cp x.txt s3://dst/ --grants foo")
            .file("x.txt", "x")
            .bucket("dst"),
        sc("cp-grants-bad-perm", "s3 cp x.txt s3://dst/ --grants foo=bar")
            .file("x.txt", "x")
            .bucket("dst"),
        // mb / rb / website
        sc("mb", "s3 mb s3://newbucket"),
        sc("mb-exists", "s3 mb s3://dst").bucket("dst"),
        sc("mb-tags", "s3 mb s3://newbucket --tags k1 v1 --tags k2 v2"),
        sc("mb-region", "s3 mb s3://newbucket --region eu-west-1"),
        sc("rb", "s3 rb s3://dst").bucket("dst"),
        sc("rb-not-empty", "s3 rb s3://src").with(objects),
        sc("rb-force", "s3 rb s3://src --force").with(objects),
        sc("rb-missing", "s3 rb s3://nope"),
        sc(
            "website",
            "s3 website s3://dst --index-document index.html --error-document err.html",
        )
        .bucket("dst"),
        sc("website-index-only", "s3 website dst/ --index-document index.html").bucket("dst"),
        // presign (signature and date masked)
        sc("presign", "s3 presign s3://src/a.txt").with(objects),
        sc("presign-expires", "s3 presign s3://src/sub/c.html --expires-in 60").with(objects),
        sc("presign-default-endpoint", "s3 presign s3://src/a b.txt").no_endpoint(),
        sc("presign-region", "s3 presign s3://src/k --region eu-west-1").no_endpoint(),
        // upload
        sc("cp-upload", "s3 cp x.txt s3://dst/").file("x.txt", "hello\n").bucket("dst"),
        sc("cp-upload-key", "s3 cp x.txt s3://dst/renamed.txt")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-bucket-noslash", "s3 cp x.txt s3://dst")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-progress", "s3 cp x.txt s3://dst/")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-no-progress", "s3 cp x.txt s3://dst/ --no-progress")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-quiet", "s3 cp x.txt s3://dst/ --quiet")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc(
            "cp-upload-params",
            "s3 cp x.txt s3://dst/ --no-progress --acl public-read --storage-class STANDARD_IA --cache-control max-age=60 --content-disposition inline --content-encoding gzip --content-language en --expires 2030-01-01T00:00:00Z --website-redirect /a --sse --metadata a=1,b=2 --grants read=uri=http://acs.amazonaws.com/groups/global/AllUsers full=id=abc",
        )
        .file("x.txt", "hello\n")
        .bucket("dst"),
        sc("cp-upload-content-type", "s3 cp x.txt s3://dst/ --no-progress --content-type text/x-custom")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-no-guess", "s3 cp x.html s3://dst/ --no-progress --no-guess-mime-type")
            .file("x.html", "<p/>")
            .bucket("dst"),
        sc("cp-upload-sse-kms", "s3 cp x.txt s3://dst/ --no-progress --sse aws:kms --sse-kms-key-id key-1")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-sse-c", "s3 cp x.txt s3://dst/ --no-progress --sse-c --sse-c-key 01234567890123456789012345678901")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-checksum", "s3 cp x.txt s3://dst/ --no-progress --checksum-algorithm SHA256")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-no-overwrite", "s3 cp x.txt s3://dst/ --no-progress --no-overwrite")
            .file("x.txt", "hello\n")
            .obj("dst", "x.txt", "old"),
        sc("cp-upload-tags", "s3 cp x.txt s3://dst/ --no-progress --tags env prod --tags team a&b=c/d")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-multipart-tags", "s3 cp big s3://dst/ --no-progress --tags k v")
            .file("big", blob(11 * 1024 * 1024 + 5, 3))
            .bucket("dst"),
        sc("mv-upload-tags", "s3 mv x.txt s3://dst/ --no-progress --tags k v")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("sync-upload-tags", "s3 sync dir s3://dst --no-progress --tags k v")
            .file("dir/a.txt", "a\n")
            .file("dir/b.txt", "b\n")
            .bucket("dst"),
        sc("cp-download-tags-bad", "s3 cp s3://src/a.txt out --tags k v").obj("src", "a.txt", "a"),
        sc("cp-copy-tags-bad", "s3 cp s3://src/a.txt s3://dst/ --tags k v").obj("src", "a.txt", "a").bucket("dst"),
        sc("cp-upload-no-bucket", "s3 cp x.txt s3://nope/ --no-progress").file("x.txt", "hello\n"),
        sc("cp-upload-dryrun", "s3 cp x.txt s3://dst/ --dryrun")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("cp-upload-empty", "s3 cp e s3://dst/ --no-progress").file("e", "").bucket("dst"),
        sc("cp-upload-mime-types", "s3 cp dir s3://dst/ --recursive --no-progress")
            .file("dir/a.css", "x")
            .file("dir/b.js", "x")
            .file("dir/c.png", "x")
            .file("dir/d.svg", "x")
            .file("dir/e.tar.gz", "x")
            .file("dir/f.unknownext", "x")
            .file("dir/g.JPG", "x")
            .file("dir/h.md", "x")
            .file("dir/i.wasm", "x")
            .file("dir/j.csv", "x")
            .file("dir/k.pdf", "x")
            .file("dir/l.zip", "x")
            .file("dir/m.mp4", "x")
            .file("dir/n", "x")
            .file("dir/o.webp", "x")
            .file("dir/p.xml", "x")
            .file("dir/q.yaml", "x")
            .file("dir/r.txt.bz2", "x")
            .bucket("dst"),
        sc("cp-upload-recursive", "s3 cp dir s3://dst/prefix --recursive")
            .with(tree)
            .bucket("dst"),
        sc("cp-upload-recursive-slash", "s3 cp dir/ s3://dst/ --recursive --no-progress")
            .with(tree)
            .bucket("dst"),
        sc(
            "cp-upload-exclude",
            "s3 cp dir s3://dst/ --recursive --no-progress --exclude *.log --exclude sub/*",
        )
        .with(tree)
        .bucket("dst"),
        sc(
            "cp-upload-include",
            "s3 cp dir s3://dst/ --recursive --no-progress --exclude * --include *.json --include sub/deeper/*",
        )
        .with(tree)
        .bucket("dst"),
        sc(
            "cp-upload-include-charclass",
            "s3 cp dir s3://dst/ --recursive --no-progress --exclude * --include *.[jh]*",
        )
        .with(tree)
        .bucket("dst"),
        sc("cp-upload-dir-without-recursive", "s3 cp dir s3://dst/ --no-progress")
            .with(tree)
            .bucket("dst"),
        sc("cp-upload-symlink", "s3 cp dir s3://dst/ --recursive --no-progress")
            .file("dir/a.txt", "a")
            .symlink("dir/link.txt", "a.txt")
            .bucket("dst"),
        sc("cp-upload-no-follow-symlinks", "s3 cp dir s3://dst/ --recursive --no-progress --no-follow-symlinks")
            .file("dir/a.txt", "a")
            .symlink("dir/link.txt", "a.txt")
            .bucket("dst"),
        sc("cp-upload-broken-symlink", "s3 cp dir s3://dst/ --recursive --no-progress")
            .file("dir/a.txt", "a")
            .symlink("dir/broken", "nowhere")
            .bucket("dst"),
        sc("cp-upload-multipart", "s3 cp big s3://dst/ --no-progress")
            .file("big", super::s3scenarios::blob(11 * 1024 * 1024 + 5, 1))
            .bucket("dst"),
        sc("cp-upload-multipart-config", "s3 cp big s3://dst/ --no-progress --metadata k=v --sse")
            .file("big", blob(12 * 1024 * 1024, 2))
            .config(MULTIPART)
            .bucket("dst"),
        sc("cp-upload-stream", "s3 cp - s3://dst/streamed")
            .stdin("streamed data\n")
            .bucket("dst"),
        sc("cp-upload-stream-multipart", "s3 cp - s3://dst/streamed --expected-size 12582912")
            .stdin(blob(12 * 1024 * 1024, 3))
            .config(MULTIPART)
            .bucket("dst"),
        sc("cp-upload-stream-big-nosize", "s3 cp - s3://dst/streamed")
            .stdin(blob(9 * 1024 * 1024, 4))
            .bucket("dst"),
        sc("mv-upload", "s3 mv x.txt s3://dst/ --no-progress")
            .file("x.txt", "hello\n")
            .bucket("dst"),
        sc("mv-upload-recursive", "s3 mv dir s3://dst/ --recursive --no-progress")
            .with(tree)
            .bucket("dst"),
        sc("cp-upload-unicode", "s3 cp dir s3://dst/ --recursive --no-progress")
            .file("dir/日本語.txt", "x")
            .file("dir/sp ace+plus.txt", "y")
            .bucket("dst"),
        sc("cp-max-concurrency", "s3 cp dir s3://dst/ --recursive --no-progress")
            .with(tree)
            .config("s3 =\n    max_concurrent_requests = 1\n")
            .bucket("dst"),
        sc("cp-config-bad", "s3 cp x.txt s3://dst/ --no-progress")
            .file("x.txt", "x")
            .config("s3 =\n    max_concurrent_requests = 0\n")
            .bucket("dst"),
        sc("cp-config-bandwidth", "s3 cp x.txt s3://dst/ --no-progress")
            .file("x.txt", "x")
            .config("s3 =\n    max_bandwidth = 10MB/s\n")
            .bucket("dst"),
        sc("cp-config-crt-only", "s3 cp x.txt s3://dst/ --no-progress")
            .file("x.txt", "x")
            .config("s3 =\n    target_bandwidth = 10MB/s\n")
            .bucket("dst"),
        // download
        sc("cp-download", "s3 cp s3://src/a.txt .").with(objects),
        sc("cp-download-rename", "s3 cp s3://src/a.txt out.txt --no-progress").with(objects),
        sc("cp-download-into-dir", "s3 cp s3://src/a.txt outdir/ --no-progress").with(objects),
        sc("cp-download-missing", "s3 cp s3://src/nope.txt . --no-progress").with(objects),
        sc("cp-download-recursive", "s3 cp s3://src/ out --recursive").with(objects),
        sc("cp-download-recursive-prefix", "s3 cp s3://src/sub out --recursive --no-progress")
            .with(objects),
        sc(
            "cp-download-recursive-exclude",
            "s3 cp s3://src out --recursive --no-progress --exclude * --include sub/*",
        )
        .with(objects),
        sc("cp-download-dir-marker", "s3 cp s3://src out --recursive --no-progress")
            .obj("src", "folder/", "")
            .obj("src", "folder/f.txt", "f"),
        sc("cp-download-stream", "s3 cp s3://src/a.txt -").with(objects),
        sc("cp-download-multipart", "s3 cp s3://src/big out.bin --no-progress")
            .obj("src", "big", blob(12 * 1024 * 1024, 5))
            .config(MULTIPART),
        sc("cp-download-sse-c", "s3 cp s3://src/a.txt . --no-progress --sse-c --sse-c-key 01234567890123456789012345678901")
            .with(objects),
        sc("cp-download-checksum-mode", "s3 cp s3://src/a.txt . --no-progress --checksum-mode ENABLED")
            .with(objects),
        sc("cp-download-glacier", "s3 cp s3://src out --recursive --no-progress")
            .obj_full("src", "cold", "c", OBJECT_TIME, &[("x-amz-storage-class", "GLACIER")])
            .obj("src", "warm", "w"),
        sc("cp-download-glacier-ignore", "s3 cp s3://src out --recursive --no-progress --ignore-glacier-warnings")
            .obj_full("src", "cold", "c", OBJECT_TIME, &[("x-amz-storage-class", "GLACIER")])
            .obj("src", "warm", "w"),
        sc("cp-download-glacier-force", "s3 cp s3://src out --recursive --no-progress --force-glacier-transfer")
            .obj_full("src", "cold", "c", OBJECT_TIME, &[("x-amz-storage-class", "DEEP_ARCHIVE")])
            .obj("src", "warm", "w"),
        sc("cp-download-parent-ref", "s3 cp s3://src out --recursive --no-progress")
            .obj("src", "../escape.txt", "e")
            .obj("src", "ok.txt", "o"),
        sc("cp-download-no-overwrite", "s3 cp s3://src out --recursive --no-progress --no-overwrite")
            .with(objects)
            .file("out/a.txt", "local"),
        sc("mv-download", "s3 mv s3://src/a.txt . --no-progress").with(objects),
        sc("mv-download-recursive", "s3 mv s3://src out --recursive --no-progress").with(objects),
        sc("cp-download-case-conflict", "s3 cp s3://src out --recursive --no-progress --case-conflict skip")
            .obj("src", "A.txt", "upper")
            .obj("src", "a.txt", "lower"),
        sc("cp-download-case-conflict-warn", "s3 cp s3://src out --recursive --no-progress --case-conflict warn")
            .obj("src", "A.txt", "upper")
            .obj("src", "a.txt", "lower"),
        sc("cp-download-case-conflict-error", "s3 cp s3://src out --recursive --no-progress --case-conflict error")
            .obj("src", "A.txt", "upper")
            .obj("src", "a.txt", "lower"),
        sc("cp-download-unicode", "s3 cp s3://src out --recursive --no-progress")
            .obj("src", "日本語/ファイル.txt", "j")
            .obj("src", "sp ace+plus%25.txt", "s"),
        // copy
        sc("cp-copy", "s3 cp s3://src/a.txt s3://dst/").with(objects).bucket("dst"),
        sc("cp-copy-rename", "s3 cp s3://src/a.txt s3://dst/b.txt --no-progress")
            .with(objects)
            .bucket("dst"),
        sc("cp-copy-recursive", "s3 cp s3://src s3://dst --recursive --no-progress")
            .with(objects)
            .bucket("dst"),
        sc(
            "cp-copy-metadata-directive",
            "s3 cp s3://src/a.txt s3://dst/ --no-progress --metadata-directive REPLACE --content-type text/x",
        )
        .with(objects)
        .bucket("dst"),
        sc("cp-copy-metadata", "s3 cp s3://src/a.txt s3://dst/ --no-progress --metadata k=v")
            .with(objects)
            .bucket("dst"),
        sc("cp-copy-props-none", "s3 cp s3://src/a.txt s3://dst/ --no-progress --copy-props none")
            .with(objects)
            .bucket("dst"),
        sc(
            "cp-copy-props-metadata-directive",
            "s3 cp s3://src/a.txt s3://dst/ --no-progress --copy-props metadata-directive --cache-control no-cache",
        )
        .obj_full(
            "src",
            "a.txt",
            "alpha",
            OBJECT_TIME,
            &[("content-type", "text/plain"), ("x-amz-meta-m", "1")],
        )
        .bucket("dst"),
        sc("cp-copy-multipart", "s3 cp s3://src/big s3://dst/ --no-progress")
            .obj_full(
                "src",
                "big",
                blob(11 * 1024 * 1024, 6),
                OBJECT_TIME,
                &[("content-type", "text/plain"), ("x-amz-meta-m", "1")],
            )
            .tag("src", "big", "t1", "v 1")
            .config(MULTIPART)
            .bucket("dst"),
        sc("cp-copy-multipart-recursive", "s3 cp s3://src s3://dst --recursive --no-progress")
            .obj("src", "big", blob(11 * 1024 * 1024, 7))
            .config(MULTIPART)
            .bucket("dst"),
        sc(
            "cp-copy-sse-c-copy-source",
            "s3 cp s3://src/a.txt s3://dst/ --no-progress --sse-c-copy-source --sse-c-copy-source-key 01234567890123456789012345678901",
        )
        .with(objects)
        .bucket("dst"),
        sc("cp-copy-missing", "s3 cp s3://src/nope s3://dst/ --no-progress")
            .with(objects)
            .bucket("dst"),
        sc("cp-copy-glacier", "s3 cp s3://src/cold s3://dst/ --no-progress")
            .obj_full("src", "cold", "c", OBJECT_TIME, &[("x-amz-storage-class", "GLACIER")])
            .bucket("dst"),
        sc("cp-copy-request-payer", "s3 cp s3://src/a.txt s3://dst/ --no-progress --request-payer requester")
            .with(objects)
            .bucket("dst"),
        sc("mv-copy", "s3 mv s3://src/a.txt s3://dst/ --no-progress")
            .with(objects)
            .bucket("dst"),
        sc("mv-copy-recursive", "s3 mv s3://src/sub s3://dst/moved --recursive --no-progress")
            .with(objects)
            .bucket("dst"),
        sc("cp-copy-dryrun", "s3 cp s3://src s3://dst --recursive --dryrun")
            .with(objects)
            .bucket("dst"),
        // rm
        sc("rm", "s3 rm s3://src/a.txt").with(objects),
        sc("rm-missing", "s3 rm s3://src/nope").with(objects),
        sc("rm-recursive", "s3 rm s3://src --recursive").with(objects),
        sc("rm-recursive-prefix", "s3 rm s3://src/sub --recursive").with(objects),
        sc("rm-recursive-exclude", "s3 rm s3://src --recursive --exclude *.txt").with(objects),
        sc("rm-dryrun", "s3 rm s3://src --recursive --dryrun").with(objects),
        sc("rm-quiet", "s3 rm s3://src --recursive --quiet").with(objects),
        sc("rm-only-show-errors", "s3 rm s3://src --recursive --only-show-errors").with(objects),
        sc("rm-dir-marker", "s3 rm s3://src --recursive")
            .obj("src", "folder/", "")
            .obj("src", "folder/f.txt", "f"),
        sc("rm-no-bucket", "s3 rm s3://nope --recursive"),
        sc("rm-page-size", "s3 rm s3://src --recursive --page-size 2").with(objects),
        // sync
        sc("sync-upload", "s3 sync dir s3://dst").with(tree).bucket("dst"),
        sc("sync-upload-no-progress", "s3 sync dir s3://dst/pre --no-progress")
            .with(tree)
            .bucket("dst"),
        sc("sync-upload-unchanged", "s3 sync dir s3://dst --no-progress")
            .file("dir/a.txt", "alpha\n")
            .file("dir/b.txt", "changed size\n")
            .file_at("dir/c.txt", "newer\n", OBJECT_TIME + 100)
            .obj("dst", "a.txt", "alpha\n")
            .obj("dst", "b.txt", "bbb\n")
            .obj("dst", "c.txt", "older\n"),
        sc("sync-upload-delete", "s3 sync dir s3://dst --no-progress --delete")
            .file("dir/a.txt", "alpha\n")
            .obj("dst", "a.txt", "alpha\n")
            .obj("dst", "stale.txt", "x")
            .obj("dst", "zz/stale2.txt", "x"),
        sc("sync-upload-delete-exclude", "s3 sync dir s3://dst --no-progress --delete --exclude stale*")
            .file("dir/a.txt", "alpha\n")
            .obj("dst", "stale.txt", "x")
            .obj("dst", "zz/stale2.txt", "x"),
        sc("sync-upload-size-only", "s3 sync dir s3://dst --no-progress --size-only")
            .file_at("dir/a.txt", "alpha\n", OBJECT_TIME + 100)
            .file("dir/b.txt", "bb")
            .obj("dst", "a.txt", "alpha\n")
            .obj("dst", "b.txt", "b"),
        sc("sync-upload-exact-timestamps", "s3 sync dir s3://dst --no-progress --exact-timestamps")
            .file_at("dir/a.txt", "alpha\n", OBJECT_TIME + 100)
            .obj("dst", "a.txt", "alpha\n"),
        sc("sync-upload-no-overwrite", "s3 sync dir s3://dst --no-progress --no-overwrite")
            .file("dir/a.txt", "changed\n")
            .file("dir/new.txt", "new\n")
            .obj("dst", "a.txt", "alpha\n"),
        sc("sync-upload-dryrun-delete", "s3 sync dir s3://dst --dryrun --delete")
            .with(tree)
            .obj("dst", "stale.txt", "x"),
        sc("sync-download", "s3 sync s3://src out").with(objects),
        sc("sync-download-existing", "s3 sync s3://src out --no-progress")
            .obj_at("src", "a.txt", "alpha\n", OBJECT_TIME)
            .obj_at("src", "b.txt", "bbbb\n", OBJECT_TIME)
            .obj_at("src", "c.txt", "c\n", OBJECT_TIME)
            .file_at("out/a.txt", "alpha\n", OBJECT_TIME)
            .file_at("out/b.txt", "b\n", OBJECT_TIME)
            .file_at("out/c.txt", "c\n", OBJECT_TIME - 50),
        sc("sync-download-exact-timestamps", "s3 sync s3://src out --no-progress --exact-timestamps")
            .obj_at("src", "a.txt", "alpha\n", OBJECT_TIME)
            .file_at("out/a.txt", "alpha\n", OBJECT_TIME + 50),
        sc("sync-download-delete", "s3 sync s3://src out --no-progress --delete")
            .obj("src", "a.txt", "alpha\n")
            .file("out/stale.txt", "stale")
            .file("out/sub/stale2.txt", "stale"),
        sc("sync-copy", "s3 sync s3://src s3://dst --no-progress")
            .with(objects)
            .obj("dst", "a.txt", "alpha\n"),
        sc("sync-copy-delete", "s3 sync s3://src/sub s3://dst --no-progress --delete")
            .with(objects)
            .obj("dst", "zzz", "z"),
        sc("sync-no-source", "s3 sync nodir s3://dst --no-progress").bucket("dst"),
        sc("sync-empty-dir", "s3 sync emptydir s3://dst --no-progress")
            .dir("emptydir")
            .bucket("dst"),
        sc("sync-case-conflict", "s3 sync s3://src out --no-progress --case-conflict skip")
            .obj("src", "A.txt", "upper")
            .obj("src", "a.txt", "lower"),
        sc("sync-upload-multipart", "s3 sync dir s3://dst --no-progress")
            .file("dir/big", blob(11 * 1024 * 1024, 8))
            .file("dir/small", "s")
            .config(MULTIPART)
            .bucket("dst"),
        sc("sync-page-size", "s3 sync s3://src out --no-progress --page-size 1").with(objects),
        sc("sync-only-show-errors", "s3 sync dir s3://nope --only-show-errors").with(tree),
        sc("sync-upload-error", "s3 sync dir s3://nope --no-progress").with(tree),
        sc("cp-progress-multiline", "s3 cp dir s3://dst --recursive --progress-multiline")
            .with(tree)
            .bucket("dst"),
        sc("cp-progress-frequency", "s3 cp dir s3://dst --recursive --progress-frequency 5")
            .with(tree)
            .bucket("dst"),
    ];
    v.extend([
        sc("s3-help", "s3 help"),
        sc("ls-help", "s3 ls help"),
        sc("cp-local-local", "s3 cp a b").file("a", "x"),
        sc("ls-dir-marker", "s3 ls s3://src --recursive")
            .obj("src", "folder/", "")
            .obj("src", "folder/f.txt", "f"),
        sc("ls-buckets-page-size", "s3 ls --page-size 1")
            .bucket("alpha")
            .bucket("beta")
            .bucket("gamma"),
        sc("ls-virtual", "s3 ls s3://src/sub/")
            .with(objects)
            .config("s3 =\n    addressing_style = virtual\n"),
        sc("cp-virtual", "s3 cp x.txt s3://dst/ --no-progress")
            .file("x.txt", "v")
            .bucket("dst")
            .config("s3 =\n    addressing_style = virtual\n"),
        sc(
            "cp-download-prefix-no-recursive",
            "s3 cp s3://src/sub/ out --no-progress",
        )
        .with(objects),
        sc(
            "cp-download-existing-dir",
            "s3 cp s3://src/a.txt outdir --no-progress",
        )
        .with(objects)
        .dir("outdir"),
        sc(
            "cp-download-new-parents",
            "s3 cp s3://src/a.txt new/deep/x.txt --no-progress",
        )
        .with(objects),
        sc("cp-download-stream-multipart", "s3 cp s3://src/big -")
            .obj("src", "big", blob(11 * 1024 * 1024, 9))
            .config(MULTIPART),
        sc(
            "sync-download-exclude",
            "s3 sync s3://src out --no-progress --exclude sub/*",
        )
        .with(objects),
        sc(
            "mv-upload-recursive-exclude",
            "s3 mv dir s3://dst/ --recursive --no-progress --exclude *.log",
        )
        .with(tree)
        .bucket("dst"),
        sc("mb-account-regional", "s3 mb s3://foo-an"),
        sc("mb-express", "s3 mb s3://foo--usw2-az1--x-s3"),
        sc(
            "website-trailing",
            "s3 website s3://dst/ --error-document e.html",
        )
        .bucket("dst"),
        sc(
            "cp-copy-content-type",
            "s3 cp s3://src/a.txt s3://dst/ --no-progress --content-type text/x",
        )
        .obj_full(
            "src",
            "a.txt",
            "alpha",
            OBJECT_TIME,
            &[
                ("content-type", "text/plain"),
                ("x-amz-meta-m", "1"),
                ("cache-control", "max-age=1"),
            ],
        )
        .bucket("dst"),
        sc(
            "cp-copy-metadata-directive-copy",
            "s3 cp s3://src/a.txt s3://dst/ --no-progress --metadata-directive COPY",
        )
        .with(objects)
        .bucket("dst"),
        sc(
            "cp-copy-props-all",
            "s3 cp s3://src/a.txt s3://dst/ --no-progress --copy-props all",
        )
        .with(objects)
        .bucket("dst"),
        sc("rm-key-recursive-file", "s3 rm s3://src/a.txt --recursive").with(objects),
        sc(
            "sync-s3-trailing",
            "s3 sync s3://src/sub/ out/ --no-progress",
        )
        .with(objects),
        sc(
            "cp-follow-symlink-dir",
            "s3 cp dir s3://dst/ --recursive --no-progress",
        )
        .file("real/x.txt", "x")
        .file("dir/a.txt", "a")
        .symlink("dir/linked", "../real")
        .bucket("dst"),
        sc(
            "cp-download-no-follow-symlinks",
            "s3 cp s3://src out --recursive --no-progress --no-follow-symlinks",
        )
        .obj("src", "linked/x.txt", "x")
        .obj("src", "a.txt", "a")
        .dir("real")
        .symlink("out/linked", "../real"),
        sc(
            "cp-upload-expected-size-small",
            "s3 cp - s3://dst/s --expected-size 5",
        )
        .stdin("hello")
        .bucket("dst"),
        sc(
            "cp-stream-no-overwrite",
            "s3 cp s3://src/a.txt - --no-overwrite",
        )
        .with(objects),
        sc(
            "cp-upload-dotdot",
            "s3 cp a/../x.txt s3://dst/ --no-progress",
        )
        .file("x.txt", "x")
        .dir("a")
        .bucket("dst"),
    ]);
    let mut many = sc("sync-many", "s3 sync dir s3://dst --no-progress").bucket("dst");
    let mut many_q = sc("cp-many-small-queue", "s3 cp dir s3://dst --recursive")
        .bucket("dst")
        .config("s3 =\n    max_queue_size = 1\n    max_concurrent_requests = 3\n");
    for i in 0..60 {
        many = many.file(&format!("dir/f{i:02}.txt"), format!("file {i}\n"));
        many_q = many_q.file(&format!("dir/f{i:02}.txt"), format!("file {i}\n"));
    }
    v.extend([
        many,
        many_q,
        sc(
            "cp-upload-multipart-no-bucket",
            "s3 cp big s3://nope/ --no-progress",
        )
        .file("big", blob(6 * 1024 * 1024, 10))
        .config(MULTIPART),
        sc(
            "cp-download-multipart-stream-missing",
            "s3 cp s3://src/nope - ",
        )
        .bucket("src"),
        sc("rb-force-no-bucket", "s3 rb s3://nope --force"),
        sc(
            "cp-upload-expected-size-bad",
            "s3 cp - s3://dst/s --expected-size 10MB",
        )
        .stdin("hello")
        .bucket("dst"),
        sc(
            "cp-copy-multipart-no-dest",
            "s3 cp s3://src/big s3://nope/ --no-progress",
        )
        .obj("src", "big", blob(6 * 1024 * 1024, 11))
        .config(MULTIPART),
    ]);
    v.push(
        sc("mv-validate-env", "s3 mv s3://b/k s3://b/k")
            .env("AWS_CLI_S3_MV_VALIDATE_SAME_S3_PATHS", "true")
            .no_requests(),
    );
    v
}

impl Scenario {
    fn with(self, f: fn(Scenario) -> Scenario) -> Scenario {
        f(self)
    }
}

pub struct Options {
    pub reference: PathBuf,
    pub candidate: PathBuf,
    pub filter: Option<regex::Regex>,
    pub show: usize,
    pub report_dir: PathBuf,
    pub jobs: usize,
    pub reference_only: bool,
}

pub async fn run(opts: Options) -> Result<bool> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/difftest/s3work");
    std::fs::create_dir_all(&base)?;
    if opts.report_dir.exists() {
        std::fs::remove_dir_all(&opts.report_dir)?;
    }
    std::fs::create_dir_all(&opts.report_dir)?;
    let scenarios: Vec<Scenario> = all()
        .into_iter()
        .filter(|s| opts.filter.as_ref().is_none_or(|f| f.is_match(&s.name)))
        .collect();
    let total = scenarios.len();
    let sem = Arc::new(tokio::sync::Semaphore::new(opts.jobs.max(1)));
    let opts = Arc::new(opts);
    let mut handles = vec![];
    for sc in scenarios {
        let permit = sem.clone().acquire_owned().await?;
        let (opts, base) = (opts.clone(), base.clone());
        handles.push(tokio::spawn(async move {
            let r = run_one(&sc, &opts, &base).await;
            drop(permit);
            (sc, r)
        }));
    }
    let mut failed = vec![];
    let mut shown = 0;
    for h in handles {
        let (sc, r) = h.await?;
        match r {
            Ok(None) => {}
            Ok(Some((cause, detail))) => {
                std::fs::write(opts.report_dir.join(format!("{}.txt", sc.name)), &detail)?;
                if shown < opts.show {
                    shown += 1;
                    eprintln!("\n{detail}");
                }
                eprintln!("FAIL {}: {cause}", sc.name);
                failed.push(sc.name.clone());
            }
            Err(e) => {
                eprintln!("FAIL {}: harness: {e:#}", sc.name);
                failed.push(sc.name.clone());
            }
        }
    }
    println!(
        "{total} s3 scenarios: {} passed, {} failed",
        total - failed.len(),
        failed.len()
    );
    println!("reports: {}", opts.report_dir.display());
    Ok(failed.is_empty())
}

async fn run_one(sc: &Scenario, opts: &Options, base: &Path) -> Result<Option<(String, String)>> {
    let stub = S3Stub::start("difftest.localhost").await?;
    let a = s3run::run_side(&opts.reference, sc, &stub, base).await?;
    if opts.reference_only {
        let detail = s3run::report(sc, &a, &[]);
        println!(
            "{detail}== requests\n{}\n== s3\n{}\n== local\n{}",
            a.requests.join("---\n"),
            a.s3,
            a.local
        );
        return Ok(None);
    }
    let b = s3run::run_side(&opts.candidate, sc, &stub, base).await?;
    let diffs = s3run::compare(&a, &b, sc.compare_requests);
    if diffs.is_empty() {
        return Ok(None);
    }
    let cause = format!(
        "differs: {}",
        diffs
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(Some((cause, s3run::report(sc, &a, &diffs))))
}
