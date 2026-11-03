//! The `rest-xml` protocol (`botocore/serialize.py` `RestXMLSerializer`,
//! `botocore/parsers.py` `RestXMLParser`).

use anyhow::Result;

use crate::model::{Operation, ServiceModel};
use crate::protocol::{HttpRequest, HttpResponse, ParseOptions, ServiceError};
use crate::value::Value;

pub fn serialize(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
) -> Result<HttpRequest> {
    let _ = (model, operation, params);
    todo!("rest_xml::serialize")
}

pub fn parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<Value> {
    let _ = (model, operation, response, options);
    todo!("rest_xml::parse")
}

pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    let _ = (model, operation, response, options);
    todo!("rest_xml::parse_error")
}
