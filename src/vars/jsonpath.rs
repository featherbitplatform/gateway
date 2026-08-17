//! JSONPath subjects for condition expressions: `$.user.name` (request
//! body), `request_body:$...`, `response_body:$...`. Paths are compiled at
//! config load (RFC 9535 via serde_json_path); malformed paths fail policy
//! compilation, not requests.

use serde_json_path::JsonPath;

/// Which body a JSONPath subject queries.
pub enum BodyTarget {
    Request,
    Response,
}

/// A compiled JSONPath subject.
pub struct JsonSubject {
    pub target: BodyTarget,
    pub path: JsonPath,
    /// The subject string as written in config, for error messages.
    /// Not yet read within this crate (kept for downstream diagnostics /
    /// future condition-engine work); silence dead_code accordingly.
    #[allow(dead_code)]
    pub raw: String,
}

/// Recognizes and compiles a JSONPath subject. `None` means the subject is
/// a plain var name; `Some(Err)` means it looked like a JSONPath subject
/// but the path is malformed.
pub fn parse_json_subject(subject: &str) -> Option<Result<JsonSubject, String>> {
    let (target, path_str) = if let Some(p) = subject.strip_prefix("request_body:") {
        (BodyTarget::Request, p)
    } else if let Some(p) = subject.strip_prefix("response_body:") {
        (BodyTarget::Response, p)
    } else if subject.starts_with('$') {
        (BodyTarget::Request, subject)
    } else {
        return None;
    };
    Some(
        JsonPath::parse(path_str)
            .map(|path| JsonSubject {
                target,
                path,
                raw: subject.to_string(),
            })
            .map_err(|e| format!("invalid JSONPath '{}': {}", subject, e)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonpath_subject_detection() {
        assert!(parse_json_subject("http_authorization").is_none());
        assert!(parse_json_subject("arg_name").is_none());
        assert!(matches!(
            parse_json_subject("$.user.name"),
            Some(Ok(JsonSubject {
                target: BodyTarget::Request,
                ..
            }))
        ));
        assert!(matches!(
            parse_json_subject("request_body:$.a"),
            Some(Ok(JsonSubject {
                target: BodyTarget::Request,
                ..
            }))
        ));
        assert!(matches!(
            parse_json_subject("response_body:$.a[*].b"),
            Some(Ok(JsonSubject {
                target: BodyTarget::Response,
                ..
            }))
        ));
        // looked like JSONPath, malformed path -> hard error
        assert!(matches!(parse_json_subject("$.["), Some(Err(_))));
        assert!(matches!(
            parse_json_subject("response_body:nope"),
            Some(Err(_))
        ));
    }
}
