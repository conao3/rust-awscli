//! `cloudformation package` (`package.py`, `artifact_exporter.py`).

use std::path::{Path, PathBuf};

use serde_json::json;

use super::uploader::Uploader;
use super::yamlhelper::{timestamp_kind, yaml_dump, yaml_parse};
use super::zip::{is_zipfile, make_zip};
use super::{CfnError, basic};
use crate::commands::services::basic::ArgDef;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

fn args() -> Vec<ArgDef> {
    vec![
        ArgDef::new("template-file").required(),
        ArgDef::new("s3-bucket").required(),
        ArgDef::new("s3-prefix"),
        ArgDef::new("kms-key-id"),
        ArgDef::new("output-template-file"),
        ArgDef::new("use-json").flag(),
        ArgDef::new("force-upload").flag(),
        ArgDef::new("metadata").type_name("map").schema(json!({
            "type": "map",
            "key": {"type": "string"},
            "value": {"type": "string"},
        })),
    ]
}

pub(super) fn run(ctx: &Context, args_in: &[String]) -> Result<ExitCode, CliError> {
    let Some(a) = basic::parse(ctx, &["cloudformation", "package"], &args(), args_in)? else {
        return Ok(ExitCode::Success);
    };
    run_main(ctx, &a).map_err(|e| e.render(ctx))
}

fn run_main(ctx: &Context, a: &basic::Args) -> Result<ExitCode, CfnError> {
    let template_path = a.str("template-file").unwrap_or_default();
    let mut uploader = Uploader::new(
        ctx,
        a.str("s3-bucket").unwrap_or_default(),
        a.str("s3-prefix"),
        a.str("kms-key-id"),
        a.bool("force-upload"),
    )?;
    if !Path::new(&template_path).is_file() {
        return Err(CfnError::General(format!(
            "Invalid template path {template_path}"
        )));
    }
    uploader.metadata = a.get("metadata").cloned();
    let cwd = std::env::current_dir().map_err(|e| CfnError::General(e.to_string()))?;
    let exported = Template::new(&template_path, &cwd)?.export(&uploader)?;
    let text = if a.bool("use-json") {
        if let Some(kind) = timestamp_kind(&exported) {
            return Err(CfnError::General(format!(
                "Object of type {kind} is not JSON serializable"
            )));
        }
        crate::pyfmt::json_dumps_pretty(&exported)
    } else {
        yaml_dump(&exported)
    };
    basic::print("\n");
    match a.str("output-template-file") {
        None => basic::print(&text),
        Some(out) => {
            std::fs::write(&out, &text).map_err(|e| CfnError::General(os_error(&e, &out)))?;
            let abs = abspath(Path::new(&out));
            basic::print(&format!(
                "Successfully packaged artifacts and wrote output template to file {out}.\nExecute the following command to deploy the packaged template\naws cloudformation deploy --template-file {} --stack-name <YOUR STACK NAME>\n",
                abs.display()
            ));
        }
    }
    basic::flush();
    Ok(ExitCode::Success)
}

fn os_error(e: &std::io::Error, path: &str) -> String {
    let msg = match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        std::io::ErrorKind::IsADirectory => "Is a directory".to_string(),
        _ => e.to_string(),
    };
    format!(
        "[Errno {}] {msg}: {}",
        e.raw_os_error().unwrap_or(0),
        crate::pyfmt::str_repr(path)
    )
}

/// `os.path.normpath`.
pub(super) fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(comp);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = parts.join("/");
    let out = format!("{}{joined}", "/".repeat(initial));
    if out.is_empty() { ".".to_string() } else { out }
}

/// `os.path.join(a, b)`.
fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

fn abspath(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s.starts_with('/') {
        PathBuf::from(normpath(&s))
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        PathBuf::from(normpath(&join(&cwd.to_string_lossy(), &s)))
    }
}

/// `make_abs_path`.
fn make_abs_path(directory: &str, path: &Value) -> Value {
    match path {
        Value::Str(s) if !s.starts_with('/') => Value::Str(normpath(&join(directory, s))),
        other => other.clone(),
    }
}

fn is_local_folder(path: &Value) -> bool {
    matches!(path, Value::Str(s) if Path::new(s).is_dir())
}

fn is_local_file(path: &Value) -> bool {
    matches!(path, Value::Str(s) if Path::new(s).is_file())
}

fn is_zip_file(path: &Value) -> bool {
    matches!(path, Value::Str(s) if is_zipfile(Path::new(s)))
}

