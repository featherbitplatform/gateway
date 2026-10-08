//! The public surface the gateway-operator links against. If this file stops
//! compiling, the operator breaks: extend it, never trim it.

use featherbit::config::{
    GatewayConfig, PluginConfigDef, PolicyConfig, StoreConfig, SupernodeConfig,
};
use featherbit::consumers::{ConsumerConfig, ConsumerStore};
use featherbit::graph::{prepare_policy, validate_policy, validate_supernode};
use featherbit::plugins::port_spec;
use featherbit::routing::validate_match_rule;
use featherbit::state::validate_gateway_config;
use featherbit::stores::validate_stores;

const MINIMAL: &str = r#"
routes:
  - name: hello
    match: { path: /hello }
    policy: hello
policies:
  - name: hello
    nodes:
      - { id: listener, type: listener }
      - { id: mock, type: mocking, config: { response_example: "{}" } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: mock.in }
      - { from: mock.success, to: client.in }
"#;

#[test]
fn whole_config_compile_accepts_minimal_config() {
    let gw: GatewayConfig = serde_yaml::from_str(MINIMAL).unwrap();
    validate_gateway_config(&gw).expect("minimal config compiles");
}

#[test]
fn whole_config_compile_rejects_route_to_missing_policy() {
    let gw: GatewayConfig =
        serde_yaml::from_str(&MINIMAL.replace("policy: hello", "policy: nope")).unwrap();
    let err = validate_gateway_config(&gw).unwrap_err();
    assert!(
        err.contains("nope"),
        "error names the missing policy: {err}"
    );
}

#[test]
fn policy_validator_reports_duplicate_node_ids() {
    let policy: PolicyConfig = serde_yaml::from_str(
        r#"
name: dup
nodes:
  - { id: listener, type: listener }
  - { id: listener, type: listener }
  - { id: client, type: client }
edges:
  - { from: listener.out, to: client.in }
"#,
    )
    .unwrap();
    let errors = validate_policy(&policy).unwrap_err();
    assert!(
        errors.iter().any(|e| e == "Duplicate node id 'listener'"),
        "{errors:?}"
    );
}

#[test]
fn match_rule_validator_rejects_bad_host_pattern() {
    let gw: GatewayConfig =
        serde_yaml::from_str(&MINIMAL.replace("{ path: /hello }", "{ path: /hello, host: '*' }"))
            .unwrap();
    assert!(validate_match_rule(&gw.routes[0].match_rule).is_err());
}

#[test]
fn port_spec_is_the_node_type_catalog() {
    assert!(port_spec("upstream").is_some());
    assert!(port_spec("listener").is_some());
    assert!(port_spec("no-such-node-type").is_none());
}

/// `reconcile_prefix` is the operator's etcd sink; it must stay callable
/// without a `SharedState`. An unreachable endpoint is the cheapest way to
/// prove the signature from outside the crate.
#[tokio::test]
async fn reconcile_prefix_is_callable_without_runtime_state() {
    let cfg: featherbit::config::EtcdConfig =
        serde_yaml::from_str("endpoints: ['http://127.0.0.1:1']\ntimeout_ms: 100\n").unwrap();
    let gw: GatewayConfig = serde_yaml::from_str(MINIMAL).unwrap();
    let err = featherbit::config_store::etcd::reconcile_prefix(&cfg, &gw)
        .await
        .unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn supernode_validator_signature_and_behavior() {
    let f: fn(&SupernodeConfig) -> Result<(), Vec<String>> = validate_supernode;
    let sn: SupernodeConfig = serde_yaml::from_str(
        "{ name: s1, nodes: [{ id: input, type: input }, { id: output, type: output }, { id: error, type: error }], edges: [{ from: input.out, to: output.in }] }",
    )
    .unwrap();
    assert!(f(&sn).is_ok());
}

#[test]
fn prepare_policy_signature_and_behavior() {
    type PrepareFn =
        fn(PolicyConfig, &[SupernodeConfig], &[PluginConfigDef]) -> Result<PolicyConfig, String>;
    let f: PrepareFn = prepare_policy;
    let gw: GatewayConfig = serde_yaml::from_str(MINIMAL).unwrap();
    let prepared = f(gw.policies[0].clone(), &[], &[]).expect("minimal policy prepares");
    assert_eq!(prepared.name, "hello");
}

#[test]
fn store_validator_signature_and_behavior() {
    let f: fn(&[StoreConfig]) -> Result<(), String> = validate_stores;
    let ok: StoreConfig =
        serde_yaml::from_str("{ name: st1, type: redis, url: 'redis://r:6379' }").unwrap();
    assert!(f(std::slice::from_ref(&ok)).is_ok());
    assert!(f(&[ok.clone(), ok]).is_err(), "duplicate names rejected");
}

#[test]
fn consumer_store_builds_from_config() {
    let f: fn(&[ConsumerConfig]) -> Result<ConsumerStore, String> = ConsumerStore::from_config;
    let c: ConsumerConfig =
        serde_yaml::from_str("{ name: c1, credentials: { key-auth: { key: k } } }").unwrap();
    assert!(f(&[c]).is_ok());
}

/// `JsonSchema` must be derivable-from-outside: the operator generates its
/// CRD schemas from these types.
#[test]
fn config_types_expose_json_schema() {
    let policy = serde_json::to_value(schemars::schema_for!(PolicyConfig)).unwrap();
    assert!(policy["properties"]["nodes"].is_object(), "{policy}");
    let consumer = serde_json::to_value(schemars::schema_for!(ConsumerConfig)).unwrap();
    assert!(
        consumer["properties"]["credentials"].is_object(),
        "{consumer}"
    );
}
