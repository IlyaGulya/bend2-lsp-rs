//! A subset of JSON-RPC types used by the Language Server Protocol.

pub(crate) use self::error::not_initialized_error;
pub use self::error::{Error, ErrorCode, Result};
pub use self::request::{Request, RequestBuilder};
pub use self::response::{EncodedResult, Response};
pub(crate) use self::router::Router;
pub use self::router::{FromParams, IntoResponse, Method};

use std::borrow::Cow;
use std::fmt::{self, Debug, Display, Formatter};
use std::str::FromStr;

use lsp_types::NumberOrString;
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

mod error;
mod request;
mod response;
mod router;

/// A unique ID used to correlate requests and responses together.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric ID.
    Number(i64),
    /// String ID.
    String(String),
    /// Null ID.
    ///
    /// While `null` is considered a valid request ID by the JSON-RPC 2.0 specification, its use is
    /// _strongly_ discouraged because the specification also uses a `null` value to indicate an
    /// unknown ID in the [`Response`] object.
    #[default]
    Null,
}

impl Display for Id {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Id::Number(id) => Display::fmt(id, f),
            Id::String(id) => Debug::fmt(id, f),
            Id::Null => f.write_str("null"),
        }
    }
}

impl From<i64> for Id {
    fn from(n: i64) -> Self {
        Id::Number(n)
    }
}

impl From<&'_ str> for Id {
    fn from(s: &'_ str) -> Self {
        Id::String(s.to_string())
    }
}

impl From<String> for Id {
    fn from(s: String) -> Self {
        Id::String(s)
    }
}

impl From<NumberOrString> for Id {
    fn from(num_or_str: NumberOrString) -> Self {
        match num_or_str {
            NumberOrString::Number(num) => Id::Number(num as i64),
            NumberOrString::String(s) => Id::String(s),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Version;

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Inner<'a>(#[serde(borrow)] Cow<'a, str>);

        let Inner(ver) = Inner::deserialize(deserializer)?;

        match ver.as_ref() {
            "2.0" => Ok(Version),
            _ => Err(de::Error::custom("expected JSON-RPC version \"2.0\"")),
        }
    }
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str("2.0")
    }
}

/// An incoming or outgoing JSON-RPC message.
#[derive(Deserialize)]
#[cfg_attr(test, derive(Debug))]
#[serde(untagged)]
pub(crate) enum Message {
    /// A response message.
    Response(Response),
    /// A request or notification message.
    Request(Request),
}

impl FromStr for Message {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl Message {
    pub(crate) fn to_json_bytes(&self) -> serde_json::Result<Vec<u8>> {
        match self {
            Message::Response(response) => response.to_json_bytes(),
            Message::Request(request) => serde_json::to_vec(request),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reserved_json_object_keys_remain_literal_request_data() {
        let message: Message = r#"{"jsonrpc":"2.0","id":1,"method":"example","params":{"$serde_json::private::RawValue":"1"}}"#
            .parse()
            .unwrap();
        let Message::Request(request) = message else {
            panic!("request was not dispatched as a request");
        };
        assert_eq!(
            request.params(),
            Some(&json!({"$serde_json::private::RawValue": "1"}))
        );
    }

    #[test]
    fn incoming_round_trip() {
        let v = json!({"jsonrpc":"2.0","method":"initialize","params":{"capabilities":{}},"id":0});
        let message: Message = v.to_string().parse().unwrap();
        let from_value: Message = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&from_value.to_json_bytes().unwrap())
                .unwrap(),
            v
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&message.to_json_bytes().unwrap()).unwrap(),
            v
        );
    }

