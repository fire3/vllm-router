# Coding Agent 会话粘性：设计、实现与开发文档

> 分支：`feat/coding-agent-session-affinity`（基于 `main` @ `1d10e71`）
> 功能：为 `consistent_hash` 补充主流 coding agent 的会话标识支持；支持 JSON 配置文件补充 header 名单；在 header/body 都没有会话标识时回退到**首轮 user prompt hash**，保证多轮 coding agent 会话保持粘性。
>
> 背景调研见 `notes/coding-agent-session-key-research.md`；策略行为分析见 `notes/consistent-hash-main-analysis.md`。

## 1. 要解决的问题

`consistent_hash` 通过一个稳定的路由 key 把同一会话钉到同一 worker。旧实现（`main`）只认识：

```text
HTTP header: x-session-id / x-user-id / x-tenant-id / x-correlation-id / x-request-id / x-trace-id
body:        session_params.session_id / user / session_id / user_id
fallback:    整包 body hash
```

这对 coding agent 流量有三个问题：

1. Claude Code 的 `x-claude-code-session-id`、OpenCode 的 `X-Session-Id` / `x-session-affinity`、Codex CLI / Pi 的 `session-id` / `thread-id`、Cline / Roo 的 `session_id` 等都不在默认名单里；
2. Body 中的会话标识是**协议私有的嵌套字段**（Anthropic `metadata.user_id` 的 `_session_<uuid>` 后缀、Responses API `client_metadata.session_id/thread_id`、`prompt_cache_key`），旧文本扫描不识别，还可能在 prompt 内容里误命中 `"user":` / `"session_id":`；
3. 每轮请求都会携带**增长的完整对话历史**，整包 body hash 每轮都变，多轮会话天然失去粘性。

## 2. 实现概览

本分支新增：

- 配置类型 `SessionAffinityConfig`（`src/config/session_affinity.rs`），可 JSON 序列化/反序列化；
- `PolicyConfig::ConsistentHash` 新增 `session_config` 字段（带 serde default，向后兼容旧 JSON）；
- `ConsistentHashPolicy` 保存 `SessionAffinityConfig`，选路时使用增强提取；
- `src/policies/hash_key.rs`：
  - 默认 header 名单加入 agent 原生会话 header；
  - body 提取改为 JSON-aware（保留文本扫描兜底）；
  - 新增 `first_user_prompt:` 回退（整包 body hash 之前）；
- CLI / Python 启动器新增 `--hash-key-config <file.json>`；
- 示例配置与开发文档。

## 3. 配置文件

### 3.1 字段说明

文件为 UTF-8 JSON，所有字段可选：

| 字段 | 类型 | 默认 | 含义 |
|---|---|---|---|
| `session_headers` | `string[] \| null` | `null` | 完整、有序的会话 header 名单；`null`/缺省使用内置默认名单 |
| `extra_session_headers` | `string[]` | `[]` | 追加到默认（或自定义）名单之后，用于“补充” |
| `use_body_session_fields` | `boolean` | `true` | 是否在 header 未命中时解析 body 会话字段 |
| `fallback_to_first_user_prompt` | `boolean` | `true` | 无 header/body 会话标识时是否回退到首轮 user prompt hash |

Header 匹配不区分大小写（router 内部统一小写）。

### 3.2 示例

最小配置（只补充自定义 header，其余用默认）：

```json
{
  "extra_session_headers": ["x-my-session-id", "x-gateway-conversation-id"],
  "use_body_session_fields": true,
  "fallback_to_first_user_prompt": true
}
```

全量自定义名单（`session_headers` 会**替换**内置名单）：

```json
{
  "session_headers": [
    "x-session-id",
    "x-claude-code-session-id",
    "x-session-affinity",
    "x-opencode-session",
    "session-id",
    "session_id",
    "thread-id",
    "x-user-id",
    "x-tenant-id",
    "x-correlation-id",
    "x-request-id",
    "x-trace-id"
  ],
  "extra_session_headers": ["x-gateway-session-id"],
  "use_body_session_fields": true,
  "fallback_to_first_user_prompt": true
}
```

仓库内示例：

- `examples/configs/consistent_hash_router_config.example.json`（完整 router 配置，policy=consistent_hash）
- `examples/configs/consistent_hash_session_config.minimal.json`
- `examples/configs/consistent_hash_session_config.full.json`

