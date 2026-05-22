# Models API: Dual-Mode Handler Design

**Date:** 2026-05-21 (revised)
**Status:** Draft
**Supersedes:** Previous single-mode Models API spec

## Overview

Add Models API endpoints (`GET /v1/models`, `GET /v1/models/{model}`) with two runtime modes selected by the `model_catalog` configuration option.

| Mode | `model_catalog` | Response format | Upstream call |
|------|-----------------|-----------------|---------------|
| 1 — Standard OpenAI | Empty (default) | `{ "object": "list", "data": [...] }` | Yes — Anthropic `GET /v1/models` |
| 2 — Codex Extended | One or more files | `{ "models": [ModelInfo...] }` | Yes — Anthropic `GET /v1/models` for filtering |

**Research entry:** `docs/protocol_research.md` section 13.

## Routing

### URL Pattern

Same convention as `/responses`:

| Codex CLI request | Proxy forwards to (Mode 1) |
|---|---|
| `GET /https/api.anthropic.com/models` | `GET https://api.anthropic.com/v1/models` |
| `GET /https/api.anthropic.com/models/claude-sonnet-4-20250514` | `GET https://api.anthropic.com/v1/models/claude-sonnet-4-20250514` |

Supports the same path normalization variants (`/https:/`, `/https://`, `/v1/` prefix).

### Route Parsing Rules

After stripping protocol prefix and normalizing, apply to the trailing segments:

| Path (after host) | Segments after `/models` | Route type |
|---|---|---|
| `.../models` | 0 | `ModelsList` |
| `.../models/` | 0 (trailing slash stripped) | `ModelsList` |
| `.../models/claude-sonnet-4-20250514` | 1 | `ModelsGet("claude-sonnet-4-20250514")` |
| `.../models/a/b` | 2+ | Reject (400) |

**Implementation logic:**
1. After extracting the host and stripping protocol prefixes, split remaining path on `/`.
2. If first meaningful segment is `models`:
   - No further segments (or only empty trailing slash) → `ModelsList`.
   - Exactly one further segment → `ModelsGet`, that segment is the `model_id`.
   - Two or more further segments → reject with 400 `invalid_request`.
3. If first meaningful segment is `responses` → existing `Responses` route.

### RouteInfo Extension

`RouteInfo::parse()` currently only accepts paths ending in `/responses`. Extend to recognize three route types:

```
RouteType enum:
  - Responses  (path ends with /responses)
  - ModelsList (path ends with /models or /models/)
  - ModelsGet  (path ends with /models/{model_id}, exactly one segment after /models)
```

`RouteInfo` gains a `route_type: RouteType` field and a `model_id: Option<String>` field.

Upstream URL builder methods:
- `upstream_models_list_url()` → `{base}/v1/models`
- `upstream_models_get_url(model_id)` → `{base}/v1/models/{model_id}`

### Router Registration

`build_router()` adds GET routes alongside the existing POST catch-all:

```rust
Router::new()
    .route("/{*path}", post(handle_responses))
    .route("/{*path}", get(handle_models))
    .with_state(state)
```

## Mode 1: Standard OpenAI Format

Activated when `model_catalog` is empty (default).

### Flow

1. **Auth check** — extract API key from `Authorization` header, return 401 if missing
2. **Route parsing** — determine list vs. get, extract model_id
3. **Build upstream request** — `GET` to Anthropic with `x-api-key` and `anthropic-version` headers
4. **Query parameter forwarding** — forward all query params to Anthropic (e.g. `?limit=20`, `?after_id=...`)
5. **Forward request** — reqwest client, 30-second timeout
6. **Convert response** — field mapping below
7. **Return** — `content-type: application/json`

### Field Mapping: Anthropic → OpenAI

**List models (`GET /models`):**

Anthropic response:
```json
{
  "data": [{ "type": "model", "id": "claude-sonnet-4-20250514", "display_name": "Claude Sonnet 4", "created_at": "2025-02-19T00:00:00Z" }],
  "has_more": false,
  "first_id": "...",
  "last_id": "..."
}
```

