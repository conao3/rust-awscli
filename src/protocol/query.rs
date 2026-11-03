//! The `query` protocol (`botocore/serialize.py` `QuerySerializer`,
//! `botocore/parsers.py` `QueryParser`).

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
    todo!("query::serialize")
}

pub fn parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<Value> {
    let _ = (model, operation, response, options);
    todo!("query::parse")
}

pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    let _ = (model, operation, response, options);
    todo!("query::parse_error")
}
