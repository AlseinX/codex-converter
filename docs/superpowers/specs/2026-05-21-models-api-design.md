# Models API Conversion Design

**Date:** 2026-05-21
**Status:** Draft

## Overview

Add support for the OpenAI Models API (`GET /v1/models`, `GET /v1/models/{model}`) to the codex-conv proxy. Codex CLI uses these endpoints to list available models and verify model availability. The proxy forwards requests to Anthropic's Models API and converts the response format.

**Research entry:** See `docs/protocol_research.md` section 13 for verified field mappings and API references.

## Scope

- `GET /models` — list models
- `GET /models/{model}` — retrieve single model

Both are non-streaming, synchronous GET requests. No SSE conversion needed.

## Routing

### URL Pattern

Mirrors the existing `/responses` route pattern:

| Codex CLI request | Proxy forwards to |
|---|---|
| `GET /https/api.anthropic.com/models` | `GET https://api.anthropic.com/v1/models` |
| `GET /https/api.anthropic.com/models/claude-sonnet-4-20250514` | `GET https://api.anthropic.com/v1/models/claude-sonnet-4-20250514` |

Also supports the same path normalization variants (`/https:/`, `/https://`, `/v1/` prefix).

### RouteInfo Extension

`RouteInfo::parse()` currently only accepts paths ending in `/responses`. Extend it to recognize three route types:

```
RouteType enum:
  - Responses  (path ends with /responses)
  - ModelsList (path ends with /models)
  - ModelsGet  (path ends with /models/{model_id})
```

`RouteInfo` gains a `route_type: RouteType` field and a `model_id: Option<String>` field.

The upstream URL builder gains methods:
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

Axum allows registering the same path with different methods.

## Response Conversion

### Anthropic → OpenAI Field Mapping

**Per-model object:**

| Anthropic | OpenAI | Conversion |
|---|---|---|
| `type: "model"` | `object: "model"` | Rename key |
| `id` | `id` | Passthrough |
| `created_at` (ISO 8601 string) | `created` (integer, Unix seconds) | Parse and convert |
| — | `owned_by` | Default `"anthropic"` |
| `display_name` | — | Discard |

**List response wrapper:**

| Anthropic | OpenAI | Conversion |
|---|---|---|
| `data` (array) | `data` (array) | Convert each element |
| — | `object: "list"` | Add |
| `has_more` | — | Discard |
| `first_id` | — | Discard |
| `last_id` | — | Discard |

**Timestamp conversion:** Parse ISO 8601 string (e.g., `"2025-05-14T00:00:00Z"`) to Unix timestamp integer. If parsing fails, use `0` as fallback (matches OpenAI's convention for unknown dates).

### Implementation

New module: `src/conversion/models.rs`

```rust
/// Convert Anthropic model object to OpenAI model object.
fn convert_model(anthropic_model: &serde_json::Value) -> serde_json::Value

/// Convert Anthropic list models response to OpenAI list models response.
fn convert_models_list(anthropic_response: &serde_json::Value) -> serde_json::Value
```

Both functions are pure transformations — no I/O, no state.

## Handler Flow

`handle_models` handler:

1. **Auth check** — Same as `handle_responses`: extract API key from `Authorization` header, return 401 if missing
2. **Route parsing** — Parse path to determine list vs. get, extract model_id if present
3. **Build upstream request** — `GET` to Anthropic with `x-api-key` and `anthropic-version` headers
4. **Forward request** — Use reqwest client (same TLS/proxy config as responses handler)
5. **Convert response** — Apply field mapping
6. **Return** — JSON response with `content-type: application/json`

Error handling: If Anthropic returns 404 for a model ID, forward the 404 to the client with OpenAI-style error format. If Anthropic returns any other error, forward the status code and convert the error body to OpenAI format.

## Files Changed

| File | Change |
|---|---|
| `src/router.rs` | Add `RouteType` enum, extend `RouteInfo::parse()`, add `handle_models` handler |
| `src/conversion/models.rs` | New module: `convert_model()`, `convert_models_list()` |
| `src/conversion/mod.rs` | Add `pub mod models;` |
| `src/lib.rs` | No change needed |

## Testing

Integration tests (`tests/models.rs`):

1. **List models** — Proxy returns OpenAI-format model list with correct field mapping
2. **Get model** — Proxy returns single model in OpenAI format
3. **Get nonexistent model** — Proxy returns 404
4. **Auth required** — Models endpoints require API key, return 401 without
5. **Field conversion** — Verify `type`→`object`, `created_at`→`created`, `owned_by` added

Unit tests in `src/conversion/models.rs`:

1. `convert_model` — ISO 8601 → Unix timestamp, field renaming
2. `convert_models_list` — Array mapping, wrapper fields
3. Edge cases: missing fields, malformed timestamps