Converted response:
```json
{
  "object": "list",
  "data": [
    {
      "id": "claude-sonnet-4-20250514",
      "object": "model",
      "created": 1739923200,
      "owned_by": "anthropic"
    }
  ]
}
```

Field mapping per model object:

| Anthropic field | OpenAI field | Conversion |
|---|---|---|
| `id` | `id` | Direct passthrough |
| `type` (`"model"`) | `object` | Rename field |
| `created_at` (ISO 8601) | `created` (Unix timestamp) | Parse ISO 8601, convert to epoch seconds. On parse failure, use `0` and log warning |
| — | `owned_by` | Hardcoded `"anthropic"` |
| `display_name` | — | Discarded |

**Get model (`GET /models/{model}`):**

Anthropic response:
```json
{ "type": "model", "id": "claude-sonnet-4-20250514", "display_name": "Claude Sonnet 4", "created_at": "2025-02-19T00:00:00Z" }
```

Converted response:
```json
{
  "id": "claude-sonnet-4-20250514",
  "object": "model",
  "created": 1739923200,
  "owned_by": "anthropic"
}
```

### Pagination Handling

Anthropic returns `has_more`, `first_id`, `last_id` for cursor-based pagination. The standard OpenAI format does not include these fields.

**Behavior:**
1. If `has_more` is `true`, log `tracing::warn!` indicating the model list is truncated.
2. Return only models from the current page. Do not auto-paginate.

### Error Handling (Mode 1)

Reuses existing `convert_non_streaming_error()` from `src/conversion/error.rs`.

**Anthropic error mapping:**

| Anthropic error type | OpenAI error code | HTTP status |
|---|---|---|
| `not_found_error` | `model_not_found` | 404 |
| `authentication_error` | `invalid_api_key` | 401 |
| `permission_error` | `invalid_api_key` | 403 |
| `rate_limit_error` | `rate_limit_exceeded` | 429 |
| `invalid_request_error` | `invalid_request` | 400 |
| `overloaded_error` | `server_error` | 503 |
| `api_error` | `server_error` | 500 |
| `billing_error` | `insufficient_quota` | 402 |
| `request_too_large` | `request_too_large` | 413 |

**Network errors:**

| Error scenario | Response |
|---|---|
| Connection refused | 502 `server_error` "Upstream connection failed" |
| Connection timeout (30s) | 504 `server_error` "Upstream request timed out" |
| TLS handshake failure | 502 `server_error` "Upstream TLS error" |
| Non-JSON 200 body | 502 `server_error` "Invalid upstream response" |
| DNS resolution failure | 502 `server_error` "Upstream DNS resolution failed" |

All network errors logged with `tracing::error!`.

## Mode 2: Codex Extended Format

Activated when `model_catalog` has one or more files.

The proxy calls Anthropic upstream to discover which models are actually available, then filters the catalog against that live set. This ensures downstream clients only see models that exist and are accessible with the current API key.

### Flow

1. **Auth check** — same as Mode 1
2. **Route parsing** — same as Mode 1
3. **Upstream call** — call Anthropic to discover available models (see per-route detail below)
4. **Filter catalog** — keep only catalog models confirmed available upstream
5. **Return** — `content-type: application/json`

**List models (`GET /models`):**

1. Call Anthropic `GET /v1/models` to retrieve the set of available model IDs.
2. Load and merge all catalog files (existing merge logic unchanged).
3. Filter: keep only catalog models whose `slug` matches an `id` in the Anthropic response.
4. **If the filtered result is empty (no overlap between catalog slugs and upstream IDs)** → discard catalog, convert the Anthropic response to standard OpenAI format (same as Mode 1) and return `{ "object": "list", "data": [...] }`.
5. If the filtered result is non-empty → return `{ "models": [<ModelInfo>, ...] }` from the filtered catalog.
6. Models sorted by `priority` (ascending — lower = higher priority), stable by `slug`.

**Get model (`GET /models/{model}`):**

1. Call Anthropic `GET /v1/models/{model}` to check if the model exists upstream.
2. If Anthropic returns 404 → return 404 to downstream with standard OpenAI error format.
3. If Anthropic confirms the model exists → look up `slug` in the merged catalog.
4. If found in catalog → return `{ "models": [<ModelInfo>] }` with one element.
5. If NOT found in catalog → return the Anthropic response converted to standard OpenAI single-model format (same as Mode 1).

