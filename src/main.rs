use clap::{ArgAction, Parser, ValueEnum};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use vllm_router_rs::config::{
    CircuitBreakerConfig, ConfigError, ConfigResult, ConnectionMode, DiscoveryConfig,
    HealthCheckConfig, HistoryBackend, KvConnector, MetricsConfig, PolicyConfig, RetryConfig,
    RouterConfig, RoutingMode, SessionAffinityConfig, TraceConfig,
};
use vllm_router_rs::metrics::PrometheusConfig;
use vllm_router_rs::server::{self, ServerConfig};
use vllm_router_rs::service_discovery::ServiceDiscoveryConfig;

type PrefillUrls = Vec<(String, Option<u16>)>;

// Helper function to parse prefill arguments from command line
// Returns prefill_entries with (URL, optional_bootstrap_port)
fn parse_prefill_args_from(args: &[String]) -> PrefillUrls {
    let mut prefill_entries = Vec::new();
    let mut i = 0;

    while i < args.len() {
        if args[i] == "--prefill" && i + 1 < args.len() {
            let url = args[i + 1].clone();

            let bootstrap_port = if i + 2 < args.len() && !args[i + 2].starts_with("--") {
                // Check if next arg is a port number
                if let Ok(port) = args[i + 2].parse::<u16>() {
                    i += 1; // Skip the port argument
                    Some(port)
                } else if args[i + 2].to_lowercase() == "none" {
                    i += 1; // Skip the "none" argument
                    None
                } else {
                    None
                }
            } else {
                None
            };
            prefill_entries.push((url, bootstrap_port));
            i += 2; // Skip --prefill and URL
        } else {
            i += 1;
        }
    }

    prefill_entries
}

fn config_path_from_args(args: &[String]) -> Option<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--config" {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            return Some(value.to_string());
        }
    }
    None
}

fn raw_args_has_option(args: &[String], flag: &str) -> bool {
    args.iter()
        .any(|arg| arg == flag || arg.starts_with(&format!("{flag}=")))
}

/// Map canonical config keys (RouterArgs-style) to the native CLI long option.
fn config_key_flag(key: &str) -> String {
    let cli_name = match key {
        // Dataclass-style aliases accepted in config files.
        "decode_urls" => "decode",
        "prefill_urls" => "prefill",
        "eviction_interval_secs" => "eviction_interval",
        other => other,
    };
    format!("--{}", cli_name.replace('_', "-"))
}

fn config_value_to_string(value: &Value) -> std::io::Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "config values must be scalar or a list of scalars, got {}",
                value
            ),
        )),
    }
}

/// Convert a flat JSON config object into CLI-style tokens. Keys are
/// snake_case option names (e.g. ``worker_urls``); values mirror what would be
/// passed after the corresponding ``--option``. ``prefill`` accepts
/// ``[["url", "9000"], ["url2"]]`` entries.
fn config_to_cli_args(config_path: &str, cli_args: &[String]) -> std::io::Result<Vec<String>> {
    let text = fs::read_to_string(config_path).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("cannot read config file '{config_path}': {e}"),
        )
    })?;
    let root: Value = serde_json::from_str(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid JSON in config file '{config_path}': {e}"),
        )
    })?;
    let obj = root.as_object().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("config file '{config_path}' must contain a JSON object"),
        )
    })?;

    let mut tokens = Vec::new();
    for (key, value) in obj {
        if key == "config" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "'config' cannot be nested inside a config file",
            ));
        }
        let flag = config_key_flag(key);
        if key == "prefill" || key == "prefill_urls" {
            if raw_args_has_option(cli_args, "--prefill") {
                continue;
            }
            let entries = value.as_array().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "config option 'prefill' expects a list of [url, optional_port] entries",
                )
            })?;
            for entry in entries {
                tokens.push("--prefill".to_string());
                if let Some(parts) = entry.as_array() {
                    for part in parts {
                        tokens.push(config_value_to_string(part)?);
                    }
                } else {
                    tokens.push(config_value_to_string(entry)?);
                }
            }
            continue;
        }
        // Explicit CLI flags win over the config file. Skipping duplicates is
        // also required by clap, which rejects repeated scalar options.
        if raw_args_has_option(cli_args, &flag) {
            continue;
        }

        if let Value::Object(map) = value {
            if !key.ends_with("selector") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("config option '{key}' cannot be a JSON object"),
                ));
            }
            for (selector_key, selector_value) in map {
                tokens.push(flag.clone());
                tokens.push(format!(
                    "{}={}",
                    selector_key,
                    config_value_to_string(selector_value)?
                ));
            }
            continue;
        }

        match value {
            Value::Null => {}
            Value::Bool(true) => tokens.push(flag),
            Value::Bool(false) => {}
            Value::Array(items) => {
                for item in items {
                    tokens.push(flag.clone());
                    tokens.push(config_value_to_string(item)?);
                }
            }
            scalar => {
                tokens.push(flag);
                tokens.push(config_value_to_string(scalar)?);
            }
        }
    }
    Ok(tokens)
}

