//! Two-tier plugin system: native Rust plugins ([`native`]) and scripted
//! plugins ([`script`]). Defines the [`Plugin`] trait — the contract every
//! graph node implements — and [`create_plugin`], the single factory that
//! maps node-type strings from YAML config to plugin instances.

pub mod native;
pub mod resources;
pub mod script;
pub mod util;

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::{Context, GatewayError};
use resources::PluginResources;

/// The result of a successful plugin execution.
#[derive(Debug)]
pub struct PluginOutput {
    /// The (possibly mutated) context, passed on to the next node in the graph.
    pub context: Context,
    /// Values published under names that downstream nodes can consume as
    /// `named_inputs`; most plugins leave this empty.
    // Part of the plugin contract: every plugin populates it, but the engine
    // does not yet wire named inputs between nodes.
    #[allow(dead_code)]
    pub named_outputs: HashMap<String, serde_json::Value>,
}

/// The result of a plugin execution: either success or an error with the context preserved.
pub type PluginResult = Result<PluginOutput, PluginExecutionError>;

/// An error that occurs during plugin execution. The context is preserved so the
/// graph engine can route it through the error port.
#[derive(Debug)]
pub struct PluginExecutionError {
    /// The context as it stood when the error occurred; execution continues
    /// with it along the node's error edge.
    pub context: Context,
    /// The error details, appended to `Context.errors` by the graph engine.
    pub error: GatewayError,
}

/// Every plugin (native or scripted) implements this trait.
///
/// A plugin is a node in a compiled policy graph. The engine drives each node
/// through [`execute`](Plugin::execute) and follows the node's `success` or
/// `error` port depending on the result.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// Unique identifier for the plugin type (e.g., "proxy-rewrite", "upstream").
    fn plugin_type(&self) -> &str;

    /// Executes the plugin logic against the request/response context.
    ///
    /// Contract:
    /// - `ctx` is taken **by value**: the plugin owns the [`Context`] for the
    ///   duration of the call and must hand it back in either outcome — inside
    ///   [`PluginOutput`] on success, or inside [`PluginExecutionError`] on
    ///   failure. The context is never lost.
    /// - `named_inputs` carries values that upstream nodes published as
    ///   `named_outputs`, keyed by name; most plugins ignore it.
    /// - On `Ok`, the graph engine routes the returned context through the
    ///   node's `success` port. On `Err`, the [`PluginExecutionError`] carries
    ///   both the context and a [`GatewayError`], letting the engine record
    ///   the error and continue through the node's `error` port (typically
    ///   toward an `error-handler` node) instead of aborting the request.
    async fn execute(
        &self,
        ctx: Context,
        named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult;
}