/// `parse_s3_url`: (bucket, key, version).
fn parse_s3_url(url: &Value) -> Option<(String, String, Option<String>)> {
    let Value::Str(url) = url else { return None };
    let rest = url.strip_prefix("s3://")?;
    let (rest, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };
    let rest = rest.split('#').next().unwrap_or(rest);
    let (netloc, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if netloc.is_empty() || path.is_empty() {
        return None;
    }
    let versions: Vec<&str> = query
        .split('&')
        .filter_map(|p| p.strip_prefix("versionId="))
        .filter(|v| !v.is_empty())
        .collect();
    let version = (versions.len() == 1).then(|| versions[0].to_string());
    Some((
        netloc.to_string(),
        path.trim_start_matches('/').to_string(),
        version,
    ))
}

fn is_s3_url(url: &Value) -> bool {
    parse_s3_url(url).is_some()
}

/// `jmespath.search` of a dotted property name.
fn search<'v>(value: &'v Value, expr: &str) -> Option<&'v Value> {
    let mut cur = value;
    for part in expr.split('.') {
        cur = cur.as_map()?.get(part)?;
    }
    Some(cur)
}

/// `set_value_from_jmespath`.
fn set_value(value: &mut Value, expr: &str, new: Value) {
    let (head, rest) = match expr.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (expr, None),
    };
    let Some(m) = value.as_map_mut() else { return };
    match rest {
        Some(rest) => {
            let child = m.entry(head.to_string()).or_insert_with(Value::map);
            set_value(child, rest, new);
        }
        None => {
            m.insert(head.to_string(), new);
        }
    }
}

fn path_of(v: &Value) -> PathBuf {
    PathBuf::from(match v {
        Value::Str(s) => s.clone(),
        other => crate::pyfmt::py_str(other),
    })
}