### Error Handling (Mode 2 — Upstream Failure)

When the Anthropic upstream call fails, the proxy returns an error to the downstream client. It must never silently return unfiltered catalog data.

| Upstream error scenario | Response |
|---|---|
| Network error (connection refused, timeout, DNS) | Log `tracing::error!`, return 502 `server_error` "Upstream request failed" |
| Anthropic 5xx | Log `tracing::error!`, return 502 `server_error` with upstream status code |
| Anthropic 401 | Return 401 to downstream (auth problem must propagate) |
| Anthropic 404 (`GET /models/{model}` only) | Return 404 to downstream |
| Non-JSON 200 body | 502 `server_error` "Invalid upstream response" |

### Response Format

```json
{
  "models": [
    {
      "slug": "claude-sonnet-4-20250514",
      "display_name": "Claude Sonnet 4",
      "description": "Balanced intelligence and speed",
      ...all ModelInfo fields...
    }
  ]
}
```

This is Codex CLI's `ModelsResponse { models: Vec<ModelInfo> }` format — see ModelInfo field reference below.

### Remaining Error Scenarios (Mode 2 — Non-Upstream)

| Scenario | Response |
|---|---|
| Invalid path (extra segments) | 400 — same as Mode 1 |
| Missing auth | 401 — same as Mode 1 |

Startup validation errors (see Config section) cause the proxy to exit with an error message.

## Config: model_catalog

### Field Definition

Add to `AppConfig`:

```rust
#[serde(default)]
pub model_catalog: Vec<std::path::PathBuf>,
```

### Config File Format

In YAML, accept either a string or an array of strings:

```yaml
# Single file
model_catalog: "./models.json"

# Multiple files (higher priority first)
model_catalog:
  - "./overrides/models.json"
  - "./base/models.json"
```

Default: `[]` (empty — activates Mode 1).

### Path Resolution

Relative paths are resolved relative to the config file's parent directory. Absolute paths are used as-is.

### Catalog File Format

JSON or YAML (parse as YAML — it is a superset of JSON). Schema matches Codex CLI's `ModelsResponse`:

```json
{
  "models": [
    {
      "slug": "claude-sonnet-4-20250514",
      "display_name": "Claude Sonnet 4",
      "description": "Balanced intelligence and speed",
      ...all required ModelInfo fields...
    }
  ]
}
```

### Startup Validation

The following conditions cause the proxy to print an error to stderr and exit with code 1:

| Condition | Error message |
|---|---|
| Catalog file does not exist | `model_catalog file not found: {path}` |
| Catalog file has parse error | `model_catalog parse error in {path}: {details}` |
| Merged catalog has zero models | `model_catalog has no models after merging all files` |

Validation runs at startup after config loading. No hot-reload — restart to pick up changes.

## Multi-File Merge Rules

When `model_catalog` contains multiple files, they are merged in config order (first file = highest priority).

### Algorithm

1. Start with an empty `HashMap<String, ModelInfo>` keyed by `slug`.
2. Process files in reverse order (last file first), building the initial map.
3. Then process files in forward order (first file = highest priority), overlaying each file's entries onto the map.

Equivalently, for each unique slug, the final value is built by starting with the last file's entry and then overlaying each earlier file's entry in reverse config order.

### Field-Level Merge for Same Slug

**Constraint:** Each catalog file must be a valid `ModelsResponse` on its own — all fields without `#[serde(default)]` must be present in every file. The merge overlay only adjusts fields that have serde defaults or are `Option<T>`.

When two files define the same `slug`:

1. **Non-`Option` scalar fields** (String, bool, i32, enum): earlier file's value wins if present. No fallback.
2. **`Option<T>` fields**: if the earlier file's value is `null` or the field is absent, fall through to the later file's value. If the earlier file provides a non-null value, it wins.
3. **Vec fields**: earlier file's entire Vec replaces later file's Vec. No element-level merge.
4. **Nested object fields**: earlier file's entire object replaces later file's object. Same `Option` rules apply at the nested level — if the earlier file's nested field is `null`/absent, the later file's nested value is used.

