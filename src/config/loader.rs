//! YAML loading and shell-style `${ENV_VAR:-default}` interpolation.
//!
//! Two loading modes: [`load_yaml_with_env`] interpolates the raw file text
//! before parsing (used for `system.yaml`, which is never served back to
//! clients), while [`load_yaml`] preserves placeholders verbatim (used for
//! `gateway.yaml`, whose contents the Admin API serves to the Web UI —
//! resolved secrets must never appear there). Placeholders in gateway config
//! are resolved at the point of consumption instead: plugin node config at
//! graph-compile time via [`interpolate_env_json`], route match rules and
//! consumer credentials when the route table / consumer store are built.

use regex::Regex;
use serde::de::DeserializeOwned;
use std::env;
use std::fs;
use std::path::Path;

/// Replaces `${VAR}` and `${VAR:-default}` patterns with environment variable values.
///
/// Semantics:
/// - `${VAR}` — substituted with the variable's value, or the empty string if unset.
/// - `${VAR:-default}` — substituted with the variable's value, or `default` if unset.
///
/// Variable names must match `[A-Za-z_][A-Za-z0-9_]*`; text that does not
/// match the pattern is left untouched. There is no escape syntax for a
/// literal `${...}`.
pub fn interpolate_env(input: &str) -> String {
    let re = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-((?:[^}\\]|\\.)*)?)?\}").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        let var_name = &caps[1];
        let default_value = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        env::var(var_name).unwrap_or_else(|_| default_value.to_string())
    })
    .to_string()
}

/// Recursively interpolates `${ENV_VAR:-default}` patterns in every string leaf
/// of a JSON value — object values, array elements, and nested combinations —
/// leaving numbers, booleans, and null untouched.
///
/// This is the single resolution point for structured plugin config from
/// every source — `gateway.yaml` (loaded raw by [`load_yaml`]), the Admin
/// API / Web UI, and etcd. Applied at graph-compile time it is
/// source-agnostic: a `client_id: ${CLIENT_ID}` resolves identically however
/// it was authored, and the stored config keeps the placeholder form (so the
/// Admin API never serves resolved secrets).
///
/// A string that is exactly one `${...}` placeholder is **typed like the
/// YAML scalar it stands in for**: a resolved value of `true`/`false`
/// becomes a boolean and a value parsing as a number becomes a number
/// (`port: ${PORT:-3010}` yields `3010`, not `"3010"`); anything else,
/// including the empty string, stays a string. A placeholder embedded in
/// wider text always resolves to a string.
pub fn interpolate_env_json(value: &mut serde_json::Value) {
    if let Some(resolved) = interpolated_replacement(value) {
        *value = resolved;
        return;
    }
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                interpolate_env_json(item);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                interpolate_env_json(v);
            }
        }
        _ => {}
    }
}

/// The replacement for a string leaf containing `${...}`, `None` for
/// everything else (the `${` fast-path guard included).
fn interpolated_replacement(value: &serde_json::Value) -> Option<serde_json::Value> {
    let s = value.as_str()?;
    if !s.contains("${") {
        return None;
    }
    let whole = Regex::new(r"^\$\{[A-Za-z_][A-Za-z0-9_]*(?::-((?:[^}\\]|\\.)*)?)?\}$")
        .unwrap()
        .is_match(s);
    let resolved = interpolate_env(s);
    Some(if whole {
        coerce_scalar(&resolved).unwrap_or(serde_json::Value::String(resolved))
    } else {
        serde_json::Value::String(resolved)
    })
}

/// Types a resolved full-placeholder value the way YAML types the same
/// unquoted scalar: booleans and finite numbers; everything else `None`.
fn coerce_scalar(s: &str) -> Option<serde_json::Value> {
    match s {
        "true" => Some(serde_json::Value::Bool(true)),
        "false" => Some(serde_json::Value::Bool(false)),
        _ => {
            if let Ok(i) = s.parse::<i64>() {
                return Some(serde_json::Value::Number(i.into()));
            }
            if let Ok(f) = s.parse::<f64>() {
                if f.is_finite() {
                    return serde_json::Number::from_f64(f).map(serde_json::Value::Number);
                }
            }
            None
        }
    }
}

/// Loads a YAML file, interpolates environment variables, and deserializes into `T`.
///
/// Interpolation runs on the raw text *before* YAML parsing, so `${VAR}`
/// works anywhere in the file — keys, values, and free-form plugin config
/// alike. Returns an error if the file is unreadable or the interpolated
/// text does not deserialize into `T`.
pub fn load_yaml_with_env<T: DeserializeOwned>(
    path: &Path,
) -> Result<T, Box<dyn std::error::Error>> {
    let raw = fs::read_to_string(path)?;
    let interpolated = interpolate_env(&raw);
    let config: T = serde_yaml::from_str(&interpolated)?;
    Ok(config)
}