fn temp_name(prefix: &str) -> PathBuf {
    let dir = std::env::var("TMPDIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(dir).join(format!("{prefix}{}", uuid::Uuid::new_v4().simple()))
}

/// `zip_and_upload`.
fn zip_and_upload(local_path: &Path, uploader: &Uploader<'_>) -> Result<String, CfnError> {
    let zip = temp_name("data-").with_extension("zip");
    let zip = PathBuf::from(format!("{}", zip.display()));
    make_zip(&zip, &abspath(local_path)).map_err(|e| CfnError::General(e.to_string()))?;
    let result = uploader.upload_with_dedup(&zip, None);
    let _ = std::fs::remove_file(&zip);
    result
}

/// `upload_local_artifacts`.
fn upload_local_artifacts(
    resource_id: &str,
    resource: &Value,
    property: &str,
    parent_dir: &str,
    uploader: &Uploader<'_>,
) -> Result<Value, CfnError> {
    let local = search(resource, property)
        .cloned()
        .filter(|v| !matches!(v, Value::Null))
        .unwrap_or_else(|| Value::str(parent_dir));
    if is_s3_url(&local) {
        return Ok(local);
    }
    let local = make_abs_path(parent_dir, &local);
    if is_local_folder(&local) {
        return Ok(Value::Str(zip_and_upload(&path_of(&local), uploader)?));
    }
    if is_local_file(&local) {
        return Ok(Value::Str(
            uploader.upload_with_dedup(&path_of(&local), None)?,
        ));
    }
    Err(CfnError::General(format!(
        "Parameter {property} of resource {resource_id} refers to a file or folder that does not exist {}",
        crate::pyfmt::py_str(&local)
    )))
}

/// An exporter class of `RESOURCES_EXPORT_LIST` / `METADATA_EXPORT_LIST`.
struct Exporter {
    resource_type: &'static str,
    property: &'static str,
    package_null: bool,
    force_zip: bool,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    /// `Resource`.
    Url,
    /// `ResourceWithS3UrlDict` (bucket, key, version property names).
    Dict(&'static str, &'static str, Option<&'static str>),
    /// `CloudFormationStackResource`.
    Stack,
}

const fn res(t: &'static str, p: &'static str, null: bool, zip: bool, kind: Kind) -> Exporter {
    Exporter {
        resource_type: t,
        property: p,
        package_null: null,
        force_zip: zip,
        kind,
    }
}

const RESOURCES: &[Exporter] = &[
    res(
        "AWS::Serverless::Function",
        "CodeUri",
        true,
        true,
        Kind::Url,
    ),
    res(
        "AWS::Serverless::Api",
        "DefinitionUri",
        false,
        false,
        Kind::Url,
    ),
    res(
        "AWS::AppSync::GraphQLSchema",
        "DefinitionS3Location",
        false,
        false,
        Kind::Url,
    ),
    res(
        "AWS::AppSync::Resolver",
        "RequestMappingTemplateS3Location",
        false,
        false,
        Kind::Url,
    ),
    res(
        "AWS::AppSync::Resolver",
        "ResponseMappingTemplateS3Location",
        false,
        false,
        Kind::Url,
    ),
    res(
        "AWS::AppSync::FunctionConfiguration",
        "RequestMappingTemplateS3Location",
        false,
        false,
        Kind::Url,
    ),
    res(
        "AWS::AppSync::FunctionConfiguration",
        "ResponseMappingTemplateS3Location",
        false,
        false,
        Kind::Url,
    ),
    res(
        "AWS::ApiGateway::RestApi",
        "BodyS3Location",
        false,
        false,
        Kind::Dict("Bucket", "Key", Some("Version")),
    ),
    res(
        "AWS::Lambda::Function",
        "Code",
        true,
        true,
        Kind::Dict("S3Bucket", "S3Key", Some("S3ObjectVersion")),
    ),
    res(
        "AWS::ElasticBeanstalk::ApplicationVersion",
        "SourceBundle",
        true,
        false,
        Kind::Dict("S3Bucket", "S3Key", None),
    ),
    res(
        "AWS::CloudFormation::Stack",
        "TemplateURL",
        true,
        false,
        Kind::Stack,
    ),
    res(
        "AWS::Serverless::Application",
        "Location",
        true,
        false,
        Kind::Stack,
    ),
    res(
        "AWS::Serverless::LayerVersion",
        "ContentUri",
        true,
        true,
        Kind::Url,
    ),
    res(
        "AWS::Lambda::LayerVersion",
        "Content",
        true,
        true,
        Kind::Dict("S3Bucket", "S3Key", Some("S3ObjectVersion")),
    ),
    res(
        "AWS::Glue::Job",
        "Command.ScriptLocation",
        true,
        false,
        Kind::Url,
    ),
    res(
        "AWS::StepFunctions::StateMachine",
        "DefinitionS3Location",
        false,
        false,
        Kind::Dict("Bucket", "Key", Some("Version")),
    ),
    res(
        "AWS::Serverless::StateMachine",
        "DefinitionUri",
        false,
        false,
        Kind::Dict("Bucket", "Key", Some("Version")),
    ),
    res(
        "AWS::CodeCommit::Repository",
        "Code.S3",
        false,
        true,
        Kind::Dict("Bucket", "Key", Some("ObjectVersion")),
    ),
];

const METADATA: &[Exporter] = &[
    res(
        "AWS::ServerlessRepo::Application",
        "ReadmeUrl",
        false,
        false,
        Kind::Url,
    ),
    res(
        "AWS::ServerlessRepo::Application",
        "LicenseUrl",
        false,
        false,
        Kind::Url,
    ),
];

impl Exporter {
    /// `Resource.export`.
    fn export(
        &self,
        resource_id: &str,
        resource: &mut Value,
        parent_dir: &str,
        uploader: &Uploader<'_>,
    ) -> Result<(), CfnError> {
        if matches!(resource, Value::Null) {
            return Ok(());
        }
        let property_value = search(resource, self.property).cloned();
        let truthy = property_value.as_ref().is_some_and(Value::is_truthy);
        if !truthy && !self.package_null {
            return Ok(());
        }
        if let Some(Value::Map(_)) = property_value {
            return Ok(());
        }
        let pv = property_value.clone().unwrap_or(Value::Null);
        let mut temp_dir = None;
        if self.force_zip && is_local_file(&pv) && !is_zip_file(&pv) {
            let dir = temp_name("tmp");
            let src = path_of(&pv);
            let copied = std::fs::create_dir(&dir).and_then(|_| {
                let dst = dir.join(src.file_name().unwrap_or_default());
                std::fs::copy(&src, &dst).map(|_| ())
            });
            copied.map_err(|e| CfnError::General(e.to_string()))?;
            set_value(
                resource,
                self.property,
                Value::Str(dir.to_string_lossy().into_owned()),
            );
            temp_dir = Some(dir);
        }
        let result = self.do_export(resource_id, resource, parent_dir, uploader);
        if let Some(d) = temp_dir {
            let _ = std::fs::remove_dir_all(d);
        }
        result.map_err(|ex| {
            CfnError::General(format!(
                "Unable to upload artifact {} referenced by {} parameter of {resource_id} resource.\n{}",
                crate::pyfmt::py_str(&pv),
                self.property,
                ex.text()
            ))
        })
    }

    fn do_export(
        &self,
        resource_id: &str,
        resource: &mut Value,
        parent_dir: &str,
        uploader: &Uploader<'_>,
    ) -> Result<(), CfnError> {
        match self.kind {
            Kind::Url => {
                let url = upload_local_artifacts(
                    resource_id,
                    resource,
                    self.property,
                    parent_dir,
                    uploader,
                )?;
                set_value(resource, self.property, url);
            }
            Kind::Dict(bucket, key, version) => {
                let url = upload_local_artifacts(
                    resource_id,
                    resource,
                    self.property,
                    parent_dir,
                    uploader,
                )?;
                let Some((b, k, v)) = parse_s3_url(&url) else {
                    return Err(CfnError::General(format!(
                        "URL given to the parse method is not a valid S3 url {}",
                        crate::pyfmt::py_str(&url)
                    )));
                };
                let mut m = Map::new();
                m.insert(bucket.to_string(), Value::Str(b));
                m.insert(key.to_string(), Value::Str(k));
                if let (Some(vp), Some(v)) = (version, v) {
                    m.insert(vp.to_string(), Value::Str(v));
                }
                set_value(resource, self.property, Value::Map(m));
            }
            Kind::Stack => {
                let template_path = resource.get(self.property).cloned();
                let Some(template_path) = template_path.filter(|v| !matches!(v, Value::Null))
                else {
                    return Ok(());
                };
                let Value::Str(tp) = &template_path else {
                    return Err(CfnError::General(format!(
                        "'{}' object has no attribute 'startswith'",
                        crate::cli::shorthand::py_type_name(&template_path)
                    )));
                };
                if is_s3_url(&template_path)
                    || tp.starts_with("http://")
                    || tp.starts_with("https://")
                {
                    return Ok(());
                }
                let abs = make_abs_path(parent_dir, &template_path);
                if !is_local_file(&abs) {
                    return Err(CfnError::General(format!(
                        "{} parameter of {resource_id} resource is invalid. It must be a S3 URL or path to CloudFormation template file. Actual: {}",
                        self.property,
                        crate::pyfmt::py_str(&abs)
                    )));
                }
                let exported = Template::new(tp, Path::new(parent_dir))?.export(uploader)?;
                let text = yaml_dump(&exported);
                let file = temp_name("");
                std::fs::write(&file, text).map_err(|e| CfnError::General(e.to_string()))?;
                let url = uploader.upload_with_dedup(&file, Some("template"));
                let _ = std::fs::remove_file(&file);
                let url = url?;
                let (_, key, version) = parse_s3_url(&Value::Str(url)).unwrap_or_default();
                let s3_path_url = uploader.to_path_style_s3_url(&key, version.as_deref());
                set_value(resource, self.property, Value::Str(s3_path_url));
            }
        }
        Ok(())
    }
}

/// `Template`.
struct Template {
    dict: Value,
    dir: String,
}

impl Template {
    fn new(template_path: &str, parent_dir: &Path) -> Result<Template, CfnError> {
        let parent = parent_dir.to_string_lossy().into_owned();
        if !(parent_dir.is_dir() && parent.starts_with('/')) {
            return Err(CfnError::General(format!(
                "parent_dir parameter must be an absolute path to a folder {parent}"
            )));
        }
        let abs = match make_abs_path(&parent, &Value::str(template_path)) {
            Value::Str(s) => s,
            _ => unreachable!("string path"),
        };
        let dir = Path::new(&abs)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = std::fs::read(&abs).map_err(|e| CfnError::General(os_error(&e, &abs)))?;
        let text = String::from_utf8(text).map_err(|e| {
            CfnError::General(format!(
                "'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte",
                e.as_bytes()[e.utf8_error().valid_up_to()],
                e.utf8_error().valid_up_to()
            ))
        })?;
        let dict = yaml_parse(&text).map_err(CfnError::General)?;
        Ok(Template { dict, dir })
    }

    /// `Template.export`.
    fn export(mut self, uploader: &Uploader<'_>) -> Result<Value, CfnError> {
        self.export_metadata(uploader)?;
        let has_resources = self
            .dict
            .as_map()
            .is_some_and(|m| m.contains_key("Resources"));
        if !has_resources {
            return Ok(self.dict);
        }
        export_global_artifacts(&mut self.dict, uploader, &self.dir)?;
        if let Some(resources) = self.dict.as_map_mut().and_then(|m| m.get_mut("Resources")) {
            export_resources(resources, uploader, &self.dir)?;
        }
        Ok(self.dict)
    }

    fn export_metadata(&mut self, uploader: &Uploader<'_>) -> Result<(), CfnError> {
        let dir = self.dir.clone();
        let Some(Value::Map(metadata)) = self.dict.as_map_mut().and_then(|m| m.get_mut("Metadata"))
        else {
            return Ok(());
        };
        for (metadata_type, metadata_dict) in metadata.iter_mut() {
            for exporter in METADATA {
                if exporter.resource_type != metadata_type {
                    continue;
                }
                exporter.export(metadata_type, metadata_dict, &dir, uploader)?;
            }
        }
        Ok(())
    }
}

/// `include_transform_export_handler`.
fn include_transform(
    value: &mut Value,
    uploader: &Uploader<'_>,
    parent_dir: &str,
) -> Result<(), CfnError> {
    if value.get("Name") != Some(&Value::str("AWS::Include")) {
        return Ok(());
    }
    let location = value
        .get("Parameters")
        .and_then(|p| p.get("Location"))
        .cloned();
    let Some(location) = location.filter(Value::is_truthy) else {
        return Ok(());
    };
    let Value::Str(loc) = &location else {
        return Ok(());
    };
    if is_s3_url(&location) {
        return Ok(());
    }
    let abs = join(parent_dir, loc);
    if Path::new(&abs).is_file() {
        let url = uploader.upload_with_dedup(Path::new(&abs), None)?;
        if let Some(Value::Map(p)) = value.as_map_mut().and_then(|m| m.get_mut("Parameters")) {
            p.insert("Location".to_string(), Value::Str(url));
        }
        Ok(())
    } else {
        Err(CfnError::General(format!(
            "Parameter Location of resource AWS::Include refers to a file or folder that does not exist {abs}"
        )))
    }
}

/// `Template.export_global_artifacts`.
fn export_global_artifacts(
    value: &mut Value,
    uploader: &Uploader<'_>,
    dir: &str,
) -> Result<(), CfnError> {
    let Value::Map(m) = value else { return Ok(()) };
    for (key, val) in m.iter_mut() {
        if key == "Fn::Transform" {
            if val.as_map().is_some() {
                include_transform(val, uploader, dir)?;
            } else {
                return Err(CfnError::General(format!(
                    "'{}' object has no attribute 'get'",
                    crate::cli::shorthand::py_type_name(val)
                )));
            }
        } else if val.as_map().is_some() {
            export_global_artifacts(val, uploader, dir)?;
        } else if let Value::List(items) = val {
            for item in items {
                if item.as_map().is_some() {
                    export_global_artifacts(item, uploader, dir)?;
                }
            }
        }
    }
    Ok(())
}

/// `Template.export_resources`.
fn export_resources(
    resources: &mut Value,
    uploader: &Uploader<'_>,
    dir: &str,
) -> Result<(), CfnError> {
    let Value::Map(m) = resources else {
        return Err(CfnError::General(format!(
            "'{}' object has no attribute 'items'",
            crate::cli::shorthand::py_type_name(resources)
        )));
    };
    for (resource_id, resource) in m.iter_mut() {
        if resource_id.starts_with("Fn::ForEach::") {
            match resource {
                Value::List(l) if l.len() == 3 => {
                    export_resources(&mut l[2], uploader, dir)?;
                    continue;
                }
                _ => {
                    return Err(CfnError::General(format!(
                        "The value of {resource_id} has an invalid \"Fn::ForEach::\" format: Must be a list of three entries"
                    )));
                }
            }
        }
        let Value::Map(r) = resource else {
            return Err(CfnError::General(format!(
                "'{}' object has no attribute 'get'",
                crate::cli::shorthand::py_type_name(resource)
            )));
        };
        let resource_type = r.get("Type").cloned();
        for exporter in RESOURCES {
            if resource_type.as_ref() != Some(&Value::str(exporter.resource_type)) {
                continue;
            }
            if let Some(props) = r.get_mut("Properties") {
                exporter.export(resource_id, props, dir, uploader)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(normpath("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(normpath("a/../../b"), "../b");
        assert_eq!(normpath("//a"), "//a");
        assert_eq!(
            parse_s3_url(&Value::str("s3://b/k/x?versionId=1")),
            Some(("b".into(), "k/x".into(), Some("1".into())))
        );
        assert_eq!(parse_s3_url(&Value::str("s3://b")), None);
    }
}