### Example

File 1 (high priority):
```json
{
  "models": [{
    "slug": "claude-sonnet-4-20250514",
    "display_name": "Sonnet 4 (custom)",
    "base_instructions": "Use concise responses",
    "context_window": 200000
  }]
}
```

File 2 (base):
```json
{
  "models": [{
    "slug": "claude-sonnet-4-20250514",
    "display_name": "Claude Sonnet 4",
    "base_instructions": "",
    "supports_reasoning_summaries": true,
    "context_window": 180000
  }]
}
```

Merged result:
- `display_name`: `"Sonnet 4 (custom)"` — File 1 wins
- `base_instructions`: `"Use concise responses"` — File 1 wins
- `supports_reasoning_summaries`: `true` — File 2 value (File 1 absent → falls through)
- `context_window`: `200000` — File 1 wins

## ModelInfo Field Reference

The catalog format uses Codex CLI's `ModelInfo` struct from `codex-rs/protocol/src/openai_models.rs`.

### Required Fields (no serde default, must be present)

| Field | Type | Notes |
|---|---|---|
| `slug` | String | Model identifier, used for deduplication in merge |
| `display_name` | String | Human-readable name |
| `description` | Option\<String\> | |
| `supported_reasoning_levels` | Vec\<ReasoningEffortPreset\> | Each: `{ "effort": String, "description": String }` |
| `shell_type` | ConfigShellToolType | `"default"` \| `"local"` \| `"unified_exec"` \| `"disabled"` \| `"shell_command"` |
| `visibility` | ModelVisibility | `"list"` \| `"hide"` \| `"none"` |
| `supported_in_api` | bool | |
| `priority` | i32 | Lower = higher priority |
| `base_instructions` | String | |
| `supports_reasoning_summaries` | bool | |
| `support_verbosity` | bool | |
| `default_verbosity` | Option\<Verbosity\> | `"low"` \| `"medium"` \| `"high"` |
| `apply_patch_tool_type` | Option\<ApplyPatchToolType\> | `"freeform"` or null |
| `truncation_policy` | TruncationPolicyConfig | `{ "mode": "bytes"\|"tokens", "limit": i64 }` |
| `supports_parallel_tool_calls` | bool | |
| `experimental_supported_tools` | Vec\<String\> | |

### Optional Fields (have serde defaults)

| Field | Type | Default |
|---|---|---|
| `default_reasoning_summary` | ReasoningSummary | `"auto"` — `"auto"` \| `"none"` \| `"concise"` \| `"detailed"` |
| `default_reasoning_level` | Option\<ReasoningEffort\> | `None` — `"none"` \| `"minimal"` \| `"low"` \| `"medium"` \| `"high"` \| `"xhigh"` |
| `additional_speed_tiers` | Vec\<String\> | `[]` |
| `service_tiers` | Vec\<ModelServiceTier\> | `[]` — each: `{ "id": String, "name": String, "description": String }` |
| `supports_image_detail_original` | bool | `false` |
| `context_window` | Option\<i64\> | `None` |
| `max_context_window` | Option\<i64\> | `None` |
| `auto_compact_token_limit` | Option\<i64\> | `None` |
| `effective_context_window_percent` | i64 | `95` |
| `input_modalities` | Vec\<InputModality\> | `["text", "image"]` |
| `supports_search_tool` | bool | `false` |
| `web_search_tool_type` | WebSearchToolType | `"text"` — `"text"` \| `"text_and_image"` |
| `availability_nux` | Option\<ModelAvailabilityNux\> | `None` — `{ "message": String }` |
| `upgrade` | Option\<ModelInfoUpgrade\> | `None` — `{ "model": String, "migration_markdown": String }` |
| `model_messages` | Option\<ModelMessages\> | `None` — `{ "instructions_template": Option\<String\>, ... }` |

## Handler Dispatch Logic

```rust
async fn handle_models(State(state): State<AppState>, req: Request) -> Result<Response<Body>, Error> {
    // 1. Auth check (both modes)
    // 2. Route parsing (both modes)
    // 3. Dispatch:
    if state.config.model_catalog.is_empty() {
        handle_models_standard(&state, route_info).await  // Mode 1
    } else {
        handle_models_catalog(&state, route_info).await   // Mode 2 — also async (calls upstream)
    }
}
```

