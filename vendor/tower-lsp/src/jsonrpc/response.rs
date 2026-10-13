use std::fmt::{self, Debug, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{Error, Id, Result, Version};

/// A successful JSON-RPC result serialized without an intermediate JSON value tree.
///
/// Construction validates the encoding through the serializer. Incoming results retain the
/// original JSON value deserialization behavior before being encoded for typed decoding.
#[derive(Clone, Debug)]
pub struct EncodedResult(Box<str>);

impl EncodedResult {
    /// Serializes a successful result directly to its JSON representation.
    pub fn new<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Self> {
        serde_json::to_string(value).map(|json| Self(json.into_boxed_str()))
    }

    /// Returns the serialized JSON result.
    pub fn get(&self) -> &str {
        &self.0
    }

    /// Returns the serialized JSON result.
    pub fn as_str(&self) -> &str {
        self.get()
    }
}

impl<'de> Deserialize<'de> for EncodedResult {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::new(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum Kind {
    Ok { result: EncodedResult },
    Err { error: Error },
}

/// A successful or failed JSON-RPC response.
#[derive(Clone, Deserialize)]
pub struct Response {
    jsonrpc: Version,
    #[serde(flatten)]
    kind: Kind,
    id: Id,
}

impl Response {
    /// Creates a new successful response from a request ID and serialized JSON result.
    pub const fn from_ok(id: Id, result: EncodedResult) -> Self {
        Response {
            jsonrpc: Version,
            kind: Kind::Ok { result },
            id,
        }
    }

    /// Creates a new error response from a request ID and `Error` object.
    pub const fn from_error(id: Id, error: Error) -> Self {
        Response {
            jsonrpc: Version,
            kind: Kind::Err { error },
            id,
        }
    }

    /// Creates a response from a request ID and either serialized JSON or an error body.
    pub fn from_parts(id: Id, body: Result<EncodedResult>) -> Self {
        match body {
            Ok(result) => Response::from_ok(id, result),
            Err(error) => Response::from_error(id, error),
        }
    }

    /// Splits the response into its request ID and serialized JSON result or error.
    pub fn into_parts(self) -> (Id, Result<EncodedResult>) {
        match self.kind {
            Kind::Ok { result } => (self.id, Ok(result)),
            Kind::Err { error } => (self.id, Err(error)),
        }
    }

    /// Returns `true` if the response indicates success.
    pub const fn is_ok(&self) -> bool {
        matches!(self.kind, Kind::Ok { .. })
    }

    /// Returns `true` if the response indicates failure.
    pub const fn is_error(&self) -> bool {
        !self.is_ok()
    }

    /// Returns the `result` value, if it exists.
    ///
    /// This member only exists if the response indicates success.
    pub const fn result(&self) -> Option<&EncodedResult> {
        match &self.kind {
            Kind::Ok { result } => Some(result),
            _ => None,
        }
    }

    /// Returns the `error` value, if it exists.
    ///
    /// This member only exists if the response indicates failure.
    pub const fn error(&self) -> Option<&Error> {
        match &self.kind {
            Kind::Err { error } => Some(error),
            _ => None,
        }
    }

    /// Returns the corresponding request ID, if known.
    pub const fn id(&self) -> &Id {
        &self.id
    }

    /// Encodes the complete JSON-RPC response without parsing the successful result.
    pub fn to_json_bytes(&self) -> serde_json::Result<Vec<u8>> {
        let result_len = self.result().map_or(0, |result| result.get().len());
        let mut bytes = Vec::with_capacity(result_len + 64);
        bytes.extend_from_slice(b"{\"jsonrpc\":\"2.0\",");
        match &self.kind {
            Kind::Ok { result } => {
                bytes.extend_from_slice(b"\"result\":");
                bytes.extend_from_slice(result.get().as_bytes());
            }
            Kind::Err { error } => {
                bytes.extend_from_slice(b"\"error\":");
                serde_json::to_writer(&mut bytes, error)?;
            }
        }
        bytes.extend_from_slice(b",\"id\":");
        serde_json::to_writer(&mut bytes, &self.id)?;
        bytes.push(b'}');
        Ok(bytes)
    }
}

impl Debug for Response {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let mut d = f.debug_struct("Response");
        d.field("jsonrpc", &self.jsonrpc);

        match &self.kind {
            Kind::Ok { result } => d.field("result", result),
            Kind::Err { error } => d.field("error", error),
        };

        d.field("id", &self.id).finish()
    }
}

impl FromStr for Response {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn preserves_successful_encoded_parts_through_wire_round_trip() {
        let payload = json!({"nested":{"values":[null, true, -5, "text"]}});
        let response =
            Response::from_parts("request".into(), Ok(EncodedResult::new(&payload).unwrap()));
        assert!(response.is_ok());
        assert!(response.error().is_none());
        assert_eq!(response.id(), &Id::String("request".into()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(response.result().unwrap().get()).unwrap(),
            payload
        );
        let wire = response.to_json_bytes().unwrap();
        let decoded: Response = serde_json::from_reader(wire.as_slice()).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&decoded.to_json_bytes().unwrap()).unwrap(),
            json!({"jsonrpc":"2.0","result":payload,"id":"request"})
        );
        let (id, result) = decoded.into_parts();
        assert_eq!(id, Id::String("request".into()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(result.unwrap().get()).unwrap(),
            payload
        );
    }

    #[test]
    fn preserves_error_parts_through_wire_round_trip() {
        let error = Error::invalid_request();
        let response = Response::from_parts(Id::Null, Err(error.clone()));
        assert!(response.is_error());
        assert!(response.result().is_none());
        assert_eq!(response.error(), Some(&error));
        let wire = response.to_json_bytes().unwrap();
        let decoded: Response = serde_json::from_slice(&wire).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&decoded.to_json_bytes().unwrap()).unwrap(),
            json!({"jsonrpc":"2.0","error":{"code":-32600,"message":"Invalid request"},"id":null})
        );
        let (id, result) = decoded.into_parts();
        assert_eq!(id, Id::Null);
        assert_eq!(result.unwrap_err(), error);
    }
}
