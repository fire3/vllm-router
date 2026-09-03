//! Session-affinity / hash-key configuration for `consistent_hash` routing.
//!
//! Coding agents (Claude Code, Codex CLI, OpenCode, Pi, Roo Code, Cline, ...)
//! identify a conversation/session in different ways:
//!
//! * explicit HTTP headers such as `x-claude-code-session-id`,
//!   `x-session-affinity`, `session-id`, `thread-id`;
//! * body fields such as Anthropic `metadata.user_id` (`_session_<uuid>`
//!   suffix), Responses API `client_metadata.session_id` / `thread_id`, and
//!   `prompt_cache_key`;
//! * or nothing at all, in which case the router can fall back to hashing the
//!   first user prompt so multi-turn coding-agent sessions stay sticky.
//!
//! The configuration below is loaded from a JSON file and attached to
//! `PolicyConfig::ConsistentHash`. When no file is supplied the router uses
//! the built-in defaults from `src/policies/hash_key.rs`.
//!
//! The same file also configures **session placement**: whether brand-new
//! sessions are placed by the pure hash ring (`ring`) or sampled across
//! ring neighbors by real-time load (`min_load`) and then pinned, so later
//! turns of the same agent conversation keep KV locality.

use crate::config::ConfigResult;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// How a brand-new session (no recorded pin yet) should pick its worker.
///
/// `Ring` keeps the historical behavior: the consistent-hash ring alone
/// decides. `MinLoad` samples `placement_candidates` ring neighbors and picks
/// the least loaded one, then pins the session so later turns keep KV
/// locality even though the first placement may have deviated from the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConsistentHashNewSessionStrategy {
    /// Historical behavior: pure consistent-hash placement.
    #[default]
    #[serde(rename = "ring")]
    Ring,

    /// New sessions pick the least loaded of K ring neighbors and are pinned.
    #[serde(rename = "min_load")]
    MinLoad,
}

/// Configuration for session-aware hash key extraction.
///
/// All fields have defaults; loading a partial file is supported.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SessionAffinityConfig {
    /// Full ordered list of session/thread/user header names used as routing
    /// keys. `None` (or an omitted field) means "use the built-in defaults".
    ///
    /// Header names are case-insensitive on the wire; router header maps are
    /// normalized to lowercase, so entries are matched case-insensitively.
    pub session_headers: Option<Vec<String>>,

    /// Extra session header names appended after `session_headers` (or after
    /// the built-in defaults when `session_headers` is `None`). This lets an
    /// operator "supplement" the defaults without replacing them.
    pub extra_session_headers: Vec<String>,

    /// Whether body-level session/user fields are consulted after headers.
    /// This covers `session_params.session_id`, Anthropic `metadata.user_id`,
    /// Responses `client_metadata.session_id/thread_id`, `conversation_id`,
    /// and legacy `session_id` / `user` / `user_id`.
    pub use_body_session_fields: bool,

    /// Whether to fall back to hashing the **first user prompt** when neither
    /// a session header nor a body session field is present. Coding agents
    /// resend the full conversation on every turn, so hashing the whole body
    /// would break affinity; the first user prompt is the most stable anchor
    /// available without client cooperation.
    pub fallback_to_first_user_prompt: bool,

    /// Placement rule for sessions with no recorded pin yet.
    pub new_session_strategy: ConsistentHashNewSessionStrategy,

    /// How many distinct ring neighbors to sample for `min_load` placement.
    pub placement_candidates: usize,

    /// How long a session pin is considered valid (seconds).
    /// `0` means pins never expire (until worker set changes or eviction).
    pub session_pin_ttl_secs: u64,

    /// Maximum number of remembered session pins (bounds memory).
    pub max_session_pins: usize,
}

impl Default for SessionAffinityConfig {
    fn default() -> Self {
        Self {
            session_headers: None,
            extra_session_headers: Vec::new(),
            use_body_session_fields: true,
            fallback_to_first_user_prompt: true,
            new_session_strategy: ConsistentHashNewSessionStrategy::Ring,
            placement_candidates: 2,
            session_pin_ttl_secs: 24 * 60 * 60, // 24h
            max_session_pins: 100_000,
        }
    }
}

