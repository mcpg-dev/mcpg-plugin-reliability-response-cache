//! Response Caching ToolGate plugin for MCPG.
//!
//! Caches successful tool results in memory with configurable TTL.
//! Uses `modified_result` in the pre-dispatch `Allow` to short-circuit
//! backend dispatch when a cached result is available.
//!
//! Distributed as a `native-cdylib-v1` plugin.

use dashmap::DashMap;
use mcpg_plugin_protocol::{GateDecision, PluginClass, PluginContext, PluginManifest};
use mcpg_plugin_sdk::declare_plugin;
use mcpg_plugin_sdk::ffi::SyncToolGate;
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::debug;

const PLUGIN_ID: &str = "dev.mcpg.response-cache";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseCacheConfig {
    /// Default TTL for cached results (seconds).
    #[serde(default = "default_ttl_ms")]
    pub default_ttl_ms: u64,
    /// Maximum number of entries in the cache.
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
    /// Per-tool TTL overrides (glob patterns supported).
    #[serde(default)]
    pub per_tool: Vec<ToolCacheConfig>,
    /// Cache scope: "shared" (all identities share cache) or "per_identity".
    #[serde(default = "default_cache_scope")]
    pub cache_scope: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCacheConfig {
    /// Tool name pattern (supports `*` glob).
    pub tools: Vec<String>,
    /// TTL for matching tools (seconds). 0 means no caching.
    pub ttl_ms: u64,
}

fn default_ttl_ms() -> u64 {
    300000
}
fn default_max_entries() -> usize {
    10_000
}
fn default_cache_scope() -> String {
    "shared".to_owned()
}

impl Default for ResponseCacheConfig {
    fn default() -> Self {
        Self {
            default_ttl_ms: default_ttl_ms(),
            max_entries: default_max_entries(),
            per_tool: Vec::new(),
            cache_scope: default_cache_scope(),
        }
    }
}

// ---------------------------------------------------------------------------
// Cache internals
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CacheEntry {
    result: serde_json::Value,
    stored_at: Instant,
    ttl: Duration,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.stored_at.elapsed() >= self.ttl
    }
}

/// Canonical JSON key for cache lookups.
///
/// The MCP surface is part of the key so a prompt and a tool with the
/// same name do not collide.
fn cache_key(
    surface: &str,
    tool_name: &str,
    arguments: &serde_json::Value,
    identity_key: Option<&str>,
) -> u64 {
    let canonical_args = canonical_json(arguments);
    let mut hasher = DefaultHasher::new();
    surface.hash(&mut hasher);
    tool_name.hash(&mut hasher);
    canonical_args.hash(&mut hasher);
    if let Some(id) = identity_key {
        id.hash(&mut hasher);
    }
    hasher.finish()
}

/// Produce a canonical JSON string by sorting object keys.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let entries: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k as &str).unwrap(),
                        canonical_json(&map[*k])
                    )
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        serde_json::Value::Array(arr) => {
            let entries: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", entries.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct ResponseCachePlugin {
    manifest: PluginManifest,
    config: ResponseCacheConfig,
    cache: DashMap<u64, CacheEntry>,
    entry_count: AtomicU64,
}

impl ResponseCachePlugin {
    pub fn new(config: ResponseCacheConfig) -> Self {
        Self {
            manifest: PluginManifest {
                id: PLUGIN_ID.into(),
                version: env!("CARGO_PKG_VERSION").into(),
                name: "Response Cache".into(),
                plugin_class: PluginClass::ToolGate,
                protocol_version: "1.0".into(),
                license: None,
                required_capabilities: Vec::new(),
                tags: Vec::new(),
                provides: Vec::new(),
                provides_schemes: Vec::new(),
                module_path_prefix: ::std::module_path!()
                    .split("::")
                    .next()
                    .unwrap_or("")
                    .to_owned(),
                backend_profile: None,
            },
            config,
            cache: DashMap::new(),
            entry_count: AtomicU64::new(0),
        }
    }

    pub fn from_config(config_value: &serde_json::Value) -> Result<Self, String> {
        let config: ResponseCacheConfig =
            serde_json::from_value(config_value.clone()).map_err(|e| format!("{e}"))?;
        Ok(Self::new(config))
    }

    pub fn from_config_json(config_json: &str) -> Self {
        let config: ResponseCacheConfig =
            mcpg_plugin_sdk::fail_closed_config!(config_json, ResponseCacheConfig);
        Self::new(config)
    }

