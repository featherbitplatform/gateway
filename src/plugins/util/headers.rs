//! Case-insensitive helpers for the context's header maps.
//!
//! Context headers are stored as a plain `HashMap<String, Vec<String>>`. The
//! keys are *usually* lowercase — hyper normalises them on the way in — but a
//! Lua script, another plugin, or a sandbox-seeded response can introduce a
//! mixed-case name. Since HTTP header names are case-insensitive (RFC 9110
//! §5.1), removals and lookups must be too, or they silently miss.

use std::collections::HashMap;

/// Removes every entry whose name matches `name` case-insensitively.
///
/// Returns `true` if anything was removed. A plain `map.remove(&name.to_lowercase())`
/// only works when the stored key is already lowercase; this handles any case.
pub fn remove_ci(map: &mut HashMap<String, Vec<String>>, name: &str) -> bool {
    let target = name.to_ascii_lowercase();
    let matches: Vec<String> = map
        .keys()
        .filter(|k| k.eq_ignore_ascii_case(&target))
        .cloned()
        .collect();
    let removed = !matches.is_empty();
    for k in matches {
        map.remove(&k);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), vec![v.to_string()]))
            .collect()
    }

    #[test]
    fn test_removes_regardless_of_stored_case() {
        let mut m = map(&[("X-Powered-By", "php"), ("content-type", "text/html")]);
        assert!(remove_ci(&mut m, "x-powered-by"));
        assert!(!m.contains_key("X-Powered-By"));
        assert!(m.contains_key("content-type"));
    }

    #[test]
    fn test_removes_regardless_of_query_case() {
        let mut m = map(&[("x-trace", "1")]);
        assert!(remove_ci(&mut m, "X-Trace"));
        assert!(m.is_empty());
    }

    #[test]
    fn test_reports_false_when_absent() {
        let mut m = map(&[("a", "1")]);
        assert!(!remove_ci(&mut m, "b"));
        assert_eq!(m.len(), 1);
    }
}
