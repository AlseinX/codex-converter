# codex-conv

将 OpenAI Responses API 与 Anthropic Messages API 互相转换的反向代理，让 [Codex CLI](https://github.com/openai/codex) 等客户端无缝对接任何 Anthropic 兼容的上游服务。

[English](README.md)

## 快速开始

无需任何配置，直接运行：

```bash
codex-conv
```

代理默认监听 `0.0.0.0:8080`。然后配置 Codex CLI：

### 1. 编辑 `~/.codex/config.toml`

```toml
model = "claude-sonnet-4-20250514"
model_provider = "anthropic-proxy"

[model_providers.anthropic-proxy]
name = "Anthropic via codex-conv"
base_url = "http://localhost:8080/https/api.anthropic.com"
```

### 2. 编辑 `~/.codex/auth.json`

```json
{
  "auth_mode": "api-key",
  "OPENAI_API_KEY": "sk-ant-..."
}
```

代理从 URL 路径中提取上游地址（`/https/<host>/responses` → `https://<host>/v1/messages`），并转发 `Authorization` 头中的 API 密钥。

## 代理配置

配置优先级从高到低：

1. CLI `-C` 覆盖项
2. CLI 便捷参数（`--listen`、`--upstream-tls-extra-ca-certs`）
3. 环境变量（`CODEX_CONV_` 前缀）
4. YAML 配置文件（`-c`）
5. 代码默认值

### CLI 参数

```
codex-conv [OPTIONS]

选项:
  -c, --config-file <PATH>                  YAML 配置文件路径
  -C, --config-item <KEY=VALUE>             覆盖配置项（可重复使用）
      --listen <ADDR:PORT>                  监听地址（覆盖 server.listen）
      --upstream-tls-extra-ca-certs <PATH>  上游 TLS 额外 CA 证书
  -h, --help                                显示帮助
  -V, --version                             显示版本
```

示例：

```bash
# 指定监听端口
codex-conv --listen 127.0.0.1:9090

# 使用配置文件
codex-conv -c /etc/codex-conv/config.yaml

# 覆盖单个配置项
codex-conv -C server.listen=0.0.0.0:9999 -C log.console.level=debug
```

### YAML 配置文件

```yaml
server:
  listen: "0.0.0.0:8080"       # 监听地址
  shutdown_timeout: 30          # 优雅关闭超时（秒）
  tls:                          # 留空则使用 HTTP
    cert: "/path/to/cert.pem"
    key: "/path/to/key.pem"

upstream:
  anthropic_version: "2023-06-01"
  proxy: ""                     # 上游 HTTP 代理地址，留空则直连
  tls:
    use_system_roots: true      # 使用系统 CA 证书
    extra_ca_certs:             # 额外 CA 证书路径
      - /path/to/ca.pem

log:
  console:
    level: "info"               # 日志级别: trace, debug, info, warn, error
  file:                         # 留空则不写文件日志
    level: "debug"
    dir: "./logs"
    rotation: "daily"

model_catalog: []               # 模型目录文件路径（字符串或数组）
```

### 环境变量

所有配置项均可通过 `CODEX_CONV_<SECTION>_<FIELD>` 覆盖：

```bash
CODEX_CONV_SERVER_LISTEN=0.0.0.0:9999
CODEX_CONV_UPSTREAM_ANTHROPIC_VERSION=2023-06-01
CODEX_CONV_LOG_CONSOLE_LEVEL=debug
```

## URL 路由

代理通过 URL 路径决定上游地址和请求类型，支持 HTTP 和 HTTPS 上游：

```
http://<proxy>/https/<host>/responses          → POST https://<host>/v1/messages
http://<proxy>/https/<host>/models             → GET  https://<host>/v1/models
http://<proxy>/https/<host>/models/<model-id>  → GET  https://<host>/v1/models/<model-id>
http://<proxy>/http/<host>/responses           → POST http://<host>/v1/messages
```

`/http/` 用于 HTTP 上游（如反向代理链）。

## Models API 双模式

### Mode 1：标准转换（默认）

不配置 `model_catalog` 时，代理将 Anthropic Models API 响应转换为 OpenAI 格式：

```json
{
  "object": "list",
  "data": [
    {"id": "claude-sonnet-4-20250514", "object": "model", "owned_by": "anthropic", "created": 1739923200}
  ]
}
```

### Mode 2：目录过滤

配置 `model_catalog` 后，代理将目录与上游可用模型取交集，返回 Codex 目录格式：

```yaml
model_catalog: /path/to/catalog.json
```

目录文件示例：

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

支持多个目录文件（前者优先级更高），路径相对于配置文件所在目录。

## TLS

**服务端：** 设置 `server.tls.cert` 和 `server.tls.key` 启用 HTTPS。

**上游：** 默认使用系统 CA 证书。自定义证书通过 `upstream.tls.extra_ca_certs` 添加。

## 许可证

在 [Apache License 2.0](LICENSE) 或 [MIT License](LICENSE) 中任选其一。

Copyright 2026 AlseinX \<xyh951115@live.com\>
