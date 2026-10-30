//! Every featherbit config file shipped in the repo (the default config, the
//! runnable examples, the test and e2e fixtures, the screenshot fixtures)
//! must load through the real loaders. This is the regression guard for the
//! YAML parser: a parser change that alters how any of these files is read
//! fails here before it reaches a user.

use featherbit::config::{load_yaml, load_yaml_with_env, GatewayConfig, SystemConfig};
use std::path::PathBuf;

fn fixture_dirs() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut dirs = vec![
        root.join("config"),
        root.join("tests/config"),
        root.join("e2e/fixtures"),
        root.join("e2e/fixtures/acme"),
        root.join("website/screenshots"),
    ];
    let mut examples: Vec<PathBuf> = std::fs::read_dir(root.join("examples"))
        .expect("examples/ directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("config"))
        .filter(|path| path.is_dir())
        .collect();
    examples.sort();
    assert!(
        examples.len() >= 8,
        "expected the runnable examples to ship config dirs, found {}",
        examples.len()
    );
    dirs.extend(examples);
    dirs
}

/// Names of the `${VAR}` placeholders in `text` that carry no `:-default`,
/// ignoring YAML comments (which mention placeholders by way of example).
fn bare_placeholders(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| match line.find('#') {
            Some(i) if i == 0 || line[..i].ends_with(' ') => &line[..i],
            _ => line,
        })
        .flat_map(|line| {
            line.split("${")
                .skip(1)
                .filter_map(|rest| rest.split_once('}').map(|(inner, _)| inner))
                .filter(|inner| !inner.contains(":-"))
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn every_shipped_gateway_yaml_loads() {
    for dir in fixture_dirs() {
        let path = dir.join("gateway.yaml");
        let _: GatewayConfig =
            load_yaml(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}

#[test]
fn every_shipped_system_yaml_loads() {
    for dir in fixture_dirs() {
        let path = dir.join("system.yaml");
        // Some fixtures (the mcp example's tokens, the acme e2e fixture's
        // directory URL) take values from the environment with no default,
        // which their compose file or harness always provides. A bare
        // placeholder left unset expands to an empty value, which is not a
        // string, so stand in for that environment here.
        let text = std::fs::read_to_string(&path).unwrap();
        for var in bare_placeholders(&text) {
            if std::env::var_os(&var).is_none() {
                std::env::set_var(&var, "fixture");
            }
        }
    }
    for dir in fixture_dirs() {
        let path = dir.join("system.yaml");
        let _: SystemConfig =
            load_yaml_with_env(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}