    fn ttl_for(&self, tool_name: &str) -> Duration {
        for rule in &self.config.per_tool {
            for pattern in &rule.tools {
                if glob_match(pattern, tool_name) {
                    return Duration::from_millis(rule.ttl_ms);
                }
            }
        }
        Duration::from_millis(self.config.default_ttl_ms)
    }

    fn identity_key(&self, ctx: &PluginContext) -> Option<String> {
        if self.config.cache_scope == "per_identity" {
            ctx.identity.subject_id.clone()
        } else {
            None
        }
    }

    fn evict_if_full(&self) {
        let count = self.entry_count.load(Ordering::Relaxed) as usize;
        if count >= self.config.max_entries {
            // Evict expired entries first
            self.cache.retain(|_, v| !v.is_expired());
            let new_count = self.cache.len() as u64;
            self.entry_count.store(new_count, Ordering::Relaxed);

            // If still full, evict oldest
            if new_count as usize >= self.config.max_entries {
                let mut oldest_key = None;
                let mut oldest_stored = Instant::now();
                for entry in self.cache.iter() {
                    if entry.value().stored_at < oldest_stored {
                        oldest_stored = entry.value().stored_at;
                        oldest_key = Some(*entry.key());
                    }
                }
                if let Some(key) = oldest_key {
                    self.cache.remove(&key);
                    self.entry_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }
}

use mcpg_glob::glob_match;

impl SyncToolGate for ResponseCachePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn evaluate_pre(
        &self,
        ctx: &PluginContext,
        arguments: &serde_json::Value,
        meta: Option<&serde_json::Value>,
        _config: &serde_json::Value,
    ) -> GateDecision {
        // Plugin-scoped span so traces from response cache
        // attribute back to dev.mcpg.response-cache.
        let _span = tracing::info_span!(
            "response_cache_evaluate_pre",
            plugin_id = PLUGIN_ID,
            tool = %ctx.tool_name,
        )
        .entered();
        let started = std::time::Instant::now();
        let decision = self.evaluate_pre_inner(ctx, arguments, meta);
        metrics::histogram!("mcpg_response_cache_evaluate_ms")
            .record(started.elapsed().as_millis() as f64);
        decision
    }

    fn evaluate_post(
        &self,
        ctx: &PluginContext,
        arguments: &serde_json::Value,
        result: &serde_json::Value,
        _execution_duration_ms: u64,
        _config: &serde_json::Value,
    ) -> GateDecision {
        // Don't cache errors
        if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            return GateDecision::allow();
        }

        let ttl = self.ttl_for(&ctx.tool_name);
        if ttl.is_zero() {
            return GateDecision::allow();
        }

        let identity_key = self.identity_key(ctx);
        let key = cache_key(
            &ctx.surface,
            &ctx.tool_name,
            arguments,
            identity_key.as_deref(),
        );

        self.evict_if_full();

        self.cache.insert(
            key,
            CacheEntry {
                result: result.clone(),
                stored_at: Instant::now(),
                ttl,
            },
        );
        self.entry_count.fetch_add(1, Ordering::Relaxed);

        debug!(tool = %ctx.tool_name, ttl_ms = ttl.as_secs(), "cached result");
        GateDecision::allow()
    }
}

impl ResponseCachePlugin {
    fn evaluate_pre_inner(
        &self,
        ctx: &PluginContext,
        arguments: &serde_json::Value,
        meta: Option<&serde_json::Value>,
    ) -> GateDecision {
        // Check for no_cache flag in _meta
        if let Some(meta) = meta
            && meta.get("no_cache").and_then(|v| v.as_bool()) == Some(true)
        {
            debug!(tool = %ctx.tool_name, "no_cache flag set — skipping cache");
            return GateDecision::allow();
        }

        let ttl = self.ttl_for(&ctx.tool_name);
        if ttl.is_zero() {
            return GateDecision::allow();
        }

        let identity_key = self.identity_key(ctx);
        let key = cache_key(
            &ctx.surface,
            &ctx.tool_name,
            arguments,
            identity_key.as_deref(),
        );

        if let Some(entry) = self.cache.get(&key) {
            if !entry.is_expired() {
                debug!(tool = %ctx.tool_name, "cache hit");
                metrics::counter!("mcpg_response_cache_total",
                    "tool" => ctx.tool_name.clone(),
                    "result" => "hit",
                )
                .increment(1);
                return GateDecision::Allow {
                    modified_arguments: None,
                    modified_result: Some(entry.result.clone()),
                    metadata: Some(serde_json::json!({"cache": "hit"})),
                };
            }
            // Expired — remove
            drop(entry);
            self.cache.remove(&key);
            self.entry_count.fetch_sub(1, Ordering::Relaxed);
        }

        debug!(tool = %ctx.tool_name, "cache miss");
        metrics::counter!("mcpg_response_cache_total",
            "tool" => ctx.tool_name.clone(),
            "result" => "miss",
        )
        .increment(1);
        GateDecision::allow()
    }
}

declare_plugin! {
    plugin_id: PLUGIN_ID,
    plugin_version: env!("CARGO_PKG_VERSION"),
    descriptor_yaml: include_str!("../plugin.yaml"),
    capabilities: &[],
    entities: [
        tool_gate as gate {
            inner_name: "",
            plugin_type: ResponseCachePlugin,
            factory: |cfg: &str, _host: ::mcpg_plugin_sdk::HostHandle| ResponseCachePlugin::from_config_json(cfg),
        }
    ],
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mcpg_plugin_protocol::PluginIdentity;

    fn test_ctx(tool: &str) -> PluginContext {
        PluginContext {
            surface: "tool".to_owned(),
            request_id: "req-test".to_owned(),
            session_id: Some("sess-test".to_owned()),
            tool_name: tool.to_owned(),
            identity: PluginIdentity {
                kind: "verified".to_owned(),
                trust_level: "verified".to_owned(),
                subject_id: Some("user@test.com".to_owned()),
                auth_provider: None,
                issuer: None,
                roles: vec![],
                groups: vec![],
                scopes: vec![],
                attributes: Default::default(),
            },
            transport: "http".to_owned(),
        }
    }

    fn success_result() -> serde_json::Value {
        serde_json::json!({
            "content": [{"type": "text", "text": "hello"}],
            "isError": false
        })
    }

    #[test]
    fn cache_miss_on_first_call() {
        let plugin = ResponseCachePlugin::new(ResponseCacheConfig::default());
        let ctx = test_ctx("test_tool");
        let config = serde_json::json!({});
        let result = plugin.evaluate_pre(&ctx, &serde_json::json!({"q": "test"}), None, &config);
        assert!(result.is_allow());
        // No modified_result on cache miss
        if let GateDecision::Allow {
            modified_result, ..
        } = result
        {
            assert!(modified_result.is_none());
        }
    }

    #[test]
    fn cache_hit_returns_cached_result() {
        let plugin = ResponseCachePlugin::new(ResponseCacheConfig::default());
        let ctx = test_ctx("test_tool");
        let config = serde_json::json!({});
        let args = serde_json::json!({"q": "test"});
        let result = success_result();

        // Populate cache via post_dispatch
        plugin.evaluate_post(&ctx, &args, &result, 50, &config);

        // Second call should be a cache hit
        let decision = plugin.evaluate_pre(&ctx, &args, None, &config);
        if let GateDecision::Allow {
            modified_result, ..
        } = decision
        {
            assert!(modified_result.is_some());
            assert_eq!(modified_result.unwrap(), result);
        } else {
            panic!("expected Allow with modified_result");
        }
    }

    #[test]
    fn ttl_expiry_causes_cache_miss() {
        let plugin = ResponseCachePlugin::new(ResponseCacheConfig {
            default_ttl_ms: 0, // immediate expiry (0 = no caching)
            ..Default::default()
        });
        let ctx = test_ctx("test_tool");
        let config = serde_json::json!({});
        let args = serde_json::json!({"q": "test"});

        // With ttl=0, entries are not cached
        plugin.evaluate_post(&ctx, &args, &success_result(), 50, &config);
        let decision = plugin.evaluate_pre(&ctx, &args, None, &config);
        if let GateDecision::Allow {
            modified_result, ..
        } = decision
        {
            assert!(modified_result.is_none());
        }
    }

    #[test]
    fn different_args_different_entries() {
        let plugin = ResponseCachePlugin::new(ResponseCacheConfig::default());
        let ctx = test_ctx("test_tool");
        let config = serde_json::json!({});

        let args1 = serde_json::json!({"q": "hello"});
        let args2 = serde_json::json!({"q": "world"});
        let result = success_result();

        plugin.evaluate_post(&ctx, &args1, &result, 50, &config);

        // Different args → cache miss
        let decision = plugin.evaluate_pre(&ctx, &args2, None, &config);
        if let GateDecision::Allow {
            modified_result, ..
        } = decision
        {
            assert!(modified_result.is_none());
        }
    }

    #[test]
    fn per_identity_isolation() {
        let plugin = ResponseCachePlugin::new(ResponseCacheConfig {
            cache_scope: "per_identity".to_owned(),
            ..Default::default()
        });
        let config = serde_json::json!({});
        let args = serde_json::json!({"q": "test"});
        let result = success_result();

        let mut ctx1 = test_ctx("test_tool");
        ctx1.identity.subject_id = Some("user1@test.com".to_owned());

        let mut ctx2 = test_ctx("test_tool");
        ctx2.identity.subject_id = Some("user2@test.com".to_owned());

        // Cache for user1
        plugin.evaluate_post(&ctx1, &args, &result, 50, &config);

        // user2 should miss
        let decision = plugin.evaluate_pre(&ctx2, &args, None, &config);
        if let GateDecision::Allow {
            modified_result, ..
        } = decision
        {
            assert!(modified_result.is_none());
        }

        // user1 should hit
        let decision = plugin.evaluate_pre(&ctx1, &args, None, &config);
        if let GateDecision::Allow {
            modified_result, ..
        } = decision
        {
            assert!(modified_result.is_some());
        }
    }

    #[test]
    fn errors_not_cached() {
        let plugin = ResponseCachePlugin::new(ResponseCacheConfig::default());
        let ctx = test_ctx("test_tool");
        let config = serde_json::json!({});
        let args = serde_json::json!({"q": "test"});
        let error_result = serde_json::json!({"content": [], "isError": true});

        plugin.evaluate_post(&ctx, &args, &error_result, 50, &config);

        // Should not be cached
        let decision = plugin.evaluate_pre(&ctx, &args, None, &config);
        if let GateDecision::Allow {
            modified_result, ..
        } = decision
        {
            assert!(modified_result.is_none());
        }
    }

    #[test]
    fn no_cache_meta_flag() {
        let plugin = ResponseCachePlugin::new(ResponseCacheConfig::default());
        let ctx = test_ctx("test_tool");
        let config = serde_json::json!({});
        let args = serde_json::json!({"q": "test"});

        // Populate cache
        plugin.evaluate_post(&ctx, &args, &success_result(), 50, &config);

        // With no_cache flag, should skip cache
        let meta = serde_json::json!({"no_cache": true});
        let decision = plugin.evaluate_pre(&ctx, &args, Some(&meta), &config);
        if let GateDecision::Allow {
            modified_result, ..
        } = decision
        {
            assert!(modified_result.is_none());
        }
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let v = serde_json::json!({"b": 2, "a": 1});
        let canonical = canonical_json(&v);
        assert_eq!(canonical, r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn argument_normalization_produces_same_key() {
        let args1 = serde_json::json!({"a": 1, "b": 2});
        let args2 = serde_json::json!({"b": 2, "a": 1});
        let key1 = cache_key("tool", "t", &args1, None);
        let key2 = cache_key("tool", "t", &args2, None);
        assert_eq!(key1, key2);
    }

    #[test]
    fn surface_distinguishes_cache_key() {
        // A tool and a prompt with the same name must not share a
        // cache entry.
        let args = serde_json::json!({"x": 1});
        let tool_key = cache_key("tool", "shared", &args, None);
        let prompt_key = cache_key("prompt", "shared", &args, None);
        assert_ne!(tool_key, prompt_key);
    }

    #[test]
    fn empty_config_yields_defaults() {
        // An empty / absent config block opts out (not a typo) and must
        // still produce the documented defaults.
        let plugin = ResponseCachePlugin::from_config_json("{}");
        let default = ResponseCacheConfig::default();
        assert_eq!(plugin.config.default_ttl_ms, default.default_ttl_ms);
        assert_eq!(plugin.config.max_entries, default.max_entries);
        assert_eq!(plugin.config.cache_scope, default.cache_scope);
        assert!(plugin.config.per_tool.is_empty());
    }

    #[test]
    #[should_panic(expected = "failing closed")]
    fn malformed_config_fails_closed() {
        // A present-but-malformed config must refuse the plugin rather
        // than silently degrading to defaults (fail-closed).
        let _ = ResponseCachePlugin::from_config_json("not json");
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        // A stray / typo'd / renamed config key must become a parse
        // error (deny_unknown_fields) so the plugin fails closed at boot
        // rather than silently ignoring the bad key.
        let cfg = serde_json::json!({
            "default_ttl_ms": 1000,
            "max_enries": 5, // typo for "max_entries"
        });
        assert!(ResponseCachePlugin::from_config(&cfg).is_err());
    }

    #[test]
    fn unknown_per_tool_key_is_rejected() {
        // Nested ToolCacheConfig is also strict.
        let cfg = serde_json::json!({
            "per_tool": [{
                "tools": ["foo_*"],
                "ttl_ms": 1000,
                "ttl": 1000, // unknown key
            }],
        });
        assert!(ResponseCachePlugin::from_config(&cfg).is_err());
    }
}
