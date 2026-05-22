# codex-conv

Reverse proxy that translates between the OpenAI Responses API and the Anthropic Messages API, enabling clients like [Codex CLI](https://github.com/openai/codex) to work with any Anthropic-compatible upstream seamlessly.

[中文文档](README.zh-CN.md)

## Quick Start

No configuration required — just run:

```bash
codex-conv
```

The proxy listens on `0.0.0.0:8080` by default. Then configure Codex CLI:

### 1. Edit `~/.codex/config.toml`

```toml
model = "claude-sonnet-4-20250514"
model_provider = "anthropic-proxy"

[model_providers.anthropic-proxy]
name = "Anthropic via codex-conv"
base_url = "http://localhost:8080/https/api.anthropic.com"
```

### 2. Edit `~/.codex/auth.json`

```json
{
  "auth_mode": "api-key",
  "OPENAI_API_KEY": "sk-ant-..."
}
```

The proxy extracts the upstream host from the URL path (`/https/<host>/responses` → `https://<host>/v1/messages`) and forwards the API key from the `Authorization` header.

## Comparison

All projects below convert OpenAI Responses API → Anthropic Messages API.

| | codex-conv | [CLIProxyAPI] | [AnthMorph] | [codex-bridge] | [rosetta-llm] |
|---|---|---|---|---|---|
| MCP namespace mapping | Yes | Yes | Partial¹ | No | No |
| 64-char name truncation | Yes | Yes | Yes | No | No |
| apply_patch | Yes | No | No | No | No |
| Models API | Yes | Yes | No | No | Yes |
| Model catalog | Full Codex¹ | No² | No | No | Standard OpenAI³ |
| Language | Rust | Go | Rust | Python | Python |

¹ AnthMorph: uses SHA1 hash suffix for name shortening, but targets Chat Completions API — does not handle OpenAI Responses API `type: "namespace"` tool definitions.
¹ Full Codex: complete Codex CLI `ModelInfo` spec (all 32 fields), intersected with actual upstream model availability.
² CLIProxyAPI: returns hardcoded OpenAI model templates (gpt-5.5, gpt-5.4, etc.) — these models do not exist on Anthropic upstream.
³ Standard OpenAI: basic model list with `id`, `object`, `owned_by`, `created` only — no Codex-specific fields.

[CLIProxyAPI]: https://github.com/router-for-me/CLIProxyAPI
[AnthMorph]: https://github.com/DioNanos/AnthMorph
[codex-bridge]: https://github.com/nicholasyangyang/codex-bridge
[rosetta-llm]: https://github.com/Lokesh-Chimakurthi/rosetta-llm

## Proxy Configuration

Configuration priority (highest to lowest):

1. CLI `-C` overrides
2. CLI convenience flags (`--listen`, `--upstream-tls-extra-ca-certs`)
3. Environment variables (`CODEX_CONV_` prefix)
4. YAML config file (`-c`)
5. Code defaults

### CLI

```
codex-conv [OPTIONS]

Options:
  -c, --config-file <PATH>                  YAML config file path
  -C, --config-item <KEY=VALUE>             Override config item (repeatable)
      --listen <ADDR:PORT>                  Listen address (overrides server.listen)
      --upstream-tls-extra-ca-certs <PATH>  Extra CA certs for upstream TLS
  -h, --help                                Show help
  -V, --version                             Show version
```

Examples:

```bash
# Custom listen port
codex-conv --listen 127.0.0.1:9090

# With config file
codex-conv -c /etc/codex-conv/config.yaml

# Override individual fields
codex-conv -C server.listen=0.0.0.0:9999 -C log.console.level=debug
```

### YAML Config

```yaml
server:
  listen: "0.0.0.0:8080"       # Listen address
  shutdown_timeout: 30          # Graceful shutdown timeout (seconds)
  tls:                          # Omit to use plain HTTP
    cert: "/path/to/cert.pem"
    key: "/path/to/key.pem"

upstream:
  anthropic_version: "2023-06-01"
  proxy: ""                     # Upstream HTTP proxy, empty for direct
  tls:
    use_system_roots: true      # Use system CA store
    extra_ca_certs:             # Additional CA certificate paths
      - /path/to/ca.pem

log:
  console:
    level: "info"               # trace, debug, info, warn, error
  file:                         # Omit to disable file logging
    level: "debug"
    dir: "./logs"
    rotation: "daily"

model_catalog: []               # Model catalog file path(s), string or array
```

### Environment Variables

All config fields can be overridden with `CODEX_CONV_<SECTION>_<FIELD>`:

```bash
CODEX_CONV_SERVER_LISTEN=0.0.0.0:9999
CODEX_CONV_UPSTREAM_ANTHROPIC_VERSION=2023-06-01
CODEX_CONV_LOG_CONSOLE_LEVEL=debug
```

## URL Routing

The proxy determines the upstream host and request type from the URL path. Both HTTP and HTTPS upstreams are supported:

```
http://<proxy>/https/<host>/responses          → POST https://<host>/v1/messages
http://<proxy>/https/<host>/models             → GET  https://<host>/v1/models
http://<proxy>/https/<host>/models/<model-id>  → GET  https://<host>/v1/models/<model-id>
http://<proxy>/http/<host>/responses           → POST http://<host>/v1/messages
```

The `/http/` scheme enables chaining behind another reverse proxy over plain HTTP.

## Models API

### Mode 1: Passthrough Conversion (default)

Without `model_catalog`, the proxy converts Anthropic model list responses to OpenAI format:

```json
{
  "object": "list",
  "data": [
    {"id": "claude-sonnet-4-20250514", "object": "model", "owned_by": "anthropic", "created": 1739923200}
  ]
}
```

### Mode 2: Catalog Filtering

With `model_catalog` configured, the proxy intersects the catalog with upstream availability and returns Codex catalog format:

```yaml
model_catalog: /path/to/catalog.json
```

Catalog file example:

```json
{
  "models": [
    {
      "slug": "claude-sonnet-4-20250514",
      "display_name": "Claude Sonnet 4",
      "supported_in_api": true,
      "priority": 1,
      "apply_patch_tool_type": "freeform",
      "context_window": 200000
    }
  ]
}
```

Multiple catalog files are supported (earlier files have higher priority). Paths are resolved relative to the config file directory.

## TLS

**Server:** Set `server.tls.cert` and `server.tls.key` to enable HTTPS on the listening port.

**Upstream:** Uses system CA roots by default. Add custom certificates via `upstream.tls.extra_ca_certs`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE) or [MIT License](LICENSE) at your option.

Copyright 2026 AlseinX \<xyh951115@live.com\>
