# Response Cache — `dev.mcpg.response-cache`

> class `tool_gate` · `native` · package `mcpg-plugin-reliability-response-cache` · artifact `libmcpg_plugin_reliability_response_cache.so` · Apache-2.0

Short-lived in-memory TTL cache for MCP tool results. It keys on the tool name
and a canonical hash of the call arguments, and returns a hit as a pre-dispatch
`Allow` carrying the stored result — which makes the gateway skip the backend
entirely. Reach for it when agents re-issue the same idempotent call within
seconds of each other and the downstream system is slow, rate-limited, or billed
per request.

## What it does
- Stores successful results on post-dispatch and serves them on pre-dispatch
  until the entry's TTL expires.
- Keys each entry on `(surface, name, canonical(arguments))`, plus the caller's
  subject id when `cache_scope` is `per_identity`. Argument JSON is canonicalised
  with object keys sorted recursively, so `{"a":1,"b":2}` and `{"b":2,"a":1}`
  hit the same entry.
- Carries the MCP surface in the key, so a tool and a prompt of the same name
  can never collide.
- Never caches errors — a result with `isError: true` is passed through and
  discarded.
- Honours a per-request opt-out: `_meta.no_cache: true` bypasses the lookup.
- Supports per-tool TTL overrides by glob, with `ttl_ms: 0` disabling caching
  for the matched tools entirely.
- Bounds itself at `max_entries`, evicting expired entries first and then the
  single oldest entry.
- Declares no host capabilities and opens no sockets.

## Configuration
Loaded from the flat top-level `plugins:` list. Every entry of class `tool_gate`
joins the gate chain the gateway evaluates before dispatch; on a hit this plugin
returns an `Allow` whose `modified_result` makes the tool-call path skip the
backend. Only the tool-call path consumes `modified_result` and reports results
back post-dispatch, so caching is effectively scoped to `tools/call`.

```yaml
plugins:
  - id: dev.mcpg.response-cache
    class: tool_gate
    source: { path: ./plugins/libmcpg_plugin_reliability_response_cache.so }
    # or, platform-agnostic — the gateway resolves the artifact for its own
    # os/arch/libc at boot:
    # source: { oci: ghcr.io/mcpg-dev/source-code/plugins/response-cache:protocol-1 }
    config:
      default_ttl_ms: 300000          # 5 minutes
      max_entries: 10000
      cache_scope: shared             # shared | per_identity
      per_tool:
        - tools: ["catalog.*"]        # glob patterns
          ttl_ms: 60000
        - tools: ["live.*", "*.stream"]
          ttl_ms: 0                   # never cache these
```

| Field | Type | Default | Description |
|---|---|---|---|
| `default_ttl_ms` | integer | `300000` | Entry lifetime in milliseconds for names no `per_tool` rule matches. |
| `max_entries` | integer | `10000` | Capacity before eviction runs. |
| `per_tool` | object[] | `[]` | Overrides: `tools` (glob patterns) and `ttl_ms`; `0` disables caching for the match. |
| `cache_scope` | string | `"shared"` | `"shared"` keys entries by arguments alone; `"per_identity"` adds the caller's subject id. |

Unknown fields are rejected, at the top level and inside `per_tool` entries. An
absent or empty `config:` block yields the defaults above; a present-but-malformed
block refuses the plugin at boot rather than quietly degrading to defaults.

The first `per_tool` rule with a matching pattern wins, and its `ttl_ms` replaces
`default_ttl_ms` for that name. Patterns use `*` for any run of characters and
`?` for exactly one.

## Security
**`cache_scope` is an exact string match.** Only the literal `"per_identity"`
enables identity scoping; every other value, including a near miss like
`per-identity`, behaves as `shared`. Confirm the value before relying on it to
separate tenants.

**Identity scoping needs a subject.** Under `per_identity` the key is extended
with the caller's subject id. Callers without one — anonymous or unauthenticated
traffic — contribute no identity component, so they share entries with each
other. Pair `per_identity` with an identity provider that always resolves a
subject, or keep per-user data out of cached tools.

**A hit skips the backend.** The gateway's own pre-dispatch authorization — trust
floor, CEL, and the policy-engine chain — still runs, because it is evaluated
before the gate chain. What does not run is anything the backend itself would
enforce per call: row filters, downstream permissions, freshness. Cache tools
whose results do not vary by caller, or scope them with `per_identity`.

## Operations
Entries live in the plugin instance, so the cache is per gateway process: N
replicas hold N independent caches, and a restart starts cold. `max_entries` is
therefore a per-replica bound.

Eviction is opportunistic and triggered by a write once the entry count reaches
`max_entries`: expired entries are dropped first, and if the cache is still at
capacity the single oldest entry by insertion time is removed. Expired entries
are also dropped lazily on lookup.

## Observability
- `mcpg_response_cache_total{tool,result}` — `result` is `hit` or `miss`.
- `mcpg_response_cache_evaluate_ms` — pre-dispatch evaluation latency.

Each evaluation opens a `response_cache_evaluate_pre` tracing span tagged with
the plugin id and name. A hit also returns `metadata: {"cache": "hit"}` on the
decision.

## Build
The `cdylib-export` feature gates the `mcpg_plugin_register` export. It is on by
default for a standalone build and switched off when the crate is linked as a
path dependency alongside other plugins, since several `mcpg_plugin_register`
symbols collide at link time:

```bash
cargo build -p mcpg-plugin-reliability-response-cache --features cdylib-export --release   # → target/release/libmcpg_plugin_reliability_response_cache.so
```

## Sign & load (production)
Sign the artifact, pin/verify via the entry's `signature:` block, and honour
revocations. See <https://mcpg.dev/docs/security/plugin-security>.

## See also
- Plugin classes, loading, and the ABI: <https://mcpg.dev/docs/plugins/plugins-and-protocol>
- Full gateway config schema: <https://mcpg.dev/docs/reference/configuration>
- Sibling reliability gates: `libs/plugins/reliability/circuit-breaker`,
  `libs/plugins/reliability/rate-limit`
