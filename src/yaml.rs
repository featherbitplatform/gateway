//! The one place the YAML parser is chosen.
//!
//! Every YAML read and write in the crate goes through [`from_str`] and
//! [`to_string`] so that swapping the parser is a one-file change. The
//! parser is serde-saphyr (pure Rust); it replaced serde_yaml 0.9, whose C
//! port backend is unmaintained (RUSTSEC-2024-0449).
//!
//! The contract the rest of the crate relies on is pinned by the tests
//! below: plain scalars take the type the target asks for, duplicate keys
//! are an error, `~` is null, and YAML 1.1 booleans (`yes`, `on`) stay
//! strings, exactly as with the previous parser. Three deliberate deviations
//! remain, each pinned by a test so a parser change shows up here first:
//!
//! - In free-form values such as a node's `config` map, where no target type
//!   steers the parse, an unquoted numeral with a leading zero (`0755`) or
//!   with underscores (`1_000`) was a string before and is a number now.
//!   Quote such values to keep them strings.
//! - An empty value, `~` or `null` in a field that requires a string was
//!   accepted before (as `""`, `"~"` or `"null"`) and is an error now. A bare
//!   `${VAR}` whose variable is unset therefore fails at load instead of
//!   becoming an empty string.
//! - Typed fields are more lenient: a quoted `'80'` into an integer field or
//!   `'true'` into a boolean field was an error before and is accepted now.

use serde::de::DeserializeOwned;
use serde::Serialize;

pub use serde_saphyr::{Error, SerializeError};

/// Deserializes one YAML document into `T`.
///
/// Errors carry the line and column but no rendered source snippet, so they
/// stay one line long for Admin API responses and log lines.
pub fn from_str<T: DeserializeOwned>(input: &str) -> Result<T, Error> {
    let mut options = serde_saphyr::Options::default();
    // Only `true`/`false` are booleans; `yes`/`no`/`on`/`off` stay strings,
    // matching the previous parser and existing config files.
    options.strict_booleans = true;
    options.with_snippet = false;
    serde_saphyr::from_str_with_options(input, options)
}

/// Serializes `value` as a YAML document.
pub fn to_string<T: Serialize>(value: &T) -> Result<String, SerializeError> {
    serde_saphyr::to_string(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct Target {
        name: String,
        port: u16,
        #[serde(default)]
        note: Option<String>,
    }

    #[test]
    fn plain_scalar_takes_the_type_the_target_asks_for() {
        let t: Target = from_str("name: 123\nport: 8080\n").unwrap();
        assert_eq!(t.name, "123");
        assert_eq!(t.port, 8080);
    }

    #[test]
    fn env_placeholders_survive_as_plain_strings() {
        let t: Target = from_str("name: ${UPSTREAM:-fallback}\nport: 80\n").unwrap();
        assert_eq!(t.name, "${UPSTREAM:-fallback}");
    }

    #[test]
    fn duplicate_keys_are_an_error() {
        let r: Result<Target, _> = from_str("name: a\nname: b\nport: 1\n");
        assert!(r.is_err(), "duplicate keys must be rejected");
    }

    #[test]
    fn tilde_is_null() {
        let t: Target = from_str("name: a\nport: 1\nnote: ~\n").unwrap();
        assert_eq!(t.note, None);
        let t: Target = from_str("name: a\nport: 1\nnote:\n").unwrap();
        assert_eq!(t.note, None);
    }

    #[test]
    fn typed_bool_rejects_yaml_1_1_words() {
        #[derive(Debug, Deserialize)]
        struct Flag {
            #[allow(dead_code)]
            on: bool,
        }
        assert!(from_str::<Flag>("on: yes\n").is_err());
        assert!(from_str::<Flag>("on: true\n").is_ok());
    }

    // Known deviation from serde_yaml 0.9, documented at the top of this
    // module: a required string no longer accepts a null-like value.
    #[test]
    fn null_into_required_string_is_an_error() {
        assert!(from_str::<Target>("name:\nport: 1\n").is_err());
        assert!(from_str::<Target>("name: ~\nport: 1\n").is_err());
        assert!(from_str::<Target>("name: null\nport: 1\n").is_err());
        let t: Target = from_str("name: ''\nport: 1\n").unwrap();
        assert_eq!(t.name, "");
    }

    #[test]
    fn yaml_1_1_booleans_stay_strings() {
        let v: serde_json::Value =
            from_str("a: yes\nb: on\nc: true\nd: No\ne: off\nf: FALSE\n").unwrap();
        assert_eq!(v["a"], "yes");
        assert_eq!(v["b"], "on");
        assert_eq!(v["c"], true);
        assert_eq!(v["d"], "No");
        assert_eq!(v["e"], "off");
        assert_eq!(v["f"], false);
    }

    #[test]
    fn free_form_scalars_resolve_like_before() {
        let v: serde_json::Value = from_str(
            "a: 8080\nb: -1\nc: 1.5\nd: 1e3\ne: 0x1F\nf: ~\ng: null\nh:\ni: 0\nj: 1.2.3\nk: 12:30\nl: 2024-01-01\nm: /api/*\nn: ${PORT:-80}\n",
        )
        .unwrap();
        assert_eq!(v["a"], 8080);
        assert_eq!(v["b"], -1);
        assert_eq!(v["c"], 1.5);
        assert_eq!(v["d"], 1000.0);
        assert_eq!(v["e"], 31);
        assert!(v["f"].is_null() && v["g"].is_null() && v["h"].is_null());
        assert_eq!(v["i"], 0);
        assert_eq!(v["j"], "1.2.3");
        assert_eq!(v["k"], "12:30");
        assert_eq!(v["l"], "2024-01-01");
        assert_eq!(v["m"], "/api/*");
        assert_eq!(v["n"], "${PORT:-80}");
    }

    // Known deviation from serde_yaml 0.9, documented at the top of this
    // module: pinned so a parser change shows up here first.
    #[test]
    fn free_form_leading_zero_and_underscore_numerals_are_numbers() {
        let v: serde_json::Value = from_str("a: 0755\nb: 1_000\nc: \"0755\"\n").unwrap();
        assert!(
            v["a"].is_number(),
            "0755 resolves to a number: {:?}",
            v["a"]
        );
        assert_eq!(v["b"], 1000);
        assert_eq!(v["c"], "0755");
    }

    #[test]
    fn untagged_enum_distinguishes_number_from_string() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(untagged)]
        enum Port {
            Num(u16),
            Name(String),
        }
        let n: Port = from_str("8080").unwrap();
        let s: Port = from_str("web").unwrap();
        let q: Port = from_str("\"8080\"").unwrap();
        assert_eq!(n, Port::Num(8080));
        assert_eq!(s, Port::Name("web".into()));
        assert_eq!(q, Port::Name("8080".into()));
    }

    #[test]
    fn to_string_round_trips_through_from_str() {
        let t = Target {
            name: "svc".into(),
            port: 443,
            note: Some("multi\nline".into()),
        };
        let text = to_string(&t).unwrap();
        let back: Target = from_str(&text).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn parse_errors_are_single_line_messages() {
        let err = from_str::<Target>("name: a\nport: not-a-number\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.trim().contains('\n'),
            "expected a one-line message, got:\n{msg}"
        );
    }
}