### 3.3 使用方式

原生 Rust 二进制：

```bash
vllm-router \
  --policy consistent_hash \
  --worker-urls http://worker1:8000 http://worker2:8000 \
  --hash-key-config examples/configs/consistent_hash_session_config.minimal.json
```

PD 模式（prefill/decode 策略也会应用同一配置）：

```bash
vllm-router \
  --vllm-pd-disaggregation \
  --prefill http://prefill1:8000 --decode http://decode1:8000 \
  --prefill-policy consistent_hash --decode-policy consistent_hash \
  --hash-key-config examples/configs/consistent_hash_session_config.minimal.json
```

Python 启动器：

```bash
vllm-router --policy consistent_hash \
  --worker-urls http://worker1:8000 \
  --hash-key-config examples/configs/consistent_hash_session_config.minimal.json
```

或直接构造 `Router`：

```python
Router(
    policy=PolicyType.ConsistentHash,
    worker_urls=["http://worker1:8000"],
    hash_key_config="session_affinity.json",
)
```

注意：配置文件只在 `consistent_hash` 策略下生效；对其它策略传入会报配置错误。

## 4. Hash key 提取算法

最终提取顺序（默认配置）：

```text
1. HTTP header（按名单顺序）
   x-session-id
   x-claude-code-session-id          # Claude Code
   x-session-affinity                # OpenCode / Pi
   x-opencode-session                # OpenCode 自家 provider
   session-id / session_id           # Codex / Pi / Roo / Cline 通道
   thread-id                         # Codex thread/对话
   x-user-id / x-tenant-id           # 旧用户/租户粘性，保留
   x-correlation-id / x-request-id / x-trace-id   # 兼容保留，可在配置中移除

2. Body 会话字段（JSON-aware）
   session_params.session_id
   metadata.session_id / metadata.user_id 的 `_session_<uuid>` 后缀（Anthropic）
   client_metadata.session_id / thread_id（Responses API）
   prompt_cache_key / conversation_id / session_id / thread_id
   user / user_id

3. 首轮 user prompt hash（默认开启）
   first_user_prompt:<fbi_hash(第一条 user 消息文本)>

4. 旧兜底（保底）
   request_hash:<fbi_hash(整包 body)> 或 request:<短文本>
```

### 4.1 Header

默认内置名单同时覆盖了：

- Claude Code：`x-claude-code-session-id`
- Codex CLI / Pi：`session-id`、`thread-id`、`x-client-request-id`（`x-client-request-id` 不做默认主 key，见“兼容性”）
- OpenCode：`X-Session-Id`、`x-session-affinity`、`x-opencode-session`
- Cline / Roo 通道：`session_id`
- 通用网关：`x-session-id`

提取结果仍为 `header:<lowercase-name>:<value>`，保持与旧版本一致，避免哈希环键值格式变化。

### 4.2 Body

由于现在可以拿到结构化 JSON，优先做协议感知解析，避免全文扫描在 prompt/工具描述里误匹配：

- Anthropic：`metadata.user_id` 形如
  `user_<account>_account__session_<session-uuid>`，只提取 `_session_` 之后的 UUID，key 为 `session:<uuid>`；这样不会把整个账号串当 key；
- OpenAI Responses（Codex CLI / Pi）：`client_metadata.session_id` / `thread_id`；
- 通用/兼容网关：`conversation_id`、顶层 `session_id` / `thread_id`、`prompt_cache_key`；
- 旧 Chat 格式：`session_params.session_id`、`user`、`user_id`。

如果 body 不是合法 JSON，仍然走旧的文本扫描器（单引号 JSON 等兼容场景）。

### 4.3 首轮 user prompt 回退

支持三种主流请求形态：

```text
Chat Completions / Anthropic: messages[].role == "user" -> content（string 或 text block 数组）
Responses API:                input[].role == "user" / input[].message.role == "user"
Legacy Completion:            prompt（string）
```

选择**第一条有文本内容的 user 消息**，对全文做 `fbi_hash`：

```text
first_user_prompt:<16 位 hex>
```

对 coding agent 而言，第一轮 user prompt 是整个会话里最稳定的内容锚点：后续轮次即使 messages 不断增长、工具调用/compaction 摘要变化，只要首轮用户消息不变，路由 key 就稳定。