fn filter_prefill_args(args: &[String]) -> Vec<String> {
    let mut filtered: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--prefill" && i + 1 < args.len() {
            i += 2;
            if i < args.len()
                && !args[i].starts_with("--")
                && (args[i].parse::<u16>().is_ok() || args[i].to_lowercase() == "none")
            {
                i += 1;
            }
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }
    filtered
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum Backend {
    #[value(name = "vllm")]
    Vllm,
    #[value(name = "trtllm")]
    Trtllm,
    #[value(name = "openai")]
    Openai,
    #[value(name = "anthropic")]
    Anthropic,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Backend::Vllm => "vllm",
            Backend::Trtllm => "trtllm",
            Backend::Openai => "openai",
            Backend::Anthropic => "anthropic",
        };
        write!(f, "{}", s)
    }
}

#[derive(Parser, Debug)]
#[command(name = "vllm-router")]
#[command(version)]
#[command(about = "VLLM Router - High-performance request distribution across worker nodes")]
#[command(long_about = r#"
VLLM Router - High-performance request distribution across worker nodes

Usage:
This launcher enables starting a router with individual worker instances. It is useful for
multi-node setups or when you want to start workers and router separately.

Examples:
  # Regular mode
  vllm-router --worker-urls http://worker1:8000 http://worker2:8000

  # vLLM PD mode with pure service discovery (workers register themselves)
  vllm-router --vllm-pd-disaggregation \
    --vllm-discovery-address 0.0.0.0:30001 \
    --policy consistent_hash

  # Note: In vLLM mode, prefill/decode workers automatically register their
  # HTTP and ZMQ addresses via service discovery. No static --prefill or
  # --decode parameters are needed.

"#)]
struct CliArgs {
    /// Host address to bind the router server
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port number to bind the router server
    #[arg(long, default_value_t = 30000)]
    port: u16,

    /// List of worker URLs (e.g., http://worker1:8000 http://worker2:8000)
    #[arg(long, num_args = 0..)]
    worker_urls: Vec<String>,

    /// Load balancing policy to use
    #[arg(long, default_value = "cache_aware", value_parser = ["random", "round_robin", "cache_aware", "power_of_two", "consistent_hash", "rendezvous_hash"])]
    policy: String,

    /// JSON config file for session-affinity / hash-key extraction
    /// (consistent_hash policy only). Example:
    /// {"extra_session_headers": ["x-my-session"], "fallback_to_first_user_prompt": true}
    #[arg(long)]
    hash_key_config: Option<PathBuf>,

    /// JSON router config file. Every option accepted on the command line can
    /// be set here under its snake_case option name. Command line arguments
    /// still take precedence for scalar values.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Enable vLLM PD (Prefill-Decode) disaggregated mode with vLLM-specific two-stage processing
    #[arg(long, default_value_t = false)]
    vllm_pd_disaggregation: bool,

    /// ZMQ service discovery address for vLLM P2P NCCL coordination (e.g., "0.0.0.0:30001")
    /// Required for --vllm-pd-disaggregation mode. Workers register their HTTP and ZMQ addresses here.
    #[arg(long)]
    vllm_discovery_address: Option<String>,

    /// Decode server URL (can be specified multiple times)
    #[arg(long, action = ArgAction::Append)]
    decode: Vec<String>,

    /// Specific policy for prefill nodes in PD mode
    #[arg(long, value_parser = ["random", "round_robin", "cache_aware", "power_of_two", "consistent_hash", "rendezvous_hash"])]
    prefill_policy: Option<String>,

    /// Specific policy for decode nodes in PD mode
    #[arg(long, value_parser = ["random", "round_robin", "cache_aware", "power_of_two", "consistent_hash", "rendezvous_hash"])]
    decode_policy: Option<String>,

    /// Timeout in seconds for worker startup
    #[arg(long, default_value_t = 600)]
    worker_startup_timeout_secs: u64,

    /// Interval in seconds between checks for worker startup
    #[arg(long, default_value_t = 30)]
    worker_startup_check_interval: u64,

    /// Cache threshold (0.0-1.0) for cache-aware routing
    #[arg(long, default_value_t = 0.3)]
    cache_threshold: f32,

    /// Absolute threshold for load balancing
    #[arg(long, default_value_t = 64)]
    balance_abs_threshold: usize,

    /// Relative threshold for load balancing
    #[arg(long, default_value_t = 1.5)]
    balance_rel_threshold: f32,

