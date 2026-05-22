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

All projects below convert OpenAI Responses API → Anthropic Messages API (Responses→Anthropic direction).

| | codex-conv | [CLIProxyAPI] | [Headroom] | [codex-bridge] | [rosetta-llm] | [AnthMorph] |
|---|---|---|---|---|---|---|
| Tool calls | Yes | Partial¹ | Yes | Yes | Partial² | Partial² |
| apply_patch | Yes | No | No | No | No | No |
| Models API | Yes | Yes | Yes | No | Yes | No |
| Model catalog | Yes | Partial³ | Yes | No | Yes | No |
| Upstream chaining | Yes | No | Yes | No | No | No |
| Language | Rust | Go | Rust+Python | Python | Python | Rust |

¹ CLIProxyAPI: known bugs in tool call conversion (see [issue #736](https://github.com/router-for-me/CLIProxyAPI/issues/736)).
² Partial: basic function call mapping only; streaming tool-use reconstruction not fully handled.
³ CLIProxyAPI: model aliasing only, no catalog filtering.

[CLIProxyAPI]: https://github.com/router-for-me/CLIProxyAPI
[Headroom]: https://github.com/chopratejas/headroom
[codex-bridge]: https://github.com/nicholasyangyang/codex-bridge
[rosetta-llm]: https://github.com/Lokesh-Chimakurthi/rosetta-llm
[AnthMorph]: https://github.com/DioNanos/AnthMorph

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