/// Creates a plugin instance from a node type string and its YAML-derived config.
///
/// This is the single factory for all node types: the graph compiler calls it
/// for every node in a policy, so adding a new node type means adding exactly
/// one match arm here. `resources` hands plugins process-wide services
/// ([`PluginResources`]); constructors that need them take the handle in
/// `from_config`. Returns an error string for unknown node types or when
/// a plugin's `from_config` rejects its configuration (surfaced at config load,
/// not at request time).
pub fn create_plugin(
    node_type: &str,
    config: &HashMap<String, serde_json::Value>,
    resources: &Arc<PluginResources>,
) -> Result<Box<dyn Plugin>, String> {
    match node_type {
        "proxy-rewrite" => Ok(Box::new(
            native::proxy_rewrite::ProxyRewritePlugin::from_config(config)?,
        )),
        "upstream" => Ok(Box::new(native::upstream::UpstreamPlugin::from_config(
            config, resources,
        )?)),
        "aws-lambda" => Ok(Box::new(native::aws_lambda::AwsLambdaPlugin::from_config(
            config, resources,
        )?)),
        "azure-functions" => Ok(Box::new(
            native::azure_functions::AzureFunctionsPlugin::from_config(config, resources)?,
        )),
        "openwhisk" => Ok(Box::new(native::openwhisk::OpenWhiskPlugin::from_config(
            config, resources,
        )?)),
        "openfunction" => Ok(Box::new(
            native::openfunction::OpenFunctionPlugin::from_config(config, resources)?,
        )),
        "error-handler" => Ok(Box::new(
            native::error_handler::ErrorHandlerPlugin::from_config(config)?,
        )),
        "listener" => Ok(Box::new(native::listener::ListenerPlugin)),
        "client" => Ok(Box::new(native::client::ClientPlugin)),
        "cors" => Ok(Box::new(native::cors::CorsPlugin::from_config(config)?)),
        "rate-limit" => Ok(Box::new(native::rate_limit::RateLimitPlugin::from_config(
            config,
        )?)),
        "limit-conn" => Ok(Box::new(native::limit_conn::LimitConnPlugin::from_config(
            config, resources,
        )?)),
        "api-breaker" => Ok(Box::new(
            native::api_breaker::ApiBreakerPlugin::from_config(config, resources)?,
        )),
        "proxy-cache" => Ok(Box::new(
            native::proxy_cache::ProxyCachePlugin::from_config(config, resources)?,
        )),
        "limit-count" => Ok(Box::new(
            native::limit_count::LimitCountPlugin::from_config(config, resources)?,
        )),
        "proxy-mirror" => Ok(Box::new(
            native::proxy_mirror::ProxyMirrorPlugin::from_config(config, resources)?,
        )),
        "ip-restriction" => Ok(Box::new(
            native::ip_restriction::IpRestrictionPlugin::from_config(config)?,
        )),
        "consumer-restriction" => Ok(Box::new(
            native::consumer_restriction::ConsumerRestrictionPlugin::from_config(config)?,
        )),
        "acl" => Ok(Box::new(native::acl::AclPlugin::from_config(config)?)),
        "attach-consumer-label" => Ok(Box::new(
            native::attach_consumer_label::AttachConsumerLabelPlugin::from_config(config)?,
        )),
        "ua-restriction" => Ok(Box::new(
            native::ua_restriction::UaRestrictionPlugin::from_config(config)?,
        )),
        "referer-restriction" => Ok(Box::new(
            native::referer_restriction::RefererRestrictionPlugin::from_config(config)?,
        )),
        "uri-blocker" => Ok(Box::new(
            native::uri_blocker::UriBlockerPlugin::from_config(config)?,
        )),
        "csrf" => Ok(Box::new(native::csrf::CsrfPlugin::from_config(config)?)),
        "request-size-limit" => Ok(Box::new(
            native::request_size_limit::RequestSizeLimitPlugin::from_config(config)?,
        )),
        "key-auth" => Ok(Box::new(native::key_auth::KeyAuthPlugin::from_config(
            config, resources,
        )?)),
        "basic-auth" => Ok(Box::new(native::basic_auth::BasicAuthPlugin::from_config(
            config, resources,
        )?)),
        "jwt-auth" => Ok(Box::new(native::jwt_auth::JwtAuthPlugin::from_config(
            config, resources,
        )?)),
        "hmac-auth" => Ok(Box::new(native::hmac_auth::HmacAuthPlugin::from_config(
            config, resources,
        )?)),
        "jwe-decrypt" => Ok(Box::new(
            native::jwe_decrypt::JweDecryptPlugin::from_config(config, resources)?,
        )),
        "multi-auth" => Ok(Box::new(native::multi_auth::MultiAuthPlugin::from_config(
            config, resources,
        )?)),
        "forward-auth" => Ok(Box::new(
            native::forward_auth::ForwardAuthPlugin::from_config(config, resources)?,
        )),
        "opa" => Ok(Box::new(native::opa::OpaPlugin::from_config(
            config, resources,
        )?)),
        "opentelemetry" => Ok(Box::new(
            native::opentelemetry::OpenTelemetryPlugin::from_config(config, resources)?,
        )),
        "zipkin" => Ok(Box::new(native::zipkin::ZipkinPlugin::from_config(
            config, resources,
        )?)),
        "skywalking" => Ok(Box::new(native::skywalking::SkywalkingPlugin::from_config(
            config, resources,
        )?)),
        "prometheus" => Ok(Box::new(native::prometheus::PrometheusPlugin::from_config(
            config, resources,
        )?)),
        "ldap-auth" => Ok(Box::new(native::ldap_auth::LdapAuthPlugin::from_config(
            config, resources,
        )?)),
        "wolf-rbac" => Ok(Box::new(native::wolf_rbac::WolfRbacPlugin::from_config(
            config, resources,
        )?)),
        "cas-auth" => Ok(Box::new(native::cas_auth::CasAuthPlugin::from_config(
            config, resources,
        )?)),
        "authz-casbin" => Ok(Box::new(
            native::authz_casbin::AuthzCasbinPlugin::from_config(config, resources)?,
        )),
        "authz-keycloak" => Ok(Box::new(
            native::authz_keycloak::AuthzKeycloakPlugin::from_config(config, resources)?,
        )),
        "authz-casdoor" => Ok(Box::new(
            native::authz_casdoor::AuthzCasdoorPlugin::from_config(config, resources)?,
        )),
        "openid-connect" => Ok(Box::new(
            native::openid_connect::OpenidConnectPlugin::from_config(config, resources)?,
        )),
        "dingtalk-auth" => Ok(Box::new(
            native::dingtalk_auth::DingtalkAuthPlugin::from_config(config, resources)?,
        )),
        "feishu-auth" => Ok(Box::new(
            native::feishu_auth::FeishuAuthPlugin::from_config(config, resources)?,
        )),
        "logging" => Ok(Box::new(native::logging::LoggingPlugin::from_config(
            config,
        )?)),
        "http-logger" => Ok(Box::new(
            native::http_logger::HttpLoggerPlugin::from_config(config, resources)?,
        )),
        "loki-logger" => Ok(Box::new(
            native::loki_logger::LokiLoggerPlugin::from_config(config, resources)?,
        )),
        "splunk-hec-logging" => Ok(Box::new(
            native::splunk_hec_logging::SplunkHecLoggingPlugin::from_config(config, resources)?,
        )),
        "datadog" => Ok(Box::new(native::datadog::DatadogPlugin::from_config(
            config, resources,
        )?)),
        "loggly" => Ok(Box::new(native::loggly::LogglyPlugin::from_config(
            config, resources,
        )?)),
        "tcp-logger" => Ok(Box::new(native::tcp_logger::TcpLoggerPlugin::from_config(
            config,
        )?)),
        "udp-logger" => Ok(Box::new(native::udp_logger::UdpLoggerPlugin::from_config(
            config,
        )?)),
        "syslog" => Ok(Box::new(native::syslog::SyslogPlugin::from_config(config)?)),
        "file-logger" => Ok(Box::new(
            native::file_logger::FileLoggerPlugin::from_config(config)?,
        )),
        "error-log-logger" => Ok(Box::new(
            native::error_log_logger::ErrorLogLoggerPlugin::from_config(config)?,
        )),
        "google-cloud-logging" => Ok(Box::new(
            native::google_cloud_logging::GoogleCloudLoggingPlugin::from_config(config, resources)?,
        )),
        "skywalking-logger" => Ok(Box::new(
            native::skywalking_logger::SkywalkingLoggerPlugin::from_config(config, resources)?,
        )),
        "elasticsearch-logger" => Ok(Box::new(
            native::elasticsearch_logger::ElasticsearchLoggerPlugin::from_config(
                config, resources,
            )?,
        )),
        "clickhouse-logger" => Ok(Box::new(
            native::clickhouse_logger::ClickhouseLoggerPlugin::from_config(config, resources)?,
        )),
        "sls-logger" => Ok(Box::new(native::sls_logger::SlsLoggerPlugin::from_config(
            config, resources,
        )?)),
        "tencent-cloud-cls" => Ok(Box::new(
            native::tencent_cloud_cls::TencentCloudClsPlugin::from_config(config, resources)?,
        )),
        "lago" => Ok(Box::new(native::lago::LagoPlugin::from_config(
            config, resources,
        )?)),
        "request-id" => Ok(Box::new(native::request_id::RequestIdPlugin::from_config(
            config,
        )?)),
        "real-ip" => Ok(Box::new(native::real_ip::RealIpPlugin::from_config(
            config,
        )?)),
        "redirect" => Ok(Box::new(native::redirect::RedirectPlugin::from_config(
            config,
        )?)),
        "echo" => Ok(Box::new(native::echo::EchoPlugin::from_config(config)?)),
        "fault-injection" => Ok(Box::new(
            native::fault_injection::FaultInjectionPlugin::from_config(config)?,
        )),
        "workflow" => Ok(Box::new(native::workflow::WorkflowPlugin::from_config(
            config, resources,
        )?)),
        "traffic-label" => Ok(Box::new(
            native::traffic_label::TrafficLabelPlugin::from_config(config)?,
        )),
        "traffic-split" => Ok(Box::new(
            native::traffic_split::TrafficSplitPlugin::from_config(config, resources)?,
        )),
        "mocking" => Ok(Box::new(native::mocking::MockingPlugin::from_config(
            config,
        )?)),
        "response-rewrite" => Ok(Box::new(
            native::response_rewrite::ResponseRewritePlugin::from_config(config)?,
        )),
        "gzip" => Ok(Box::new(native::gzip::GzipPlugin::from_config(config)?)),
        "brotli" => Ok(Box::new(native::brotli::BrotliPlugin::from_config(config)?)),
        "error-page" => Ok(Box::new(native::error_page::ErrorPagePlugin::from_config(
            config,
        )?)),
        "exit-transformer" => Ok(Box::new(
            native::exit_transformer::ExitTransformerPlugin::from_config(config)?,
        )),
        "data-mask" => Ok(Box::new(native::data_mask::DataMaskPlugin::from_config(
            config,
        )?)),
        "request-validation" => Ok(Box::new(
            native::request_validation::RequestValidationPlugin::from_config(config)?,
        )),
        "body-transformer" => Ok(Box::new(
            native::body_transformer::BodyTransformerPlugin::from_config(config)?,
        )),
        "degraphql" => Ok(Box::new(native::degraphql::DegraphqlPlugin::from_config(
            config,
        )?)),
        "oas-validator" => Ok(Box::new(
            native::oas_validator::OasValidatorPlugin::from_config(config)?,
        )),
        "serverless-pre-function" => Ok(Box::new(
            native::serverless_pre_function::ServerlessPreFunctionPlugin::from_config(config)?,
        )),
        "serverless-post-function" => Ok(Box::new(
            native::serverless_post_function::ServerlessPostFunctionPlugin::from_config(config)?,
        )),
        "script" => Ok(Box::new(script::ScriptPlugin::from_config(config)?)),
        _ => Err(format!("Unknown plugin type: {}", node_type)),
    }
}