impl SessionAffinityConfig {
    /// Load a session-affinity configuration from a UTF-8 JSON file.
    pub fn from_file(path: impl AsRef<Path>) -> ConfigResult<Self> {
        let path = path.as_ref();
        let content =
            fs::read_to_string(path).map_err(|e| crate::config::ConfigError::InvalidValue {
                field: "session_affinity_config".to_string(),
                value: path.display().to_string(),
                reason: format!("cannot read config file: {e}"),
            })?;
        serde_json::from_str(&content).map_err(|e| crate::config::ConfigError::InvalidValue {
            field: "session_affinity_config".to_string(),
            value: path.display().to_string(),
            reason: format!("invalid JSON: {e}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ConsistentHashNewSessionStrategy;
    use crate::config::{PolicyConfig, SessionAffinityConfig};
    use std::fs;

    #[test]
    fn test_default_config() {
        let config = SessionAffinityConfig::default();
        assert!(config.session_headers.is_none());
        assert!(config.extra_session_headers.is_empty());
        assert!(config.use_body_session_fields);
        assert!(config.fallback_to_first_user_prompt);
        assert_eq!(
            config.new_session_strategy,
            ConsistentHashNewSessionStrategy::Ring
        );
        assert_eq!(config.placement_candidates, 2);
        assert_eq!(config.session_pin_ttl_secs, 24 * 60 * 60);
        assert_eq!(config.max_session_pins, 100_000);
    }

    #[test]
    fn test_deserialize_partial_json() {
        let config: SessionAffinityConfig =
            serde_json::from_str(r#"{"extra_session_headers": ["x-my-session"]}"#).unwrap();
        assert!(config.session_headers.is_none());
        assert_eq!(config.extra_session_headers, vec!["x-my-session"]);
        assert!(config.use_body_session_fields);
        assert!(config.fallback_to_first_user_prompt);
        assert_eq!(
            config.new_session_strategy,
            ConsistentHashNewSessionStrategy::Ring
        );
        assert_eq!(config.max_session_pins, 100_000);
    }

    #[test]
    fn test_deserialize_session_placement_config() {
        let config: SessionAffinityConfig = serde_json::from_str(
            r#"{
                "new_session_strategy": "min_load",
                "placement_candidates": 3,
                "session_pin_ttl_secs": 7200,
                "max_session_pins": 50
            }"#,
        )
        .unwrap();
        assert_eq!(
            config.new_session_strategy,
            ConsistentHashNewSessionStrategy::MinLoad
        );
        assert_eq!(config.placement_candidates, 3);
        assert_eq!(config.session_pin_ttl_secs, 7200);
        assert_eq!(config.max_session_pins, 50);
    }

    #[test]
    fn test_deserialize_full_json() {
        let config: SessionAffinityConfig = serde_json::from_str(
            r#"{
                "session_headers": ["x-session-id", "x-custom-session"],
                "extra_session_headers": ["x-tenant"],
                "use_body_session_fields": false,
                "fallback_to_first_user_prompt": false
            }"#,
        )
        .unwrap();
        assert_eq!(
            config.session_headers,
            Some(vec![
                "x-session-id".to_string(),
                "x-custom-session".to_string()
            ])
        );
        assert_eq!(config.extra_session_headers, vec!["x-tenant"]);
        assert!(!config.use_body_session_fields);
        assert!(!config.fallback_to_first_user_prompt);
    }

    #[test]
    fn test_config_file_attaches_to_consistent_hash_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(
            &path,
            r#"{
                "session_headers": ["x-session-id", "x-my-session"],
                "fallback_to_first_user_prompt": false
            }"#,
        )
        .unwrap();

        let policy = PolicyConfig::ConsistentHash {
            virtual_nodes: 160,
            session_config: SessionAffinityConfig::default(),
        };
        let updated = policy
            .with_session_affinity_config_file(Some(path.as_path()))
            .unwrap();
        match updated {
            PolicyConfig::ConsistentHash {
                virtual_nodes,
                session_config,
            } => {
                assert_eq!(virtual_nodes, 160);
                assert_eq!(
                    session_config.session_headers,
                    Some(vec!["x-session-id".to_string(), "x-my-session".to_string()])
                );
                assert!(!session_config.fallback_to_first_user_prompt);
            }
            _ => panic!("expected consistent_hash policy"),
        }
    }

    #[test]
    fn test_config_file_rejected_for_non_consistent_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(&path, "{}").unwrap();

        let result =
            PolicyConfig::RoundRobin.with_session_affinity_config_file(Some(path.as_path()));
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_config_file_is_an_error() {
        let result = PolicyConfig::ConsistentHash {
            virtual_nodes: 160,
            session_config: SessionAffinityConfig::default(),
        }
        .with_session_affinity_config_file(Some(std::path::Path::new(
            "/nonexistent/session-affinity.json",
        )));
        assert!(result.is_err());
    }
}