    #[test]
    fn outgoing_round_trip() {
        let v = json!({"jsonrpc":"2.0","result":{},"id":1});
        let message: Message = v.to_string().parse().unwrap();
        let from_value: Message = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&from_value.to_json_bytes().unwrap())
                .unwrap(),
            v
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&message.to_json_bytes().unwrap()).unwrap(),
            v
        );
    }

    #[test]
    fn parses_incoming_message() {
        let server_request =
            json!({"jsonrpc":"2.0","method":"initialize","params":{"capabilities":{}},"id":0});
        let incoming: Message = server_request.to_string().parse().unwrap();
        assert!(matches!(incoming, Message::Request(_)));

        let server_notif = json!({"jsonrpc":"2.0","method":"initialized","params":{}});
        let incoming: Message = server_notif.to_string().parse().unwrap();
        assert!(matches!(incoming, Message::Request(_)));

        let client_request = json!({"jsonrpc":"2.0","id":0,"result":[null]});
        let incoming: Message = client_request.to_string().parse().unwrap();
        assert!(matches!(incoming, Message::Response(_)));
    }

    #[test]
    fn parses_outgoing_message() {
        let client_request = json!({"jsonrpc":"2.0","method":"workspace/configuration","params":{"scopeUri":null,"section":"foo"},"id":0});
        let outgoing: Message = client_request.to_string().parse().unwrap();
        assert!(matches!(outgoing, Message::Request(_)));

        let client_notif = json!({"jsonrpc":"2.0","method":"window/logMessage","params":{"message":"foo","type":0}});
        let outgoing: Message = client_notif.to_string().parse().unwrap();
        assert!(matches!(outgoing, Message::Request(_)));

        let server_response = json!({"jsonrpc":"2.0","id":0,"result":[null]});
        let outgoing: Message = server_response.to_string().parse().unwrap();
        assert!(matches!(outgoing, Message::Response(_)));
    }

    #[test]
    fn parses_invalid_server_request() {
        let unknown_method = json!({"jsonrpc":"2.0","method":"foo"});
        let incoming: Message = unknown_method.to_string().parse().unwrap();
        assert!(matches!(incoming, Message::Request(_)));

        let unknown_method_with_id = json!({"jsonrpc":"2.0","method":"foo","id":0});
        let incoming: Message = unknown_method_with_id.to_string().parse().unwrap();
        assert!(matches!(incoming, Message::Request(_)));

        let missing_method = json!({"jsonrpc":"2.0"});
        let incoming: Message = missing_method.to_string().parse().unwrap();
        assert!(matches!(incoming, Message::Request(_)));

        let missing_method_with_id = json!({"jsonrpc":"2.0","id":0});
        let incoming: Message = missing_method_with_id.to_string().parse().unwrap();
        assert!(matches!(incoming, Message::Request(_)));
    }

    #[test]
    fn accepts_null_request_id() {
        let request_id: Id = serde_json::from_value(json!(null)).unwrap();
        assert_eq!(request_id, Id::Null);
    }

    #[test]
    fn accepts_negative_integer_request_id() {
        let request_id: Id = serde_json::from_value(json!(-1)).unwrap();
        assert_eq!(request_id, Id::Number(-1));
    }

    #[test]
    fn preserves_response_first_dispatch_and_branch_precedence() {
        let responses = [
            (
                r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
                json!(null),
                json!(1),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":4,"error":{"code":-32603,"message":"bad"}}"#,
                json!(4),
                json!(1),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"error":7,"result":4}"#,
                json!(4),
                json!(1),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":4,"method":7}"#,
                json!(4),
                json!(1),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":null,"method":"a","method":"b"}"#,
                json!(null),
                json!(1),
            ),
            (
                r#"{"result":{"b":2,"a":1},"id":"str","jsonrpc":"2.0","unknown":false}"#,
                json!({"a":1,"b":2}),
                json!("str"),
            ),
            (
                r#"{"jsonrpc":"2.0","id":null,"error":{"unknown":true},"result":"success"}"#,
                json!("success"),
                json!(null),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"a"},"error":{"code":2,"message":"b"},"result":null}"#,
                json!(null),
                json!(1),
            ),
        ];
        for (source, result, id) in responses {
            let message: Message = source.parse().unwrap();
            assert!(matches!(&message, Message::Response(_)), "{source}");
            let expected = json!({"jsonrpc":"2.0","result":result,"id":id});
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&message.to_json_bytes().unwrap())
                    .unwrap(),
                expected,
                "{source}"
            );
            let response: Response = source.parse().unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&response.to_json_bytes().unwrap())
                    .unwrap(),
                expected,
                "{source}"
            );
        }

        let source = r#"{"jsonrpc":"2.0","id":1,"result":1,"result":2,"error":{"code":-32603,"message":"bad"}}"#;
        let message: Message = source.parse().unwrap();
        assert!(matches!(&message, Message::Response(_)));
        let expected = json!({"jsonrpc":"2.0","error":{"code":-32603,"message":"bad"},"id":1});
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&message.to_json_bytes().unwrap()).unwrap(),
            expected
        );
        let response: Response = source.parse().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response.to_json_bytes().unwrap())
                .unwrap(),
            expected
        );
    }

    #[test]
    fn falls_back_to_requests_for_invalid_response_branches() {
        let requests = [
            (
                r#"{"jsonrpc":"2.0","id":1,"result":1,"result":2}"#,
                json!({"jsonrpc":"2.0","method":"","id":1}),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"a"},"error":{"code":2,"message":"b"}}"#,
                json!({"jsonrpc":"2.0","method":"","id":1}),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"a","unknown":true}}"#,
                json!({"jsonrpc":"2.0","method":"","id":1}),
            ),
            (
                r#"{"jsonrpc":"2.0","result":null}"#,
                json!({"jsonrpc":"2.0","method":""}),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1}"#,
                json!({"jsonrpc":"2.0","method":"","id":1}),
            ),
            (
                r#"{"jsonrpc":"2.0","id":null,"method":"a"}"#,
                json!({"jsonrpc":"2.0","method":"a","id":null}),
            ),
        ];
        for (source, expected) in requests {
            assert!(source.parse::<Response>().is_err(), "{source}");
            let message: Message = source.parse().unwrap();
            assert!(matches!(&message, Message::Request(_)), "{source}");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&message.to_json_bytes().unwrap())
                    .unwrap(),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_ids_and_versions() {
        for source in [
            r#"{"jsonrpc":"2.0","id":1,"id":2,"result":null}"#,
            r#"{"jsonrpc":"2.0","jsonrpc":"2.0","id":1,"result":null}"#,
        ] {
            assert!(source.parse::<Response>().is_err());
            assert!(source.parse::<Message>().unwrap_err().is_data());
        }
    }

    #[test]
    fn preserves_response_id_domains() {
        for id in [
            json!(null),
            json!("string"),
            json!(i64::MIN),
            json!(i64::MAX),
        ] {
            let source = json!({"jsonrpc":"2.0","result":null,"id":id}).to_string();
            let message: Message = source.parse().unwrap();
            assert!(matches!(&message, Message::Response(_)));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&message.to_json_bytes().unwrap())
                    .unwrap(),
                json!({"jsonrpc":"2.0","result":null,"id":id})
            );
        }
        for id in [
            json!(true),
            json!(1.5),
            json!([]),
            json!({}),
            json!(u64::MAX),
        ] {
            let source = json!({"jsonrpc":"2.0","result":null,"id":id}).to_string();
            assert!(source.parse::<Response>().is_err());
            assert!(source.parse::<Message>().unwrap_err().is_data());
        }
    }

    #[test]
    fn preserves_json_error_categories() {
        for source in [
            "{}",
            "null",
            "[]",
            r#"{"jsonrpc":"1.0","result":null,"id":1}"#,
        ] {
            assert!(source.parse::<Message>().unwrap_err().is_data(), "{source}");
        }
        for source in [
            r#"{"jsonrpc":"2.0","result":1e400,"id":1}"#,
            r#"{"jsonrpc":"2.0","method":"a","unknown":1e400}"#,
            r#"{"jsonrpc":"2.0","result":null,"id":1,"error":{"code":1e400}}"#,
            r#"{"jsonrpc":"2.0","result":null,"id":1} trailing"#,
        ] {
            assert!(
                source.parse::<Message>().unwrap_err().is_syntax(),
                "{source}"
            );
        }
        assert!(r#"{"jsonrpc":"2.0","result":"#.parse::<Message>().unwrap_err().is_eof());
        let source = r#"{"jsonrpc":"2.0","result":18446744073709551615,"id":1}"#;
        let message: Message = source.parse().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&message.to_json_bytes().unwrap()).unwrap(),
            json!({"jsonrpc":"2.0","result":u64::MAX,"id":1})
        );
    }

    #[test]
    fn preserves_envelope_recursion_boundary() {
        for field in ["result", "params", "unknown"] {
            let accepted = format!(
                r#"{{"jsonrpc":"2.0","id":1,"{field}":{}null{}}}"#,
                "[".repeat(126),
                "]".repeat(126)
            );
            let message: Message = accepted.parse().unwrap();
            if field == "result" {
                assert!(matches!(message, Message::Response(_)));
                assert!(accepted.parse::<Response>().is_ok());
            } else {
                assert!(matches!(message, Message::Request(_)));
            }

            let rejected = format!(
                r#"{{"jsonrpc":"2.0","id":1,"{field}":{}null{}}}"#,
                "[".repeat(127),
                "]".repeat(127)
            );
            assert!(
                rejected.parse::<Message>().unwrap_err().is_syntax(),
                "{field}"
            );
            assert!(rejected.parse::<Response>().is_err(), "{field}");
        }
    }
}
