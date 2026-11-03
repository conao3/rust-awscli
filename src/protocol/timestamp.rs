//! Timestamps in responses (`botocore.utils.parse_timestamp`,
//! `awscli/customizations/timestampformat.py`) and requests
//! (`botocore/serialize.py` `_timestamp_*`).

use crate::protocol::TimestampFormat;
use crate::value::Value;

/// A timestamp as it appears on the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum WireTimestamp {
    /// A JSON / CBOR number (epoch seconds).
    Number(Value),
    /// Text (XML, headers, JSON strings).
    Text(String),
}

/// Convert a wire timestamp to the CLI output value: with `Iso8601`,
/// `parse_timestamp(value).isoformat()`; with `Wire`, the value unchanged
/// (numbers stay numbers, text stays text).
pub fn output_value(wire: WireTimestamp, format: TimestampFormat) -> Value {
    let _ = (wire, format);
    todo!("timestamp::output_value")
}

/// Serialize an input timestamp value (a string or number from the command
/// line or JSON) in the given wire format (`iso8601`, `rfc822`,
/// `unixTimestamp`), the way botocore's serializers do after
/// `parse_to_aware_datetime`.
pub fn serialize(value: &Value, wire_format: &str) -> anyhow::Result<Value> {
    let _ = (value, wire_format);
    todo!("timestamp::serialize")
}