    /// Interval in seconds between cache eviction operations
    #[arg(long, default_value_t = 120)]
    eviction_interval: u64,

    /// Maximum size of the approximation tree for cache-aware routing
    #[arg(long, default_value_t = 67108864)] // 2^26
    max_tree_size: usize,

    /// Maximum payload size in bytes
    #[arg(long, default_value_t = 536870912)] // 512MB
    max_payload_size: usize,

    /// Intra-node data parallel size (number of DP replicas per worker URL). When > 1, the router will create multiple worker instances per URL, one for each DP rank.
    #[arg(long, default_value_t = 1)]
    intra_node_data_parallel_size: usize,

    /// API key for worker authorization
    #[arg(long)]
    api_key: Option<String>,

    /// API key validation URLs (defaults to env file)
    #[arg(long, num_args = 0..)]
    api_key_validation_urls: Vec<String>,

    /// Backend to route requests to (vllm, trtllm, openai, anthropic)
    #[arg(long, value_enum, default_value_t = Backend::Vllm, alias = "runtime")]
    backend: Backend,

    /// Directory to store log files
    #[arg(long)]
    log_dir: Option<String>,

    /// Set the logging level
    #[arg(long, default_value = "info", value_parser = ["debug", "info", "warn", "error"])]
    log_level: String,

    /// Enable OpenTelemetry tracing
    #[arg(
        long,
        default_value_t = false,
        help_heading = "Tracing (OpenTelemetry)"
    )]
    enable_trace: bool,

    /// OTLP collector endpoint (format: host:port). If omitted, respects OTEL_EXPORTER_OTLP_ENDPOINT.
    #[arg(long, help_heading = "Tracing (OpenTelemetry)")]
    otlp_traces_endpoint: Option<String>,

    /// Parent-based sampling ratio for OpenTelemetry traces.
    #[arg(long, default_value_t = 1.0, help_heading = "Tracing (OpenTelemetry)")]
    otel_sampling_ratio: f64,

    /// Exact HTTP paths to exclude from OpenTelemetry server spans.
    #[arg(long, num_args = 0.., help_heading = "Tracing (OpenTelemetry)")]
    otel_excluded_paths: Vec<String>,

    /// Enable Kubernetes service discovery
    #[arg(long, default_value_t = false)]
    service_discovery: bool,

    /// Label selector for Kubernetes service discovery (format: key1=value1 key2=value2)
    #[arg(long, num_args = 0..)]
    selector: Vec<String>,

    /// Port to use for discovered worker pods
    #[arg(long, default_value_t = 80)]
    service_discovery_port: u16,

    /// Kubernetes namespace to watch for pods
    #[arg(long)]
    service_discovery_namespace: Option<String>,

    /// Label selector for prefill server pods in PD mode
    #[arg(long, num_args = 0..)]
    prefill_selector: Vec<String>,

    /// Label selector for decode server pods in PD mode
    #[arg(long, num_args = 0..)]
    decode_selector: Vec<String>,

    /// Port to expose Prometheus metrics
    #[arg(long, default_value_t = 29000)]
    prometheus_port: u16,

    /// Host address to bind the Prometheus metrics server
    #[arg(long, default_value = "127.0.0.1")]
    prometheus_host: String,

    /// Custom HTTP headers to check for request IDs
    #[arg(long, num_args = 0..)]
    request_id_headers: Vec<String>,

    /// Request timeout in seconds
    #[arg(long, default_value_t = 1800)]
    request_timeout_secs: u64,

    /// Maximum number of concurrent requests allowed
    #[arg(long, default_value_t = 32768)]
    max_concurrent_requests: usize,

    /// CORS allowed origins
    #[arg(long, num_args = 0..)]
    cors_allowed_origins: Vec<String>,

    // Retry configuration
    /// Maximum number of retries
    #[arg(long, default_value_t = 5)]
    retry_max_retries: u32,

    /// Initial backoff in milliseconds for retries
    #[arg(long, default_value_t = 50)]
    retry_initial_backoff_ms: u64,

    /// Maximum backoff in milliseconds for retries
    #[arg(long, default_value_t = 30000)]
    retry_max_backoff_ms: u64,

    /// Backoff multiplier for exponential backoff
    #[arg(long, default_value_t = 1.5)]
    retry_backoff_multiplier: f32,

    /// Jitter factor for retry backoff
    #[arg(long, default_value_t = 0.2)]
    retry_jitter_factor: f32,

    /// Disable retries
    #[arg(long, default_value_t = false)]
    disable_retries: bool,

    // Circuit breaker configuration
    /// Number of failures before circuit breaker opens
    #[arg(long, default_value_t = 10)]
    cb_failure_threshold: u32,

    /// Number of successes before circuit breaker closes
    #[arg(long, default_value_t = 3)]
    cb_success_threshold: u32,

    /// Timeout duration in seconds for circuit breaker
    #[arg(long, default_value_t = 60)]
    cb_timeout_duration_secs: u64,

    /// Window duration in seconds for circuit breaker
    #[arg(long, default_value_t = 120)]
    cb_window_duration_secs: u64,

    /// Disable circuit breaker
    #[arg(long, default_value_t = false)]
    disable_circuit_breaker: bool,

    // Health check configuration
    /// Number of consecutive health check failures before marking worker unhealthy
    #[arg(long, default_value_t = 3)]
    health_failure_threshold: u32,

    /// Number of consecutive health check successes before marking worker healthy
    #[arg(long, default_value_t = 2)]
    health_success_threshold: u32,

    /// Timeout in seconds for health check requests
    #[arg(long, default_value_t = 5)]
    health_check_timeout_secs: u64,

    /// Interval in seconds between runtime health checks
    #[arg(long, default_value_t = 60)]
    health_check_interval_secs: u64,

    /// Health check endpoint path
    #[arg(long, default_value = "/health")]
    health_check_endpoint: String,

    // IGW (Inference Gateway) configuration
    /// Enable Inference Gateway mode
    #[arg(long, default_value_t = false)]
    enable_igw: bool,

    /// History backend configuration (memory or none)
    #[arg(long, default_value = "memory", value_parser = ["memory", "none"])]
    history_backend: String,

    /// Enable profiling calls to vLLM workers
    #[arg(long, default_value_t = false)]
    profile: bool,

    /// KV connector type for PD disaggregation (nixl or mooncake)
    #[arg(long, value_enum, default_value_t = KvConnector::Nixl)]
    kv_connector: KvConnector,
}