Both mode handlers are separate async functions. Shared logic (auth, route parsing, error formatting) stays in `handle_models`. Mode 2's `handle_models_catalog` calls Anthropic upstream and then filters the merged catalog against the upstream response.

## Files Changed

| File | Change |
|---|---|
| `src/config.rs` | Add `model_catalog: Vec<PathBuf>` to `AppConfig` |
| `src/router.rs` | Add `RouteType` enum, extend `RouteInfo::parse()`, add `handle_models` handler with mode dispatch |
| `src/conversion/models.rs` | New: Mode 1 conversion (`anthropic_to_openai_model`, `anthropic_list_to_openai_list`), Mode 2 upstream filter logic, minimal ModelInfo generation |
| `src/catalog.rs` | New: catalog loading, multi-file merge, `ModelInfo` types |
| `src/conversion/mod.rs` | Add `pub mod models;` |
| `src/lib.rs` | No change needed |

## Testing

### Mode 1 Tests (Integration: `tests/models.rs`)

1. **List models** — returns `{ "object": "list", "data": [{ "id", "object", "created", "owned_by" }] }`
2. **Get model** — returns `{ "id", "object", "created", "owned_by" }`
3. **Get nonexistent model** — 404 with `model_not_found`
4. **Auth required** — 401 without API key
5. **Field correctness** — `type`→`object`, `id`→`id`, ISO 8601→Unix timestamp, `owned_by: "anthropic"`
6. **Pagination truncation** — `has_more: true` triggers warning log, returns partial list
7. **Invalid route** — `/models/{id}/extra` returns 400
8. **Query param forwarding** — `?limit=20` forwarded to upstream

### Mode 2 Tests (Integration: `tests/models.rs`)

1. **List models — filtered by upstream** — returns `{ "models": [ModelInfo...] }` containing only catalog models whose `slug` exists in the Anthropic upstream response
2. **List models — upstream 5xx returns error** — when Anthropic returns 5xx, downstream receives 502 `server_error`
3. **List models — upstream network error returns error** — when Anthropic is unreachable, downstream receives 502 `server_error`
4. **List models — upstream 401 propagated** — when Anthropic returns 401, downstream receives 401
5. **Get model — found in catalog** — returns `{ "models": [single ModelInfo] }` for slug that exists upstream and in catalog
6. **Get model — upstream 404** — returns 404 with `model_not_found` when Anthropic says model doesn't exist
7. **Get model — exists upstream but not in catalog** — returns minimal generated `ModelInfo` with `slug` = Anthropic `id`
8. **Auth required** — 401 without API key
9. **All required fields present** — every ModelInfo in response has all 18 required fields
10. **Multi-file merge: different slugs** — union of all models across files (filtered by upstream)
11. **Multi-file merge: same slug overlay** — earlier file's non-null fields override later file's
12. **Multi-file merge: Option fallthrough** — absent/null in earlier file falls through to later file
13. **Priority ordering** — models sorted by `priority` ascending

### Config Validation Tests (Unit: `src/catalog.rs`)

1. Parse valid catalog file with all required fields
2. Missing file → startup error
3. Malformed JSON → startup error with parse details
4. Empty models array → startup error
5. Single string config → converted to single-element Vec
6. Relative path resolution → resolved relative to config file parent

### Mode 1 Unit Tests (`src/conversion/models.rs`)

1. `anthropic_to_openai_model` — correct field mapping for single model
2. `anthropic_list_to_openai_list` — wraps in `{ "object": "list", "data": [...] }`
3. Timestamp conversion: valid ISO 8601 → Unix epoch
4. Timestamp conversion: invalid string → `0` with warning log

### Mode 2 Unit Tests (`src/catalog.rs`)

1. Single file load and parse
2. Two files, different slugs → union
3. Two files, same slug → field overlay
4. Three files, mixed same/different slugs
5. Empty optional field in earlier file → falls through to later file
6. Array fields → complete replacement, no element merge
