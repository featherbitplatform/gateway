//! YAML loading with shell-style environment variable interpolation, applied
//! to the raw file text before deserialization so every config value —
//! including nested plugin config — supports `${ENV_VAR:-default}`.

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
/// This brings the same `${ENV_VAR}` substitution that [`load_yaml_with_env`]
/// applies to raw YAML *text* to config that arrives as already-parsed
/// structured data — i.e. plugin node config authored through the Admin API /
/// Web UI or delivered over etcd, which never passes through the file-text
/// interpolation path. Applied at graph-compile time it is source-agnostic: a
/// `client_id: ${CLIENT_ID}` set in the UI resolves exactly as it would in
/// `gateway.yaml`. File-loaded values were already interpolated before parsing,
/// so on them this is a no-op (the `${` fast-path guard skips them).
pub fn interpolate_env_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if s.contains("${") {
                *s = interpolate_env(s);
            }
        }
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
    fn test_interpolate_json_leaves_plain_strings_untouched() {
        // No `${...}` -> value passes through byte-for-byte (fast-path guard).
        let mut value = serde_json::json!({ "path": "/api/v1", "n": 42 });
        interpolate_env_json(&mut value);
        assert_eq!(value["path"], serde_json::json!("/api/v1"));
        assert_eq!(value["n"], serde_json::json!(42));
    }
}