若请求不是上述形态（例如无法拿到首轮消息），自动落到第 4 步旧兜底，保证路由仍有确定性。

## 5. 代码改动点

| 文件 | 改动 |
|---|---|
| `src/config/session_affinity.rs` | 新增 `SessionAffinityConfig`、JSON 文件加载、单测 |
| `src/config/mod.rs` | 导出新模块 |
| `src/config/types.rs` | `PolicyConfig::ConsistentHash` 增加 `session_config` 字段与 `with_session_affinity_config_file` |
| `src/policies/hash_key.rs` | 默认 header 名单、JSON body 解析、`metadata.user_id` 后缀、首轮 user prompt 回退 |
| `src/policies/consistent_hash.rs` | 保存并使用 `SessionAffinityConfig`；`with_session_config` 构造 |
| `src/policies/factory.rs` / `registry.rs` | 把 `PolicyConfig.session_config` 传给 policy；动态 `consistent_hash` hint 分支 |
| `src/main.rs` | `--hash-key-config` CLI 参数，加载到 main/prefill/decode policy |
| `src/lib.rs` | PyO3 `Router` 增加 `hash_key_config`，`to_router_config` 应用配置 |
| `py_src/vllm_router/router_args.py` / `router.py` | Python CLI 参数与文档 |
| `docs/load_balancing/README.md` | 更新 hash key 优先级说明 |

## 6. 测试

运行：

```bash
# hash key 提取与配置（Rust 单元测试）
cargo test --lib hash_key
cargo test --lib session_affinity
cargo test --lib consistent_hash

# consistent_hash 集成测试（含 agent header、metadata、首轮 prompt 回退）
cargo test --test test_consistent_hash_policy

# 全量单测/集成
cargo test
```

新增覆盖点：

- `x-claude-code-session-id`、`session-id`/`thread-id` 命中；
- 自定义 `session_headers` 覆盖内置名单；
- Anthropic `metadata.user_id` 的 `_session_<uuid>` 提取；
- Responses `client_metadata.session_id/thread_id` 提取；
- 同一首轮 user prompt 的多轮请求（含 assistant/tool 历史增长）路由到同一 worker；
- 关闭 `fallback_to_first_user_prompt` 后行为回退到旧整包 body hash；
- 配置文件加载、非 consistent_hash 策略报错、文件缺失报错。

## 7. 兼容性与注意事项

1. **key 前缀格式**：header 继续使用 `header:<name>:<value>`，body session 继续使用 `session:` / `user:`；新增 `thread:` 前缀（Responses `thread_id`）与 `first_user_prompt:` 前缀。
2. **行为变化**：
   - 没有 session header/body 的多轮请求从“整包 body hash”变为“首轮 user prompt hash”，第一次升级时同一会话可能迁移一次 worker；
   - `x-request-id` / `x-trace-id` 仍保留在默认名单末尾以便向后兼容；若网关注入 per-request ID，建议在配置文件中用 `session_headers` 排除它们。
3. **`x-client-request-id` 不在默认主 key 名单**：Codex/Pi 把它用作会话/thread 级 ID，但通用 HTTP 客户端常把它当 per-request ID；需要它时请通过配置文件显式加入。
4. **首轮 user prompt 不是客户端声明**：如果两个不同会话首轮 prompt 完全相同且都不带任何标识，会被 hash 到同一 worker（无状态回退的固有上限）。有真实会话 header 时不受影响。
5. **配置文件仅作用于 `consistent_hash`**：`rendezvous_hash` 等策略继续使用内置默认提取（包含新增 header/body 与首轮 prompt 回退），但不会读取自定义文件。
6. **Anthropic Messages 端点**：本仓库当前 HTTP 入口以 OpenAI Chat/Responses 为主；如果前面有 Anthropic 兼容网关转发，Claude Code 的 header/body 会话标识会在转发后保留，按上述逻辑工作。

## 8. 后续可做项

- 把 `x-request-id`/`x-trace-id` 等 per-request 头从默认名单彻底移除，并提供独立的 `request_id_fallback` 开关；
- 对 Responses `previous_response_id` / `x-codex-turn-state` 做状态化的“服务端会话”粘性；
- 在 metrics 中记录 hash key 来源（header/body/prompt-anchor/request），便于生产观测；
- 提供 TOML/YAML 等更多配置文件格式（当前为 JSON）。
