//! `awscli/customizations/s3uploader.py` `S3Uploader` (single `PutObject`
//! uploads, as `s3transfer` sends them for files under the multipart
//! threshold).

use std::path::Path;

use md5::{Digest, Md5};

use super::{CfnError, client_error_text};
use crate::commands::services::basic;
use crate::commands::services::exec::Client;
use crate::driver::{CallResult, Context};
use crate::value::{Map, Value};

pub(super) struct Uploader<'c> {
    client: Client<'c>,
    pub bucket: String,
    prefix: Option<String>,
    kms_key_id: Option<String>,
    force_upload: bool,
    pub metadata: Option<Value>,
}

impl<'c> Uploader<'c> {
    pub(super) fn new(
        ctx: &'c Context,
        bucket: String,
        prefix: Option<String>,
        kms_key_id: Option<String>,
        force_upload: bool,
    ) -> Result<Uploader<'c>, CfnError> {
        Ok(Uploader {
            client: Client::new(ctx, "s3")?,
            bucket,
            prefix,
            kms_key_id: kms_key_id.filter(|k| !k.is_empty()),
            force_upload,
            metadata: None,
        })
    }

    /// `S3Uploader.upload`.
    pub(super) fn upload(&self, file: &Path, remote_path: &str) -> Result<String, CfnError> {
        let remote_path = match self.prefix.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("{p}/{remote_path}"),
            None => remote_path.to_string(),
        };
        if !self.force_upload && self.file_exists(&remote_path)? {
            return Ok(self.make_url(&remote_path));
        }
        let body = std::fs::read(file).map_err(|e| CfnError::General(e.to_string()))?;
        let mut params = Map::new();
        params.insert("Bucket".to_string(), Value::str(&self.bucket));
        params.insert("Key".to_string(), Value::str(&remote_path));
        params.insert("Body".to_string(), Value::Bytes(body.clone()));
        params.insert("ServerSideEncryption".to_string(), Value::str("AES256"));
        if let Some(k) = &self.kms_key_id {
            params.insert("ServerSideEncryption".to_string(), Value::str("aws:kms"));
            params.insert("SSEKMSKeyId".to_string(), Value::str(k));
        }
        if let Some(m) = self.metadata.as_ref().filter(|m| m.is_truthy()) {
            params.insert("Metadata".to_string(), m.clone());
        }
        params.insert("ChecksumAlgorithm".to_string(), Value::str("CRC64NVME"));
        crate::session::useragent::register_command(&[
            crate::session::useragent::Feature::S3Transfer,
        ]);
        let result = self.client.try_call("PutObject", &Value::Map(params))?;
        if !body.is_empty() {
            let size = body.len();
            basic::eprint(&format!(
                "\rUploading to {remote_path}  {size} / {}  (100.00%)",
                crate::pyfmt::float_repr(size as f64)
            ));
        }
        match result {
            CallResult::Ok(_) => Ok(self.make_url(&remote_path)),
            CallResult::ServiceError(info) => {
                if info.code == "NoSuchBucket" {
                    return Err(CfnError::General(format!(
                        "S3 Bucket does not exist. Execute the command to create a new bucket\naws s3 mb s3://{}",
                        self.bucket
                    )));
                }
                Err(CfnError::General(client_error_text(&info)))
            }
        }
    }

    /// `S3Uploader.upload_with_dedup`.
    pub(super) fn upload_with_dedup(
        &self,
        file: &Path,
        extension: Option<&str>,
    ) -> Result<String, CfnError> {
        let data = std::fs::read(file).map_err(|e| CfnError::General(e.to_string()))?;
        let mut remote = hex::encode(Md5::digest(&data));
        if let Some(ext) = extension {
            remote = format!("{remote}.{ext}");
        }
        self.upload(file, &remote)
    }

    /// `S3Uploader.file_exists`.
    fn file_exists(&self, remote_path: &str) -> Result<bool, CfnError> {
        let mut params = Map::new();
        params.insert("Bucket".to_string(), Value::str(&self.bucket));
        params.insert("Key".to_string(), Value::str(remote_path));
        Ok(matches!(
            self.client.try_call("HeadObject", &Value::Map(params))?,
            CallResult::Ok(_)
        ))
    }

    pub(super) fn make_url(&self, path: &str) -> String {
        format!("s3://{}/{path}", self.bucket)
    }

    /// `S3Uploader.to_path_style_s3_url` (`client.meta.endpoint_url`).
    pub(super) fn to_path_style_s3_url(&self, key: &str, version: Option<&str>) -> String {
        let ctx = self.client.ctx;
        let base = ctx
            .globals
            .endpoint_url
            .clone()
            .or_else(|| {
                ctx.session
                    .config()
                    .configured_endpoint_url("s3")
                    .ok()
                    .flatten()
            })
            .unwrap_or_else(|| match ctx.session.region().as_deref() {
                None | Some("us-east-1") | Some("aws-global") => {
                    "https://s3.amazonaws.com".to_string()
                }
                Some(r) if r.starts_with("cn-") => format!("https://s3.{r}.amazonaws.com.cn"),
                Some(r) => format!("https://s3.{r}.amazonaws.com"),
            });
        let mut result = format!("{base}/{}/{key}", self.bucket);
        if let Some(v) = version {
            result = format!("{result}?versionId={v}");
        }
        result
    }
}
