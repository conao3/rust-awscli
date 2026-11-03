//! Server-side serialization of generated values and errors in each
//! protocol, the inverse of botocore's `parsers.py`.

use awsr::model::{Operation, ServiceModel, Shape};
use serde_json::Value as Json;

use crate::rng::Rng;
use crate::values::{G, Proto};
use crate::wire::{self, C, El, J, JsonStyle};

#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct Ser<'a> {
    pub model: &'a ServiceModel,
    pub proto: Proto,
    pub rng: Rng,
}

const S3_NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

fn location<'a>(m: &Shape<'a>) -> Option<&'a str> {
    m.member_get("location").and_then(Json::as_str)
}

fn xml_name<'a>(m: &Shape<'a>, fallback: &'a str) -> &'a str {
    m.get_str("locationName").unwrap_or(fallback)
}

/// botocore `BaseXMLResponseParser._member_key_name`.
fn member_key_name<'a>(m: &Shape<'a>, name: &'a str) -> &'a str {
    if m.type_name() == "list"
        && m.get_bool("flattened")
        && let Some(n) = m.list_member().and_then(|l| l.get_str("locationName"))
    {
        return n;
    }
    xml_name(m, name)
}

impl Ser<'_> {
    fn is_s3(&self) -> bool {
        matches!(self.model.name.as_str(), "s3" | "s3control")
    }

    fn timestamp_format(&self, s: &Shape<'_>, header: bool) -> &'static str {
        match s.get_str("timestampFormat") {
            Some("iso8601") => "iso8601",
            Some("rfc822") => "rfc822",
            Some("unixTimestamp") => "unixTimestamp",
            _ if header => "rfc822",
            _ => match self.proto {
                Proto::Json | Proto::RestJson | Proto::Cbor => "unixTimestamp",
                _ => "iso8601",
            },
        }
    }

    fn ts_text(&self, s: &Shape<'_>, ts: crate::values::Ts, header: bool) -> (String, bool) {
        match self.timestamp_format(s, header) {
            "iso8601" => (wire::iso8601(ts), false),
            "rfc822" => (wire::rfc822(ts), false),
            _ => (wire::epoch(ts), true),
        }
    }

    // ------------------------------------------------------------ JSON

    pub fn json_value(&mut self, s: &Shape<'_>, g: &G) -> J {
        match g {
            G::Null => J::Null,
            G::Bool(b) => J::Bool(*b),
            G::Int(i) => J::Num(i.clone()),
            G::Float(f) => J::Num(f.text.clone()),
            G::Str(v) => J::Str(v.clone()),
            G::Blob(b) => J::Str(wire::b64(b)),
            G::Ts(ts) => {
                let (text, numeric) = self.ts_text(s, *ts, false);
                if numeric { J::Num(text) } else { J::Str(text) }
            }
            G::Doc(d) => J::from_serde(d),
            G::List(items) => {
                let m = s.list_member();
                J::Arr(
                    items
                        .iter()
                        .map(|v| match &m {
                            Some(m) => self.json_value(m, v),
                            None => J::Null,
                        })
                        .collect(),
                )
            }
            G::Map(entries) => {
                let vs = s.map_value();
                J::Obj(
                    entries
                        .iter()
                        .map(|(k, v)| {
                            let jv = match &vs {
                                Some(vs) => self.json_value(vs, v),
                                None => J::Null,
                            };
                            (k.clone(), jv)
                        })
                        .collect(),
                )
            }
            G::Struct(members) => {
                let mut out = vec![];
                for (name, v) in members {
                    let Some(m) = s.member_shape(name) else {
                        continue;
                    };
                    if location(&m).is_some() {
                        continue;
                    }
                    let key = m.get_str("locationName").unwrap_or(name).to_string();
                    out.push((key, self.json_value(&m, v)));
                }
                if out.len() > 1 && self.rng.chance(1, 3) {
                    out.reverse();
                }
                if !s.is_union() && self.rng.chance(1, 8) {
                    out.push(("unmodeledExtraKey".to_string(), J::Num("1".into())));
                }
                J::Obj(out)
            }
        }
    }

    fn json_doc(&mut self, j: &J) -> Vec<u8> {
        let style = JsonStyle {
            ascii: self.rng.chance(1, 3),
            spaced: self.rng.chance(1, 2),
        };
        wire::json_text(j, style).into_bytes()
    }

    // ------------------------------------------------------------- XML

    fn namespace_attr(el: &mut El, s: &Shape<'_>) {
        if let Some(ns) = s.get("xmlNamespace").and_then(Json::as_object)
            && let Some(uri) = ns.get("uri").and_then(Json::as_str)
        {
            match ns.get("prefix").and_then(Json::as_str) {
                Some(p) => el.attrs.push((format!("xmlns:{p}"), uri.to_string())),
                None => el.attrs.push(("xmlns".to_string(), uri.to_string())),
            }
        }
    }

    fn xml_scalar(&self, s: &Shape<'_>, g: &G) -> Option<String> {
        Some(match g {
            G::Bool(b) => b.to_string(),
            G::Int(i) => i.clone(),
            G::Float(f) => f.text.clone(),
            G::Str(v) => v.clone(),
            G::Blob(b) => wire::b64(b),
            G::Ts(ts) => self.ts_text(s, *ts, false).0,
            G::Doc(d) => d.to_string(),
            _ => return None,
        })
    }

    /// Fill `el` with the content of `g`.
    fn xml_content(&mut self, el: &mut El, s: &Shape<'_>, g: &G) {
        match g {
            G::Struct(members) => {
                let mut used: Vec<&str> = vec![];
                for (name, v) in members {
                    let Some(m) = s.member_shape(name) else {
                        continue;
                    };
                    if location(&m).is_some() {
                        continue;
                    }
                    // Two members serialized under one element name would be
                    // merged into a list by the parser
                    // (`_build_name_to_xml_node`); a service never sends both.
                    let key = member_key_name(&m, name);
                    if !m.get_bool("xmlAttribute") {
                        if used.contains(&key) {
                            continue;
                        }
                        used.push(key);
                    }
                    self.xml_member(el, name, &m, v);
                }
            }
            G::List(items) => {
                if let Some(m) = s.list_member() {
                    let n = xml_name(&m, "member").to_string();
                    for v in items {
                        let mut c = El::new(n.clone());
                        self.xml_content(&mut c, &m, v);
                        el.children.push(c);
                    }
                }
            }
            G::Map(entries) => self.xml_map_entries(el, s, entries, "entry"),
            G::Null => {}
            other => {
                el.text = Some(self.xml_scalar(s, other).unwrap_or_default());
            }
        }
    }

    fn xml_map_entries(&mut self, el: &mut El, s: &Shape<'_>, entries: &[(String, G)], wrap: &str) {
        let (Some(ks), Some(vs)) = (s.map_key(), s.map_value()) else {
            return;
        };
        let kn = xml_name(&ks, "key").to_string();
        let vn = xml_name(&vs, "value").to_string();
        for (k, v) in entries {
            let mut e = El::new(wrap);
            e.children.push(El::text(kn.clone(), k.clone()));
            let mut ve = El::new(vn.clone());
            self.xml_content(&mut ve, &vs, v);
            e.children.push(ve);
            el.children.push(e);
        }
    }

    fn xml_member(&mut self, parent: &mut El, name: &str, m: &Shape<'_>, g: &G) {
        let tag = xml_name(m, name).to_string();
        if m.get_bool("xmlAttribute") {
            if let Some(text) = self.xml_scalar(m, g) {
                if let Some((prefix, _)) = tag.split_once(':')
                    && let Some(uri) = m
                        .get("xmlNamespace")
                        .and_then(|n| n.get("uri"))
                        .and_then(Json::as_str)
                {
                    let decl = format!("xmlns:{prefix}");
                    if !parent.attrs.iter().any(|(k, _)| *k == decl) {
                        parent.attrs.push((decl, uri.to_string()));
                    }
                }
                parent.attrs.push((tag, text));
            }
            return;
        }
        let flattened = m.get_bool("flattened");
        match (m.type_name(), g) {
            ("list", G::List(items)) if flattened => {
                let Some(lm) = m.list_member() else { return };
                let item_tag = lm.get_str("locationName").unwrap_or(&tag).to_string();
                for v in items {
                    let mut c = El::new(item_tag.clone());
                    self.xml_content(&mut c, &lm, v);
                    parent.children.push(c);
                }
            }
            ("map", G::Map(entries)) if flattened => {
                let mut holder = El::new("");
                self.xml_map_entries(&mut holder, m, entries, &tag);
                parent.children.extend(holder.children);
            }
            _ => {
                let mut c = El::new(tag);
                Self::namespace_attr(&mut c, m);
                self.xml_content(&mut c, m, g);
                parent.children.push(c);
            }
        }
    }

    // ------------------------------------------------------------ CBOR

    fn cbor_value(&mut self, s: &Shape<'_>, g: &G) -> C {
        match g {
            G::Null => C::Null,
            G::Bool(b) => C::Bool(*b),
            G::Int(i) => C::int(i.parse().unwrap_or(0)),
            G::Float(f) => {
                let narrow = f.value as f32;
                if f64::from(narrow) == f.value && self.rng.chance(1, 2) {
                    C::F32(narrow)
                } else {
                    C::F64(f.value)
                }
            }
            G::Str(v) => C::Text(v.clone()),
            G::Blob(b) => C::Bytes(b.clone()),
            G::Ts(ts) => wire::cbor_timestamp(*ts),
            G::Doc(d) => wire::cbor_from_json(d),
            G::List(items) => {
                let m = s.list_member();
                let items = items
                    .iter()
                    .map(|v| match &m {
                        Some(m) => self.cbor_value(m, v),
                        None => C::Null,
                    })
                    .collect();
                C::Arr(items, self.rng.chance(1, 4))
            }
            G::Map(entries) => {
                let vs = s.map_value();
                let entries = entries
                    .iter()
                    .map(|(k, v)| {
                        let cv = match &vs {
                            Some(vs) => self.cbor_value(vs, v),
                            None => C::Null,
                        };
                        (C::Text(k.clone()), cv)
                    })
                    .collect();
                C::Map(entries, self.rng.chance(1, 4))
            }
            G::Struct(members) => {
                let mut out = vec![];
                for (name, v) in members {
                    let Some(m) = s.member_shape(name) else {
                        continue;
                    };
                    out.push((C::Text(name.clone()), self.cbor_value(&m, v)));
                }
                C::Map(out, self.rng.chance(1, 4))
            }
        }
    }

    fn cbor_doc(c: &C) -> Vec<u8> {
        let mut out = vec![];
        wire::cbor_encode(&mut out, c);
        out
    }

    // --------------------------------------------------------- headers

    fn header_text(&self, s: &Shape<'_>, g: &G) -> Option<String> {
        Some(match g {
            G::Str(v) => v.clone(),
            G::Doc(d) => wire::b64(d.to_string().as_bytes()),
            G::Int(i) => i.clone(),
            G::Float(f) => f.text.clone(),
            G::Bool(b) => b.to_string(),
            G::Blob(b) => wire::b64(b),
            G::Ts(ts) => self.ts_text(s, *ts, true).0,
            G::List(items) => {
                let m = s.list_member()?;
                let parts: Vec<String> = items
                    .iter()
                    .filter_map(|v| self.header_text(&m, v))
                    .collect();
                if parts.is_empty() {
                    return None;
                }
                parts.join(", ")
            }
            _ => return None,
        })
    }

    fn rest_headers(&self, out_shape: &Shape<'_>, g: &G, resp: &mut Resp) {
        let G::Struct(members) = g else { return };
        for (name, v) in members {
            let Some(m) = out_shape.member_shape(name) else {
                continue;
            };
            match location(&m) {
                Some("header") => {
                    if let Some(text) = self.header_text(&m, v) {
                        let hn = m.get_str("locationName").unwrap_or(name);
                        resp.headers.push((hn.to_string(), text));
                    }
                }
                Some("headers") => {
                    let prefix = m.get_str("locationName").unwrap_or_default();
                    if let G::Map(entries) = v {
                        for (k, hv) in entries {
                            if let G::Str(hv) = hv {
                                resp.headers.push((format!("{prefix}{k}"), hv.clone()));
                            }
                        }
                    }
                }
                Some("statusCode") => {
                    if let G::Int(i) = v {
                        resp.status = i.parse().unwrap_or(200);
                    }
                }
                _ => {}
            }
        }
    }

    // --------------------------------------------------------- success

    fn request_id_headers(&mut self, resp: &mut Resp) -> String {
        let id = self.rng.uuid();
        if self.proto == Proto::RestXml && self.is_s3() {
            resp.headers.push(("x-amz-request-id".into(), id.clone()));
            resp.headers
                .push(("x-amz-id-2".into(), wire::b64(&self.rng.bytes(24))));
        } else if !matches!(self.proto, Proto::Query | Proto::Ec2) {
            resp.headers.push(("x-amzn-RequestId".into(), id.clone()));
        }
        id
    }

    pub fn success(&mut self, op: &Operation<'_>, out: Option<&G>) -> Resp {
        let mut resp = Resp {
            status: op
                .http()
                .and_then(|h| h.get("responseCode"))
                .and_then(Json::as_u64)
                .unwrap_or(200) as u16,
            headers: vec![],
            body: vec![],
        };
        let id = self.request_id_headers(&mut resp);
        let out_shape = op.output();
        let empty = G::Struct(vec![]);
        let g = out.unwrap_or(&empty);
        match self.proto {
            Proto::Json => {
                let version = self.model.metadata_str("jsonVersion").unwrap_or("1.0");
                resp.headers.push((
                    "Content-Type".into(),
                    format!("application/x-amz-json-{version}"),
                ));
                let j = match &out_shape {
                    Some(s) => self.json_value(s, g),
                    None => J::Obj(vec![]),
                };
                resp.body = self.json_doc(&j);
            }
            Proto::Cbor => {
                resp.headers
                    .push(("Content-Type".into(), "application/cbor".into()));
                resp.headers
                    .push(("smithy-protocol".into(), "rpc-v2-cbor".into()));
                if let Some(s) = &out_shape {
                    let c = self.cbor_value(s, g);
                    resp.body = Self::cbor_doc(&c);
                }
            }
            Proto::RestJson | Proto::RestXml => {
                let Some(s) = out_shape else {
                    if self.proto == Proto::RestJson {
                        resp.headers
                            .push(("Content-Type".into(), "application/json".into()));
                        resp.body = b"{}".to_vec();
                    }
                    return resp;
                };
                self.rest_headers(&s, g, &mut resp);
                self.rest_body(&s, g, &mut resp);
            }
            Proto::Query => {
                let mut root = El::new(format!("{}Response", op.name));
                if let Some(ns) = self.model.metadata_str("xmlNamespace") {
                    root.attrs.push(("xmlns".into(), ns.to_string()));
                }
                if let Some(s) = &out_shape {
                    match s.member_get("resultWrapper").and_then(Json::as_str) {
                        Some(w) => {
                            let mut result = El::new(w);
                            self.xml_content(&mut result, s, g);
                            root.children.push(result);
                        }
                        None => self.xml_content(&mut root, s, g),
                    }
                }
                root.children
                    .push(El::new("ResponseMetadata").child(El::text("RequestId", id)));
                resp.headers
                    .push(("Content-Type".into(), "text/xml".into()));
                resp.body = root.document(false).into_bytes();
            }
            Proto::Ec2 => {
                let mut root = El::new(format!("{}Response", op.name)).attr(
                    "xmlns",
                    format!("http://ec2.amazonaws.com/doc/{}/", self.model.api_version),
                );
                root.children.push(El::text("requestId", id));
                if let Some(s) = &out_shape {
                    self.xml_content(&mut root, s, g);
                }
                resp.headers
                    .push(("Content-Type".into(), "text/xml;charset=UTF-8".into()));
                resp.body = root.document(true).into_bytes();
            }
        }
        resp
    }

    fn rest_body(&mut self, s: &Shape<'_>, g: &G, resp: &mut Resp) {
        let json = self.proto == Proto::RestJson;
        if let Some(payload) = s.get_str("payload") {
            let Some(pm) = s.member_shape(payload) else {
                return;
            };
            let Some(pv) = g.get(payload) else {
                if json {
                    resp.headers
                        .push(("Content-Type".into(), "application/json".into()));
                }
                return;
            };
            match (pm.type_name(), pv) {
                ("blob", G::Blob(b)) => {
                    resp.headers
                        .push(("Content-Type".into(), "application/octet-stream".into()));
                    resp.body = b.clone();
                }
                ("string", G::Str(v)) => {
                    resp.headers
                        .push(("Content-Type".into(), "text/plain".into()));
                    resp.body = v.clone().into_bytes();
                }
                _ if json => {
                    resp.headers
                        .push(("Content-Type".into(), "application/json".into()));
                    let j = self.json_value(&pm, pv);
                    resp.body = self.json_doc(&j);
                }
                _ => {
                    let target = pm.name;
                    let tag = pm
                        .member_get("locationName")
                        .and_then(Json::as_str)
                        .or_else(|| pm.shape_get("locationName").and_then(Json::as_str))
                        .unwrap_or(target)
                        .to_string();
                    let mut root = El::new(tag);
                    Self::namespace_attr(&mut root, &pm);
                    if root.attrs.is_empty() && self.is_s3() {
                        root.attrs.push(("xmlns".into(), S3_NS.into()));
                    }
                    self.xml_content(&mut root, &pm, pv);
                    resp.headers
                        .push(("Content-Type".into(), "application/xml".into()));
                    resp.body = root.document(self.rng.chance(1, 2)).into_bytes();
                }
            }
            return;
        }
        let has_body_members = s.members().iter().any(|(_, m)| location(m).is_none());
        if json {
            resp.headers
                .push(("Content-Type".into(), "application/json".into()));
            let j = self.json_value(s, g);
            resp.body = self.json_doc(&j);
            return;
        }
        if !has_body_members {
            return;
        }
        if self.is_s3() && self.model.name == "s3" && s.name == "GetBucketLocationOutput" {
            let value = match g.get("LocationConstraint") {
                Some(G::Str(v)) => v.clone(),
                _ => String::new(),
            };
            let el = El::text("LocationConstraint", value).attr("xmlns", S3_NS);
            resp.body = el.document(true).into_bytes();
            resp.headers
                .push(("Content-Type".into(), "application/xml".into()));
            return;
        }
        let tag = s
            .shape_get("locationName")
            .and_then(Json::as_str)
            .unwrap_or(s.name);
        let mut root = El::new(tag);
        Self::namespace_attr(&mut root, s);
        if root.attrs.is_empty() && self.is_s3() {
            root.attrs.push(("xmlns".into(), S3_NS.into()));
        }
        self.xml_content(&mut root, s, g);
        resp.headers
            .push(("Content-Type".into(), "application/xml".into()));
        resp.body = root.document(self.rng.chance(1, 2)).into_bytes();
    }

    // ----------------------------------------------------------- errors

    /// An error response. `shape` is the modeled error shape (with its
    /// generated members in `members`), or `None` for an unmodeled error
    /// with the given `code` and `status`.
    pub fn error(
        &mut self,
        shape: Option<&Shape<'_>>,
        members: &G,
        code: &str,
        status: u16,
        message: &str,
    ) -> Resp {
        let mut resp = Resp {
            status,
            headers: vec![],
            body: vec![],
        };
        let id = self.request_id_headers(&mut resp);
        let mut members = members.clone();
        for k in ["message", "Message"] {
            members.remove(k);
        }
        let qualified = match self.rng.below(3) {
            0 => code.to_string(),
            1 => format!(
                "com.amazonaws.{}#{code}",
                self.model.endpoint_prefix().replace('-', "")
            ),
            _ => format!("{code}:http://internal.amazon.com/coral/com.amazonaws.test/"),
        };
        let message_key = if self.rng.chance(1, 3) {
            "Message"
        } else {
            "message"
        };
        match self.proto {
            Proto::Json | Proto::RestJson => {
                let mut entries = vec![];
                if self.proto == Proto::Json {
                    let version = self.model.metadata_str("jsonVersion").unwrap_or("1.0");
                    resp.headers.push((
                        "Content-Type".into(),
                        format!("application/x-amz-json-{version}"),
                    ));
                    entries.push(("__type".to_string(), J::Str(qualified)));
                    if self.model.metadata().contains_key("awsQueryCompatible")
                        && let Some(s) = shape
                        && let Some(qcode) = s
                            .shape_get("error")
                            .and_then(|e| e.get("code"))
                            .and_then(Json::as_str)
                    {
                        resp.headers
                            .push(("x-amzn-query-error".into(), format!("{qcode};Sender")));
                    }
                } else {
                    resp.headers
                        .push(("Content-Type".into(), "application/json".into()));
                    if self.rng.chance(2, 3) {
                        resp.headers.push(("X-Amzn-ErrorType".into(), qualified));
                    } else {
                        entries.push(("code".to_string(), J::Str(code.to_string())));
                    }
                }
                entries.push((message_key.to_string(), J::Str(message.to_string())));
                if let J::Obj(extra) = self.json_value_opt(shape, &members) {
                    entries.extend(extra);
                }
                resp.body = self.json_doc(&J::Obj(entries));
            }
            Proto::Cbor => {
                resp.headers
                    .push(("Content-Type".into(), "application/cbor".into()));
                resp.headers
                    .push(("smithy-protocol".into(), "rpc-v2-cbor".into()));
                let mut entries = vec![
                    (C::Text("__type".into()), C::Text(qualified)),
                    (C::Text(message_key.into()), C::Text(message.into())),
                ];
                if let Some(s) = shape
                    && let C::Map(extra, _) = self.cbor_value(s, &members)
                {
                    entries.extend(extra);
                }
                resp.body = Self::cbor_doc(&C::Map(entries, false));
            }
            Proto::Query | Proto::RestXml if !(self.proto == Proto::RestXml && self.is_s3()) => {
                let mut err = El::new("Error")
                    .child(El::text("Type", "Sender"))
                    .child(El::text("Code", code))
                    .child(El::text("Message", message));
                if let Some(s) = shape {
                    self.xml_content(&mut err, s, &members);
                }
                let mut root = El::new("ErrorResponse")
                    .child(err)
                    .child(El::text("RequestId", id));
                if let Some(ns) = self.model.metadata_str("xmlNamespace") {
                    root.attrs.push(("xmlns".into(), ns.to_string()));
                }
                resp.headers
                    .push(("Content-Type".into(), "text/xml".into()));
                resp.body = root.document(false).into_bytes();
            }
            Proto::RestXml => {
                let host_id = resp
                    .headers
                    .iter()
                    .find(|(k, _)| k == "x-amz-id-2")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let mut err = El::new("Error")
                    .child(El::text("Code", code))
                    .child(El::text("Message", message));
                if let Some(s) = shape {
                    self.xml_content(&mut err, s, &members);
                }
                err.children.push(El::text("RequestId", id));
                err.children.push(El::text("HostId", host_id));
                resp.headers
                    .push(("Content-Type".into(), "application/xml".into()));
                resp.body = err.document(true).into_bytes();
            }
            Proto::Ec2 | Proto::Query => {
                let root = El::new("Response")
                    .child(
                        El::new("Errors").child(
                            El::new("Error")
                                .child(El::text("Code", code))
                                .child(El::text("Message", message)),
                        ),
                    )
                    .child(El::text("RequestID", id));
                resp.headers
                    .push(("Content-Type".into(), "text/xml;charset=UTF-8".into()));
                resp.body = root.document(true).into_bytes();
            }
        }
        resp
    }

    fn json_value_opt(&mut self, shape: Option<&Shape<'_>>, g: &G) -> J {
        match shape {
            Some(s) => match self.json_value(s, g) {
                J::Obj(mut e) => {
                    e.retain(|(k, _)| k != "unmodeledExtraKey");
                    J::Obj(e)
                }
                other => other,
            },
            None => J::Obj(vec![]),
        }
    }
}