/// Parse router CLI + (optionally) config file arguments into a `CliArgs` and
/// the prefill URL list. Config file tokens are inserted before the real CLI
/// tokens, so explicit command line scalars still win.
fn parse_cli_and_prefill(raw_args: &[String]) -> std::io::Result<(CliArgs, PrefillUrls)> {
    let mut combined_args = raw_args.to_vec();
    if let Some(config_path) = config_path_from_args(raw_args) {
        let config_tokens = config_to_cli_args(&config_path, raw_args)?;
        // Keep argv[0] (binary name) first, then config tokens, then real CLI.
        combined_args.splice(1..1, config_tokens);
    }

    let prefill_urls = parse_prefill_args_from(&combined_args);
    let filtered_args = filter_prefill_args(&combined_args);
    let cli_args = CliArgs::try_parse_from(filtered_args)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    Ok((cli_args, prefill_urls))
}

impl CliArgs {
    /// Parse selector strings into HashMap
    fn parse_selector(selector_list: &[String]) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for item in selector_list {
            if let Some(eq_pos) = item.find('=') {
                let key = item[..eq_pos].to_string();
                let value = item[eq_pos + 1..].to_string();
                map.insert(key, value);
            }
        }
        map
    }

    /// Convert policy string to PolicyConfig
    fn parse_policy(&self, policy_str: &str) -> PolicyConfig {
        match policy_str {
            "random" => PolicyConfig::Random,
            "round_robin" => PolicyConfig::RoundRobin,
            "cache_aware" => PolicyConfig::CacheAware {
                cache_threshold: self.cache_threshold,
                balance_abs_threshold: self.balance_abs_threshold,
                balance_rel_threshold: self.balance_rel_threshold,
                eviction_interval_secs: self.eviction_interval,
                max_tree_size: self.max_tree_size,
            },
            "power_of_two" => PolicyConfig::PowerOfTwo {
                load_check_interval_secs: 5, // Default value
            },
            "consistent_hash" => PolicyConfig::ConsistentHash {
                virtual_nodes: 160, // Default value
                session_config: SessionAffinityConfig::default(),
            },
            "rendezvous_hash" => PolicyConfig::RendezvousHash,
            _ => PolicyConfig::RoundRobin, // Fallback
        }
    }

    /// Parse a policy name and attach session-affinity settings from the
    /// optional `--hash-key-config` file.
    fn parse_policy_with_hash_key_config(&self, policy_str: &str) -> ConfigResult<PolicyConfig> {
        let policy = self.parse_policy(policy_str);
        policy.with_session_affinity_config_file(self.hash_key_config.as_deref())
    }

    /// Convert CLI arguments to RouterConfig
    fn to_router_config(
        &self,
        prefill_urls: Vec<(String, Option<u16>)>,
    ) -> ConfigResult<RouterConfig> {
        // Determine routing mode
        let mode = if self.enable_igw {
            // IGW mode - routing mode is not used in IGW, but we need to provide a placeholder
            RoutingMode::Regular {
                worker_urls: vec![],
            }
        } else if matches!(self.backend, Backend::Openai) {
            // OpenAI backend mode - use worker_urls as base(s)
            RoutingMode::OpenAI {
                worker_urls: self.worker_urls.clone(),
            }
        } else if self.vllm_pd_disaggregation {
            // Use decode URLs from CLI arguments (already parsed by clap)
            let decode_urls = &self.decode;

            // Support multiple discovery/configuration modes:
            // 1. Static URLs (--prefill/--decode)
            // 2. vLLM ZMQ discovery (--vllm-discovery-address)
            // 3. K8s service discovery (--service-discovery with --prefill-selector/--decode-selector)
            let use_static_urls = !prefill_urls.is_empty() || !decode_urls.is_empty();
            let use_vllm_discovery = self.vllm_discovery_address.is_some();
            let use_k8s_discovery = self.service_discovery
                && (!self.prefill_selector.is_empty() || !self.decode_selector.is_empty());

            if !use_static_urls && !use_vllm_discovery && !use_k8s_discovery {
                return Err(ConfigError::ValidationFailed {
                    reason: "vLLM PD disaggregation mode requires one of: --vllm-discovery-address, --prefill/--decode URLs, or --service-discovery with --prefill-selector/--decode-selector".to_string(),
                });
            }

            // Use decode URLs directly from CLI
            let final_decode_urls = decode_urls.clone();

            // Log the discovery/configuration mode being used
            if use_k8s_discovery {
                eprintln!("ℹ️  INFO: Using K8s service discovery mode for vLLM PD disaggregation.");
                eprintln!("   Prefill selector: {:?}", self.prefill_selector);
                eprintln!("   Decode selector: {:?}", self.decode_selector);
                if use_static_urls {
                    eprintln!(
                        "   Static fallback URLs - Prefill: {:?}, Decode: {:?}",
                        prefill_urls, final_decode_urls
                    );
                }
            } else if use_static_urls && use_vllm_discovery {
                eprintln!("ℹ️  INFO: Using hybrid mode - static URLs as fallback, vLLM ZMQ discovery for dynamic workers.");
                eprintln!("   Prefill URLs: {:?}", prefill_urls);
                eprintln!("   Decode URLs: {:?}", final_decode_urls);
                eprintln!("   Discovery address: {:?}", self.vllm_discovery_address);
            } else if use_static_urls {
                eprintln!("ℹ️  INFO: Using static URL mode without service discovery.");
                eprintln!("   Prefill URLs: {:?}", prefill_urls);
                eprintln!("   Decode URLs: {:?}", final_decode_urls);
            } else {
                eprintln!("ℹ️  INFO: Using vLLM ZMQ service discovery mode.");
                eprintln!("   Discovery address: {:?}", self.vllm_discovery_address);
            }

            RoutingMode::VllmPrefillDecode {
                prefill_urls: prefill_urls.clone(),
                decode_urls: final_decode_urls,
                prefill_policy: self
                    .prefill_policy
                    .as_ref()
                    .map(|p| self.parse_policy_with_hash_key_config(p))
                    .transpose()?,
                decode_policy: self
                    .decode_policy
                    .as_ref()
                    .map(|p| self.parse_policy_with_hash_key_config(p))
                    .transpose()?,
                discovery_address: self.vllm_discovery_address.clone(),
            }
        } else {
            // Regular mode
            if !self.service_discovery && self.worker_urls.is_empty() {
                return Err(ConfigError::ValidationFailed {
                    reason: "Regular mode requires --worker-urls when not using service discovery"
                        .to_string(),
                });
            }
            RoutingMode::Regular {
                worker_urls: self.worker_urls.clone(),
            }
        };

        // Main policy
        let policy = self.parse_policy_with_hash_key_config(&self.policy)?;

        // Service discovery configuration
        let discovery = if self.service_discovery {
            Some(DiscoveryConfig {
                enabled: true,
                namespace: self.service_discovery_namespace.clone(),
                port: self.service_discovery_port,
                check_interval_secs: 60,
                selector: Self::parse_selector(&self.selector),
                prefill_selector: Self::parse_selector(&self.prefill_selector),
                decode_selector: Self::parse_selector(&self.decode_selector),
                bootstrap_port_annotation: "vllm.ai/bootstrap-port".to_string(),
            })
        } else {
            None
        };

        // Metrics configuration
        let metrics = Some(MetricsConfig {
            port: self.prometheus_port,
            host: self.prometheus_host.clone(),
        });

        let connection_mode = ConnectionMode::Http;

        let api_key_validation_urls = if !self.api_key_validation_urls.is_empty() {
            self.api_key_validation_urls.clone()
        } else if let Ok(raw_urls) = std::env::var("API_KEY_VALIDATION_URLS") {
            raw_urls
                .split(',')
                .map(|url| url.trim().to_string())
                .filter(|url| !url.is_empty())
                .collect()
        } else {
            Vec::new()
        };

        // Build RouterConfig
        Ok(RouterConfig {
            mode,
            policy,
            connection_mode,
            host: self.host.clone(),
            port: self.port,
            max_payload_size: self.max_payload_size,
            request_timeout_secs: self.request_timeout_secs,
            worker_startup_timeout_secs: self.worker_startup_timeout_secs,
            worker_startup_check_interval_secs: self.worker_startup_check_interval,
            intra_node_data_parallel_size: self.intra_node_data_parallel_size,
            api_key: self.api_key.clone(),
            api_key_validation_urls,
            discovery,
            metrics,
            log_dir: self.log_dir.clone(),
            log_level: Some(self.log_level.clone()),
            request_id_headers: if self.request_id_headers.is_empty() {
                None
            } else {
                Some(self.request_id_headers.clone())
            },
            max_concurrent_requests: self.max_concurrent_requests,
            queue_size: 100,        // Default queue size
            queue_timeout_secs: 60, // Default timeout
            cors_allowed_origins: self.cors_allowed_origins.clone(),
            retry: RetryConfig {
                max_retries: self.retry_max_retries,
                initial_backoff_ms: self.retry_initial_backoff_ms,
                max_backoff_ms: self.retry_max_backoff_ms,
                backoff_multiplier: self.retry_backoff_multiplier,
                jitter_factor: self.retry_jitter_factor,
            },
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: self.cb_failure_threshold,
                success_threshold: self.cb_success_threshold,
                timeout_duration_secs: self.cb_timeout_duration_secs,
                window_duration_secs: self.cb_window_duration_secs,
            },
            disable_retries: self.disable_retries,
            disable_circuit_breaker: self.disable_circuit_breaker,
            health_check: HealthCheckConfig {
                failure_threshold: self.health_failure_threshold,
                success_threshold: self.health_success_threshold,
                timeout_secs: self.health_check_timeout_secs,
                check_interval_secs: self.health_check_interval_secs,
                endpoint: self.health_check_endpoint.clone(),
            },
            enable_igw: self.enable_igw,
            rate_limit_tokens_per_second: None,
            history_backend: match self.history_backend.as_str() {
                "none" => HistoryBackend::None,
                _ => HistoryBackend::Memory,
            },
            enable_profiling: self.profile,
            profile_timeout_secs: 10, // Default profiling timeout
            kv_connector: self.kv_connector,
        })
    }

    /// Create ServerConfig from CLI args and RouterConfig
    fn to_server_config(&self, router_config: RouterConfig) -> ServerConfig {
        // Create service discovery config if enabled
        let service_discovery_config = if self.service_discovery {
            Some(ServiceDiscoveryConfig {
                enabled: true,
                selector: Self::parse_selector(&self.selector),
                check_interval: std::time::Duration::from_secs(60),
                port: self.service_discovery_port,
                namespace: self.service_discovery_namespace.clone(),
                // HTTP service discovery only supports the vLLM PD router.
                pd_mode: self.vllm_pd_disaggregation,
                prefill_selector: Self::parse_selector(&self.prefill_selector),
                decode_selector: Self::parse_selector(&self.decode_selector),
                bootstrap_port_annotation: "vllm.ai/bootstrap-port".to_string(),
            })
        } else {
            None
        };

        // Create Prometheus config
        let prometheus_config = Some(PrometheusConfig {
            port: self.prometheus_port,
            host: self.prometheus_host.clone(),
        });

        ServerConfig {
            host: self.host.clone(),
            port: self.port,
            router_config,
            max_payload_size: self.max_payload_size,
            log_dir: self.log_dir.clone(),
            log_level: Some(self.log_level.clone()),
            service_discovery_config,
            prometheus_config,
            request_timeout_secs: self.request_timeout_secs,
            request_id_headers: if self.request_id_headers.is_empty() {
                None
            } else {
                Some(self.request_id_headers.clone())
            },
            trace_config: if self.enable_trace {
                Some(TraceConfig {
                    otlp_traces_endpoint: self.otlp_traces_endpoint.clone(),
                    sampling_ratio: self.otel_sampling_ratio,
                    excluded_paths: if self.otel_excluded_paths.is_empty() {
                        TraceConfig::default_excluded_paths()
                    } else {
                        self.otel_excluded_paths.clone()
                    },
                })
            } else {
                None
            },
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    println!("DEBUG: Main function started");

    let raw_args: Vec<String> = std::env::args().collect();
    println!("DEBUG: Raw args: {:?}", raw_args);
    println!("DEBUG: Parsing CLI + config file arguments");
    let (cli_args, prefill_urls) = parse_cli_and_prefill(&raw_args)?;
    println!("DEBUG: Prefill URLs parsed: {:?}", prefill_urls);
    println!("DEBUG: CLI args parsed successfully");
    println!(
        "DEBUG: vllm_pd_disaggregation: {}",
        cli_args.vllm_pd_disaggregation
    );

    // Print startup info
    println!("VLLM Router starting...");
    println!("Host: {}:{}", cli_args.host, cli_args.port);
    let mode_str = if cli_args.enable_igw {
        "IGW (Inference Gateway)".to_string()
    } else if matches!(cli_args.backend, Backend::Openai) {
        "OpenAI Backend".to_string()
    } else if cli_args.vllm_pd_disaggregation {
        "vLLM PD Disaggregated".to_string()
    } else {
        format!("Regular ({})", cli_args.backend)
    };
    println!("Mode: {}", mode_str);

    // Warn for runtimes that are parsed but not yet implemented
    match cli_args.backend {
        Backend::Trtllm | Backend::Anthropic => {
            println!(
                "WARNING: runtime '{}' not implemented yet; falling back to regular routing. \
Provide --worker-urls or PD flags as usual.",
                cli_args.backend
            );
        }
        Backend::Vllm | Backend::Openai => {}
    }

    if !cli_args.enable_igw {
        println!("Policy: {}", cli_args.policy);

        if cli_args.vllm_pd_disaggregation && !prefill_urls.is_empty() {
            println!("Prefill nodes: {:?}", prefill_urls);
            println!("Decode nodes: {:?}", cli_args.decode);
        }
    }

    // Convert to RouterConfig
    println!("DEBUG: Converting to RouterConfig");
    let router_config = cli_args.to_router_config(prefill_urls)?;
    println!("DEBUG: RouterConfig created successfully");

    // Validate configuration
    println!("DEBUG: Validating configuration");
    router_config.validate()?;
    println!("DEBUG: Configuration validated successfully");

    // Create ServerConfig
    println!("DEBUG: Creating ServerConfig");
    println!(
        "DEBUG: CLI host: {}, port: {}",
        cli_args.host, cli_args.port
    );
    let server_config = cli_args.to_server_config(router_config);
    println!(
        "DEBUG: ServerConfig created successfully - host: {}, port: {}",
        server_config.host, server_config.port
    );

    // Create a new runtime for the server (like Python binding does)
    println!("DEBUG: Creating Tokio runtime");
    let runtime = tokio::runtime::Runtime::new()?;
    println!("DEBUG: Tokio runtime created successfully");

    // Block on the async startup function
    println!("DEBUG: Starting server startup function");
    runtime.block_on(async move {
        let result = server::startup(server_config).await;
        // Shut down OTel while the Tokio runtime is still alive so the
        // BatchSpanProcessor can flush its final batch.
        if vllm_router_rs::otel_trace::is_otel_enabled() {
            vllm_router_rs::otel_trace::shutdown_otel();
        }
        result
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_hash_key_config_is_loaded_from_cli_flag() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("session.json");
        std::fs::write(
            &config_path,
            r#"{"extra_session_headers": ["x-cli-session"]}"#,
        )
        .unwrap();

        let cli = CliArgs::try_parse_from([
            "vllm-router",
            "--worker-urls",
            "http://worker1:8000",
            "--policy",
            "consistent_hash",
            "--hash-key-config",
            config_path.to_str().unwrap(),
        ])
        .expect("CLI args should parse");

        let policy = cli
            .parse_policy_with_hash_key_config("consistent_hash")
            .expect("policy config should load");
        match policy {
            PolicyConfig::ConsistentHash { session_config, .. } => {
                assert_eq!(
                    session_config.extra_session_headers,
                    vec!["x-cli-session".to_string()]
                );
            }
            other => panic!("expected consistent_hash, got {}", other.name()),
        }
    }

    fn write_json_config(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn test_full_config_file_sets_cli_options() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_json_config(
            dir.path(),
            "router.json",
            r#"{
                "host": "0.0.0.0",
                "port": 30001,
                "worker_urls": ["http://worker1:8000", "http://worker2:8000"],
                "policy": "consistent_hash",
                "retry_max_retries": 3,
                "request_id_headers": ["x-session-id"],
                "service_discovery": true,
                "selector": {"app": "worker", "env": "prod"}
            }"#,
        );

        let raw = vec![
            "vllm-router".to_string(),
            "--config".to_string(),
            config_path,
        ];
        let (cli, _prefill) = parse_cli_and_prefill(&raw).unwrap();

        assert_eq!(cli.host, "0.0.0.0");
        assert_eq!(cli.port, 30001);
        assert_eq!(
            cli.worker_urls,
            vec![
                "http://worker1:8000".to_string(),
                "http://worker2:8000".to_string()
            ]
        );
        assert_eq!(cli.policy, "consistent_hash");
        assert_eq!(cli.retry_max_retries, 3);
        assert_eq!(cli.request_id_headers, vec!["x-session-id".to_string()]);
        assert!(cli.service_discovery);
        assert_eq!(
            cli.selector,
            vec!["app=worker".to_string(), "env=prod".to_string()]
        );
    }

    #[test]
    fn test_config_file_pd_prefill_and_decode() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_json_config(
            dir.path(),
            "pd.json",
            r#"{
                "vllm_pd_disaggregation": true,
                "prefill": [["http://prefill1:8000", "9000"], ["http://prefill2:8000"]],
                "decode": ["http://decode1:8001", "http://decode2:8001"],
                "prefill_policy": "consistent_hash",
                "decode_policy": "consistent_hash"
            }"#,
        );

        let raw = vec![
            "vllm-router".to_string(),
            "--config".to_string(),
            config_path,
        ];
        let (cli, prefill_urls) = parse_cli_and_prefill(&raw).unwrap();

        assert!(cli.vllm_pd_disaggregation);
        assert_eq!(
            prefill_urls,
            vec![
                ("http://prefill1:8000".to_string(), Some(9000)),
                ("http://prefill2:8000".to_string(), None),
            ]
        );
        assert_eq!(
            cli.decode,
            vec![
                "http://decode1:8001".to_string(),
                "http://decode2:8001".to_string()
            ]
        );
        assert_eq!(cli.prefill_policy.as_deref(), Some("consistent_hash"));
        assert_eq!(cli.decode_policy.as_deref(), Some("consistent_hash"));
    }

    #[test]
    fn test_cli_arguments_override_config_scalars() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_json_config(
            dir.path(),
            "override.json",
            r#"{
                "host": "1.2.3.4",
                "port": 30001,
                "worker_urls": ["http://worker1:8000"]
            }"#,
        );

        let raw = vec![
            "vllm-router".to_string(),
            "--config".to_string(),
            config_path,
            "--host".to_string(),
            "9.9.9.9".to_string(),
            "--port".to_string(),
            "31111".to_string(),
        ];
        let (cli, _) = parse_cli_and_prefill(&raw).unwrap();
        assert_eq!(cli.host, "9.9.9.9");
        assert_eq!(cli.port, 31111);
        // Config list values are used when no CLI value is supplied.
        assert_eq!(cli.worker_urls, vec!["http://worker1:8000".to_string()]);
    }

    #[test]
    fn test_unknown_config_option_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_json_config(dir.path(), "bad.json", r#"{"not_an_option": 1}"#);
        let raw = vec![
            "vllm-router".to_string(),
            "--config".to_string(),
            config_path,
        ];
        let result = parse_cli_and_prefill(&raw);
        assert!(
            result.is_err(),
            "unknown config option should fail clap parse"
        );
    }

    #[test]
    fn test_native_example_config_parses() {
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples/configs/router_config.native.example.json");
        let raw = vec![
            "vllm-router".to_string(),
            "--config".to_string(),
            example.to_string_lossy().into_owned(),
        ];
        let (cli, prefill_urls) = parse_cli_and_prefill(&raw).unwrap();

        assert_eq!(cli.host, "127.0.0.1");
        assert_eq!(cli.port, 30000);
        assert_eq!(cli.policy, "consistent_hash");
        assert_eq!(cli.backend, Backend::Vllm);
        assert_eq!(cli.retry_max_retries, 5);
        assert_eq!(cli.otel_sampling_ratio, 1.0);
        assert!(cli.hash_key_config.is_some());
        assert!(prefill_urls.is_empty());
    }

    #[test]
    fn test_consistent_hash_example_config_parses() {
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples/configs/consistent_hash_router_config.example.json");
        let raw = vec![
            "vllm-router".to_string(),
            "--config".to_string(),
            example.to_string_lossy().into_owned(),
        ];
        let (cli, _prefill_urls) = parse_cli_and_prefill(&raw).unwrap();

        assert_eq!(cli.policy, "consistent_hash");
        assert_eq!(
            cli.worker_urls,
            vec![
                "http://worker1:8000".to_string(),
                "http://worker2:8000".to_string(),
                "http://worker3:8000".to_string(),
            ]
        );
        assert!(cli
            .hash_key_config
            .as_deref()
            .unwrap()
            .ends_with("consistent_hash_session_config.full.json"));
    }
}