/// Loads a YAML file and deserializes into `T` **without** resolving
/// `${ENV_VAR}` placeholders — they are preserved verbatim in the parsed
/// structure.
///
/// Used for `gateway.yaml`: its contents are served back to the Web UI by
/// the Admin API, so resolving placeholders at load time would leak secret
/// values into API responses and config exports. Resolution happens at the
/// point of consumption instead (see [`interpolate_env_json`]).
pub fn load_yaml<T: DeserializeOwned>(path: &Path) -> Result<T, Box<dyn std::error::Error>> {
    let raw = fs::read_to_string(path)?;
    let config: T = serde_yaml::from_str(&raw)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interpolation_with_env_var() {
        env::set_var("TEST_GW_VAR", "hello");
        let result = interpolate_env("value: ${TEST_GW_VAR}");
        assert_eq!(result, "value: hello");
        env::remove_var("TEST_GW_VAR");
    }

    #[test]
    fn test_interpolation_with_default() {
        env::remove_var("NONEXISTENT_VAR_XYZ");
        let result = interpolate_env("value: ${NONEXISTENT_VAR_XYZ:-fallback}");
        assert_eq!(result, "value: fallback");
    }

    #[test]
    fn test_interpolation_missing_no_default() {
        env::remove_var("MISSING_VAR_ABC");
        let result = interpolate_env("value: ${MISSING_VAR_ABC}");
        assert_eq!(result, "value: ");
    }

    #[test]
    fn test_interpolation_multiple() {
        env::set_var("GW_HOST", "0.0.0.0");
        env::set_var("GW_PORT", "8080");
        let result = interpolate_env("bind: ${GW_HOST}:${GW_PORT}");
        assert_eq!(result, "bind: 0.0.0.0:8080");
        env::remove_var("GW_HOST");
        env::remove_var("GW_PORT");
    }

    #[test]
    fn test_interpolate_json_resolves_string_leaves() {
        // Mirrors a plugin node config authored through the Web UI: `${VAR}`
        // arrives as a parsed JSON string, not raw YAML text.
        env::set_var("TEST_CLIENT_ID", "featherbit-app");
        let mut value = serde_json::json!({
            "client_id": "${TEST_CLIENT_ID}",
            "bearer_only": false,
            "scopes": ["openid", "${TEST_CLIENT_ID}"],
            "session": { "secret": "${TEST_CLIENT_ID}:${MISSING_JSON_VAR:-fallback}" }
        });
        interpolate_env_json(&mut value);
        assert_eq!(value["client_id"], serde_json::json!("featherbit-app"));
        // Non-string leaves are untouched.
        assert_eq!(value["bearer_only"], serde_json::json!(false));
        // Arrays and nested objects are interpolated recursively.
        assert_eq!(value["scopes"][1], serde_json::json!("featherbit-app"));
        assert_eq!(
            value["session"]["secret"],
            serde_json::json!("featherbit-app:fallback")
        );
        env::remove_var("TEST_CLIENT_ID");
    }

    #[test]
    fn test_interpolate_json_coerces_full_placeholder_scalars() {
        // A gateway.yaml author writes `port: ${PORT:-3010}` unquoted; loaded
        // raw that is a JSON string. The resolved value must come back typed
        // the way the old file-text interpolation produced it: numbers and
        // booleans become numbers and booleans when the string is exactly one
        // `${...}` placeholder.
        env::set_var("TEST_COERCE_PORT", "3010");
        env::set_var("TEST_COERCE_FLAG", "true");
        let mut value = serde_json::json!({
            "port": "${TEST_COERCE_PORT}",
            "flag": "${TEST_COERCE_FLAG}",
            "ratio": "${MISSING_COERCE_RATIO:-0.25}",
            "name": "${MISSING_COERCE_NAME:-plain}",
            "mixed": "${TEST_COERCE_PORT}:${TEST_COERCE_PORT}",
            "unset": "${MISSING_COERCE_UNSET}"
        });
        interpolate_env_json(&mut value);
        assert_eq!(value["port"], serde_json::json!(3010));
        assert_eq!(value["flag"], serde_json::json!(true));
        assert_eq!(value["ratio"], serde_json::json!(0.25));
        // Non-scalar-looking values stay strings.
        assert_eq!(value["name"], serde_json::json!("plain"));
        // A placeholder embedded in wider text always stays a string.
        assert_eq!(value["mixed"], serde_json::json!("3010:3010"));
        // Unset without default resolves to the empty string, no coercion.
        assert_eq!(value["unset"], serde_json::json!(""));
        env::remove_var("TEST_COERCE_PORT");
        env::remove_var("TEST_COERCE_FLAG");
    }

    #[test]
    fn test_interpolate_json_leaves_plain_strings_untouched() {
        // No `${...}` -> value passes through byte-for-byte (fast-path guard).
        let mut value = serde_json::json!({ "path": "/api/v1", "n": 42 });
        interpolate_env_json(&mut value);
        assert_eq!(value["path"], serde_json::json!("/api/v1"));
        assert_eq!(value["n"], serde_json::json!(42));
    }
}
