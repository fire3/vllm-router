# 主流 Coding Agent 请求中的会话标识调研（vllm-router consistent_hash 增强依据）

> 调研时间：2026-09-02
> 调研对象：Claude Code、OpenAI Codex CLI、OpenCode、Pi、Roo Code、Cline 等主流 coding agent
> 目的：回答“这些 agent 发出的接口请求里，哪些字段可以用来稳定地识别一个会话/对话”，为 vllm-router `consistent_hash` 的 session affinity（会话粘性）hash key 计算增强提供依据。
>
> 相关现状分析见 [notes/consistent-hash-main-analysis.md](consistent-hash-main-analysis.md)；当前实现见 `src/policies/hash_key.rs`。

---

## 0. 结论摘要

1. **主流 coding agent 的“会话/对话标识”主要出现在 HTTP Header，而不是请求体里。** 即使出现在请求体里，也不是标准 Chat Completions 的 `user` / `session_id` 顶层字段，而是各协议私有的嵌套字段（如 Responses API 的 `client_metadata.thread_id`、Anthropic Messages 的 `metadata.user_id` 后缀、`prompt_cache_key` 等）。

2. **可稳定用于 session affinity 的字段（按 agent 分类）**：

| Agent | 请求协议/形态 | 最有用的会话标识 |
|---|---|---|
| Claude Code | Anthropic Messages (`/v1/messages`) | Header `x-claude-code-session-id`（会话/对话级）；Body `metadata.user_id` 中 `_session_<uuid>` 后缀 |
| Codex CLI | OpenAI Responses API (`/responses`) | Header `session-id`（会话级）、`thread-id`（thread/对话级）、`x-client-request-id`（= thread-id）；Body `client_metadata.session_id/thread_id`、`prompt_cache_key`（= session-id） |
| OpenCode | OpenAI / Anthropic / 自研等 | Header `X-Session-Id`、`x-session-affinity`（自研 provider 为 `x-opencode-session`） |
| Pi（Codex 系） | OpenAI Chat/Responses/Codex Responses | Header `session-id`/`session_id` + `x-client-request-id`（部分格式还有 `x-session-affinity`）；Body `prompt_cache_key` |
| Roo Code | OpenAI Native/Codex Responses | Header `session_id`（值 = taskId/会话 ID） |
| Cline | openai-codex 等自有通道 | Header `session_id`（openai-codex）；自有网关 `X-Task-ID` |
| Cline / Roo（通用 OpenAI-compatible / Anthropic） | Chat Completions / Messages | **默认不携带会话 ID**，只能靠自定义 header 或内容锚点回退 |

3. **对 vllm-router 最直接的三个结论**：
   - 现在 `SESSION_HEADER_NAMES`（`src/policies/hash_key.rs:11`）里 **没有** 上述任何 agent 原生会话 header，导致绝大多数 coding agent 流量落到“整包 body hash”回退；
   - coding agent 的对话请求 body 是**每次递增的完整历史**（多轮、工具调用、compaction 摘要都会变），整包 body hash 每次都会变 → **consistent_hash 的粘性天然被打破**。这是当前实现用于 coding agent 场景时最核心的缺陷；
   - 现有 header 列表把 `x-request-id` / `x-trace-id` / `x-correlation-id` 放在高优先级。它们在很多 HTTP 客户端语义里是 **per-request** 的；若把每次请求不同的 request-id 当 hash key，同样会把同一会话的请求打散。

---

## 1. 背景：vllm-router 现在如何取 hash key

`consistent_hash` / `rendezvous_hash` 共用 `hash_key::extract_hash_key`（`src/policies/hash_key.rs:29`）：

```text
1. HTTP Header（按数组顺序，先到先得）
   x-session-id > x-user-id > x-tenant-id > x-correlation-id > x-request-id > x-trace-id
2. Body（文本扫描）
   session_params.session_id > user > session_id > user_id
3. 回退
   body > 100 字节 -> request_hash:fbi_hash(body)
   否则            -> request:<body 原文>
```

现有实现的相关事实：

- header 名统一小写存储（`src/routers/http/router.rs:489` `headers_to_request_headers`），所以 `X-Session-ID` 会被归一到 `x-session-id`；
- body 提取是轻量文本扫描，不是 JSON 解析（支持双引号/单引号/无引号，见 `extract_field_value`）；
- PD（prefill/decode）模式下同一个 key 分别在 prefill/decode 两个池做一次 consistent hash；discovery 路径目前**不把 header 传给 policy**（见 `notes/consistent-hash-main-analysis.md` §4.3）；
- `hash_key.rs` 与 `docs/load_balancing/README.md` 的优先级表不一致（以代码为准，文档表把 x-request-id 排在 x-correlation-id 前）。

### 1.1 这套逻辑面对 coding agent 时的缺口

1. **不认识 agent 原生 header**：`x-claude-code-session-id`、`x-session-affinity`、`session-id`、`thread-id`、`x-opencode-session` 等都不会命中。
2. **不认识 Responses API / Anthropic Messages 的嵌套 body 字段**：
   - Responses API：`client_metadata.session_id` / `client_metadata.thread_id`；
   - Anthropic Messages：`metadata.user_id`（值形如 `user_<hash>_account__session_<uuid>`）；
   - 部分 OpenAI 兼容网关会带顶层 `conversation_id`；
3. **文本扫描会被 message 内容“骗”到**：prompt/工具 schema/历史消息里如果出现形如 `"user": "..."`、`"session_id": "..."` 的 JSON 片段，先到先得的扫描可能取到错误值。
4. **fallback 对多轮会话不稳定**：coding agent 每一轮都携带累积历史，整包 body 的 hash 每轮都不同。
5. **request/trace 语义误用**：`x-request-id`/`x-trace-id`/`x-client-request-id`（后者未纳入列表，但属于“容易被误加”的头）在通用 HTTP 客户端里通常是单次请求标识，不应当作会话 key 的首选。

---

## 2. 调研对象与取样

以下结论基于各项目 **main 分支源码直接检索**（2026-09-02 拉取），Claude Code 为闭源，依据其官方 gateway 文档与公开抓包/第三方实现交叉验证：

| 项目 | 仓库 | 取样 commit | 证据形态 |
|---|---|---|---|
| Claude Code | anthropics/claude-code（闭源 CLI） | — | 官方 LLM Gateway 协议文档 + changelog/issue + 社区抓包 |
| Codex CLI | github.com/openai/codex | `8d32abc`（2026-09-02） | 源码（Rust） |
| OpenCode | github.com/sst/opencode | `69c172e`（2026-09-01） | 源码（TS） |
| Pi | github.com/earendil-works/pi | `256f630`（2026-09-02） | 源码（TS） |
| Roo Code | github.com/RooCodeInc/Roo-Code | `b867ec9`（2026-05-15） | 源码（TS） |
| Cline | github.com/cline/cline | `be59305`（2026-09-01） | 源码（TS） |

> 注意：CLI 类工具更新很快，字段与 header 名是**开放列表**，以下“当前 main”结论可能随版本变化；落地实现时应保留可配置性，而不是硬编码死列表。

---

## 3. 各 agent 的请求字段明细

### 3.1 Claude Code

#### 网络形态

- 默认 Anthropic Messages API：`POST /v1/messages`（走 gateway 时路径带 `?beta=true`，文档明确“按路径匹配而非完整 URL”）。
- 也可走 Bedrock / Vertex / Foundry 等封装，但 vllm-router 场景通常是 `ANTHROPIC_BASE_URL` 指向一个 Anthropic 兼容网关。

#### 官方文档确认的 Header

见 Claude Code LLM Gateway Protocol 文档“Request headers”表：

| Header | 含义 |
|---|---|
| `x-claude-code-session-id` | 当前 Claude Code **会话（session）**的唯一标识；官方用途就是“不解析 body 也能把一个 session 的所有请求聚合起来” |
| `x-claude-code-agent-id` | 子代理标识，仅在 session 内 spawn 的 agent 请求上出现 |
| `x-claude-code-parent-agent-id` | 嵌套 agent 的父级标识 |

官方 changelog：v2.1.86 起新增 `X-Claude-Code-Session-Id` header，目的是让代理/gateway 按 session 聚合请求。因此：

- **做 session affinity 时首选 `x-claude-code-session-id`**；
- 子代理与主对话共享同一个 session-id，只有 `x-claude-code-agent-id` 不同 → 建议按 session-id 路由，不要按 agent-id 路由。

#### Body 里的会话线索（社区抓包 + 第三方网关实现交叉确认）

Anthropic Messages body 里有一个 `metadata` 对象：

```json
{
  "model": "claude-...",
  "messages": [],
  "system": [],
  "tools": [],
  "metadata": {
    "user_id": "user_9e197bc9a8f0823f64ce49204027a967c70d3948256f2d2eb08492b8f4037297_account__session_e93a69e9-3d39-4c8f-82c1-dc64d7b288a3"
  },
  "max_tokens": 32000,
  "stream": true
}
```

（示例来自 2026-03 的公开抓包文章，Claude Code 2.1.x。）

`metadata.user_id` 的格式为：

```text
user_<account_hash>_account__session_<session_uuid>
```

其中 **`_session_` 之后的 UUID 与会话一一对应**。很多第三方网关（CLIProxyAPI、claude-code-hub、GoModel 等）都按这个后缀提取 session：

- 取 `_session_` 后的部分 → conversation/session key；
- 取完整 `user_...` → 只能得到 user 粒度，并且同一用户多个会话会撞到同一个 key，负载均衡粒度变粗。

> 结论：Claude Code 在 body 里**没有单独的 `conversation_id` / `session_id` 顶层字段**；header 是从 v2.1.86 起官方推荐的方式，body 后缀适合兼容老版本或 header 被网关剥掉的情况。

### 3.2 OpenAI Codex CLI

#### 网络形态

Codex CLI 当前只走 **OpenAI Responses API**（`POST /responses`，可走 SSE 或 WebSocket），且已移除 `wire_api = "chat"`（源码 `model-provider-info/src/lib.rs:57` 明确报错：`"wire_api = \"chat\" is no longer supported"`）。所以 routed 到的 vLLM 必须兼容 `/v1/responses`。

#### Header（源码确认）

`codex-rs/codex-api/src/requests/headers.rs:5` `build_session_headers()`：

```rust
if let Some(id) = session_id { headers.insert("session-id", id) }
if let Some(id) = thread_id  { headers.insert("thread-id", id) }
```

`codex-rs/codex-api/src/endpoint/responses.rs:120` 以及 `core/src/client.rs:1246`：

```text
x-client-request-id = thread_id
```

另有 `core/src/client.rs:153-170` 定义的 Codex 系列 header：

```text
x-codex-installation-id
x-codex-routing-hint
x-codex-turn-state
x-codex-turn-metadata
x-codex-parent-thread-id
x-codex-window-id
x-openai-subagent
```

其中 `x-codex-turn-state` 是服务端下发的 sticky-routing token，属于服务端会话态，router 无法单靠请求计算，但可说明 Codex 体系确实有“会话/线程”概念。

#### Body（源码确认）

`core/src/client.rs:1029`：`ResponsesApiRequest.client_metadata` 由 `CodexResponsesMetadata::client_metadata()` 生成，含：

```text
installation_id
session_id
thread_id
x-codex-window-id
（条件性）turn_id / x-codex-turn-metadata / x-codex-parent-thread-id / ...
```

同时 body 顶层带 `prompt_cache_key`：

```rust
// core/src/client.rs:540-552
fn prompt_cache_key(&self, responses_metadata: &CodexResponsesMetadata) -> String {
    ...
    responses_metadata.session_id.clone()
}
```

#### session-id vs thread-id 语义（源码确认）

`core/src/session/session.rs:759-811`：

- **thread_id** 是 thread / 对话（resume 时沿用历史 `conversation_id`，fork/subagent 会生成新 thread_id）；
- **session_id** 在 root 会话下由 thread_id 派生，**resume 后保持稳定**；subagent 请求沿用同一 session_id；
- `x-client-request-id` = thread_id；body `prompt_cache_key` = session_id。

官方单测 `core/tests/suite/prompt_cache_key.rs` 里直接断言：

```text
root:   session-id = <expected_session_id>, thread-id = root_thread_id,
        x-client-request-id = root_thread_id, prompt_cache_key = <expected_session_id>
child:  session-id = <expected_session_id>, thread-id = child_thread_id（与 root 不同）,
        x-client-request-id = child_thread_id, prompt_cache_key = <expected_session_id>
```

> 结论：
> - 想要“整个 Codex 会话（含子代理）都钉同一 worker” → 用 `session-id` header / body `prompt_cache_key` / `client_metadata.session_id`；
> - 想要“每个 thread/对话独立钉” → 用 `thread-id` header / `client_metadata.thread_id` / `x-client-request-id`；
> - 常规多轮对话中 session-id 与 thread-id 都很稳定；`x-client-request-id` 只有在能确认是 Codex/Pi 体系时才可作为 thread 级 key（通用 HTTP 客户端里它常是 per-request）。

### 3.3 OpenCode

#### Header（源码确认）

`packages/core/src/session/runner/llm.ts:210`（runner 组装 LLM 请求处）：

```ts
headers: {
  "x-session-affinity": session.id,
  "X-Session-Id": session.id,
  ...(session.parentID ? { "x-parent-session-id": session.parentID } : {}),
}
```

`packages/opencode/src/session/llm/request.ts:198`（请求最终组装处）把“非 opencode 自家 provider”与“opencode 自家 provider”分开：

```ts
opencode provider:
  x-opencode-project / x-opencode-session / x-opencode-request / x-opencode-client
其他 provider:
  "x-session-affinity": sessionID
  "X-Session-Id": sessionID
  x-parent-session-id（有 parent 时）
```

#### Body（源码确认）

OpenCode 的 `providerOptions.openai.promptCacheKey = sessionID`（`packages/core/src/session/runner/llm.ts:204-214`，并把 `ses_` 前缀裁掉）；对 OpenAI / Responses / 部分 OpenAI-compatible 供应商，会被序列化成 body 字段：

- Responses 系：`prompt_cache_key`
- 部分 Chat 系：`prompt_cache_key`（cerebras/deepinfra）或 `promptCacheKey`（openai/azure/xai/mistral 等，见 `packages/opencode/src/provider/transform.ts:1290-1307`）

> 结论：OpenCode 最稳定、协议无关的会话标识是 **`X-Session-Id` / `x-session-affinity` header**；body 里只有 provider 特定的 `prompt_cache_key`。

### 3.4 Pi（Codex 系 agent）

Pi 是基于 Codex 核心的 coding agent（仓库 `earendil-works/pi`，LLM 层在 `packages/ai`）。它的请求字段与 Codex 同源但随协议分叉：

#### OpenAI Codex Responses（`packages/ai/src/api/openai-codex-responses.ts`）

- SSE header（`buildSSEHeaders`，约 1620-1629 行）：

  ```text
  session-id = sessionId
  x-client-request-id = sessionId
  originator = "pi"
  ```

- WebSocket header：`x-client-request-id` / `session-id` 同样由会话 ID 派生（约 1647-1648 行）；
- body：`prompt_cache_key = clampOpenAIPromptCacheKey(sessionId)`（约 557 行）。

#### OpenAI Responses（`packages/ai/src/api/openai-responses.ts:238`）

```text
sessionAffinityFormat == "openrouter" -> x-session-id = sessionId
sessionAffinityFormat == "openai"     -> session_id = sessionId
任意 openai 系 -> x-client-request-id = sessionId
body -> prompt_cache_key = sessionId（cacheRetention != none 时）
```

#### OpenAI Chat Completions（`packages/ai/src/api/openai-completions.ts:753-777`）

```text
openrouter 格式 -> x-session-id = sessionId
openai 格式     -> session_id = sessionId
都带            -> x-client-request-id = sessionId, x-session-affinity = sessionId
body（仅 api.openai.com 或长缓存）-> prompt_cache_key = sessionId
```

#### Anthropic Messages（`packages/ai/src/api/anthropic-messages.ts:950`）

当 provider compat 开启时带：

```text
x-session-affinity = sessionId
```

> 结论：Pi 的 `sessionId` 几乎在所有格式下都会以某种 header 发出去（`session-id` / `session_id` / `x-session-id` / `x-session-affinity`），并且 body 常见 `prompt_cache_key`。这些值全部指向同一个会话 ID。

### 3.5 Roo Code（VS Code）

#### OpenAI Native / OpenAI Codex（Responses 系）

`src/api/providers/openai-native.ts:95-103` 默认 header：

```ts
{
  originator: "roo-code",
  session_id: this.sessionId,   // provider 生命周期 uuid
}
```

每个请求再覆盖（`openai-native.ts:414-425`、`562-575`）：

```ts
session_id: taskId || this.sessionId
```

OpenAI Codex provider 同样发送（`src/api/providers/openai-codex.ts:359-361`、`504-506`）：

```ts
originator: "roo-code",
session_id: taskId || this.sessionId,
```

这里的 `taskId` 就是 Roo 内部会话/task 的 ID：一个对话/任务对应一个 taskId，子任务有 `parentTaskId`。

#### Unbound / Roo 自家 provider

`src/api/providers/unbound.ts:145` 请求体里带：

```ts
unbound_metadata: { originApp: "roo-code", taskId: metadata?.taskId, mode: metadata?.mode }
```

#### 通用 OpenAI-compatible / Anthropic

- OpenAI-compatible provider（vLLM 常用接入方式）默认 header 只有 `HTTP-Referer`、`X-Title`、`User-Agent`（`src/api/providers/constants.ts`），**不自动带会话 ID**；
- 配置里允许用户自定义 headers（`config.headers`），所以可以手动注入 `x-session-id`，但默认没有；
- Anthropic provider 走标准 Messages API，也未自动填充 `metadata.user_id`。

> 结论：Roo 只有在 OpenAI Native / OpenAI Codex / Unbound 这些自有通道才自动携带 `session_id` 或 body `taskId`；用户把 Roo 接到 vllm-router 的“OpenAI Compatible”通道时，默认没有会话 key。

### 3.6 Cline（VS Code）

新版 Cline 的请求 header 解析集中在 `sdk/packages/llms/src/providers/request-headers.ts`：

- `openai-codex` provider（`buildOpenAICodexRequestHeaders`，约 119-135 行）：

  ```ts
  originator: "cline",
  session_id: input.sessionId,
  ChatGPT-Account-Id（有账号时）
  ```

- Cline 自有 billing provider（`buildClineRequestHeaders`，约 80-93 行）：

  ```ts
  X-Task-ID: input.sessionId
  ```

- 其他 provider（Anthropic / OpenAI-compatible 等）默认只带 `HTTP-Referer: https://cline.bot`、`X-Title: Cline` 等标识头；只有用户配置的 `headers` 层（stored/config/session）会被合并进去，**默认没有会话 ID**。

> 结论：与 Roo 类似，Cline 的通用 OpenAI-compatible / Anthropic 通道默认不带会话 key；在 Cline 自有通道或 openai-codex 通道才有 `session_id` / `X-Task-ID`。

### 3.7 其他 agent（补充观察）

- **Amp CLI（Sourcegraph）**：第三方路由实现（CLIProxyAPI 等）使用 `x-amp-thread-id` 作为 thread 级会话 key；thread ID 形如 `T-<uuid>`，`amp threads continue <thread-id>` 用它恢复会话。
- **Gemini CLI**：目前只在 hooks/本地 JSONL 里有 sessionId，请求本身不携带稳定会话 header（第三方工具如 is-ai-agent 也证实）；走 OpenAI-compatible 时需要外部注入 header 或内容锚点。
- **Copilot CLI / GitHub Copilot coding agent**：有 `X-Interaction-Type` 等请求头，会话标识主要依赖 OpenAI 侧 conversation/thread 状态，不在客户端标准 body 中暴露稳定 id。

---

## 4. 对比汇总表

| Agent | 出口 API | Body 中的会话字段 | Header 中的会话字段 | 建议路由粒度 |
|---|---|---|---|---|
| Claude Code | Anthropic Messages | `metadata.user_id` 的 `_session_<uuid>` 后缀（老版本/无 header 时兜底） | `x-claude-code-session-id`（首选） | session/对话（子代理共用） |
| Codex CLI | Responses API | `client_metadata.session_id/thread_id`、`prompt_cache_key`（=session-id） | `session-id`、`thread-id`、`x-client-request-id`（=thread-id） | session 或 thread 二选一 |
| OpenCode | 多协议 | 供应商相关的 `prompt_cache_key`/`promptCacheKey` | `X-Session-Id`、`x-session-affinity`、`x-parent-session-id`；自家 provider `x-opencode-session` | session |
| Pi | Chat/Responses/Codex/Anthropic | `prompt_cache_key`（部分格式） | `session-id` / `session_id` / `x-session-id` / `x-session-affinity` + `x-client-request-id` | session |
| Roo Code | OpenAI Native/Codex Responses | Unbound 通道 `unbound_metadata.taskId` | `session_id`（=taskId）；Unbound 另有 header 模板 | task/session |
| Roo Code | 通用 OpenAI-compatible | 无 | 无（可自定义 header） | 需注入/内容锚点 |
| Cline | openai-codex / 自有网关 | 无 | `session_id` / `X-Task-ID` | session |
| Cline | 通用 OpenAI-compatible / Anthropic | 无 | 无（可自定义 header） | 需注入/内容锚点 |
| Amp CLI | 自有/兼容 | — | `x-amp-thread-id` | thread |

---

## 5. 对 vllm-router 增强的关键洞察

### 5.1 “会话”在 agent 生态里至少分两层

1. **session / task 层**：一次 `claude`、`codex`、`opencode` 进程或一个 VS Code 任务。子代理（subagent）与主线程共享 session-id，但 thread-id/agent-id 不同。
2. **thread / conversation 层**：可 resume 的单个对话。Codex 的 `thread-id`、Claude Code 的“session”（Claude Code 的术语里 session 就是可 resume 的对话）、Amp 的 thread 都接近这一层。

对 KV cache / prefix cache 而言，最理想的是**同一对话 + 同一子代理树**共用 worker。实现上最简单且安全的做法是“agent 自己声明的 session 粒度”：

- Codex/Pi/OpenCode 显式把 `prompt_cache_key` 或 affinity header 设为 **session 级**，说明上游自己也认为 session 级聚合有利于缓存；
- 因此 vllm-router 只需与这些 agent 声明的 session key 保持一致即可，不需要自己发明 thread 语义。

### 5.2 Body 整包 hash 不适合多轮 coding agent

真实请求体每轮变化的部分非常多：

- messages 数组持续 append（用户消息、assistant 回复、tool call/result）；
- Claude Code 的 system context 会随 skills/MCP/context 变化；
- compaction 后摘要替换旧历史；
- 时间、token budget 等上下文注入变化。

因此 `request_hash:{fbi_hash(body)}` 只在“同一请求重放”场景稳定，不适合多轮会话。

### 5.3 需要区分“session 类 header”和“request 类 header”

当前列表把 request/trace 类 header 放在高优先级。从 agent 实际流量看：

- 很多 SDK/网关会生成或注入 per-request 的 request-id（`x-request-id`、`x-client-request-id`、`x-ms-client-request-id` 等）；
- Codex 的 `x-client-request-id` 虽然是 thread 级，但那是 **Codex/Pi 私有语义**，通用客户端不是这样。

所以新的 header 提取策略应**先按名字分类**：`session/thread/affinity/task` 类优先，`request/trace/correlation` 类默认不作为会话 key（或者只在明确配置下作为最后一级）。

### 5.4 Header 优于 Body

- header 提取不需要读/解析 body，开销低；
- 不依赖协议 schema（Anthropic/OpenAI Responses/Chat 都适用）；
- 避免 prompt 内容中的 JSON 片段误命中；
- 上游（Claude Code 官方、vllm-sr、CLIProxyAPI 等）也把“显式 header > body 解析”作为标准。

---

## 6. 建议的 hash key 计算改进

### 6.1 设计原则

1. **显式 > 隐式**：客户端/网关声明的 session key 优先于 router 从内容推导。
2. **语义分级**：session/thread 类 > user/tenant 类 > 稳定内容锚点 > 单请求标识。
3. **协议感知的 body 解析**：对 Responses API / Anthropic Messages / Chat Completions 分别提取，而不是全文扫字符串。
4. **可配置**：header 白名单与优先级应可配置，因为 agent 的 header 是开放列表、会随版本变。
5. **类目映射（alias 归一）**：同一会话值以不同 header 出现时（Codex 的 `session-id`、Pi 的 `session_id`、通用 `x-session-id`），应归一到同一 hash key，避免“同名不同值被前缀拆开”。

### 6.2 建议的优先级（协议无关 → 协议相关）

```text
P0  用户/网关显式声明的会话 key
     x-session-id

P1  通用“会话/亲和”header（agent 原生，值本身稳定）
     x-claude-code-session-id
     x-session-affinity            # OpenCode / Pi
     x-opencode-session            # OpenCode 自家 provider
     session-id                    # Codex / Pi
     session_id                    # Pi / Roo OpenAI / Cline openai-codex
     thread-id                     # Codex thread/对话级
     x-amp-thread-id               # Amp（可配置项）
     X-Task-ID                     # Cline 自有网关（可配置项）
     （x-client-request-id 仅在确认客户端是 Codex/Pi 系时使用，
       或放到可配置的 “codex-family” 规则里；默认不启用）

P2  Body 中的“会话/对话”字段（需要协议感知解析）
     Responses API:
       client_metadata.thread_id      -> thread 级
       client_metadata.session_id     -> session 级
       conversation_id / thread_id / session_id（部分兼容网关）
       prompt_cache_key               # Codex/Pi 体系 = session-id
     Anthropic Messages:
       metadata.user_id 的 _session_<uuid> 后缀 -> session 级
       metadata.session_id（若未来出现）
     Chat Completions:
       session_params.session_id       # 现有约定，保留
       conversation_id
       session_id（legacy）

P3  user / tenant 级（粒度更粗，负载均衡差，仅无更细粒度时使用）
     x-user-id / x-tenant-id
     body user / user_id
     metadata.user_id 的完整值（Claude Code 老版本无 header 时）

P4  稳定内容锚点（最后回退，且要“稳定”）
     hash(model + 第一条用户消息/会话开头若干 token)
     或 hash(消息数组长度不变时共享的 prefix)
     避免整包 body hash

P5  单请求标识（默认不用于 affinity）
     x-request-id / x-trace-id / x-client-request-id（非 Codex 系）
```

> 关键点：`x-request-id`/`x-trace-id` 应从默认列表移除或降到最低优先级；否则每次请求都会换 worker。

### 6.3 规范化与去别名

- 所有 header 值先 trim；
- 对 `metadata.user_id` 这类复合值，先按 `_session_<uuid>` 提取会话 UUID，再进入 hash key，而不是把整串 account+session 直接 hash；
- 建议把 P0/P1 的已知 header 映射成少量 canonical 前缀（如 `session:`、`thread:`、`user:`、`request:`），而不是 `header:{名字}:{值}`：
  - 好处：Codex 切 header 名（`session-id` ↔ `session_id` ↔ `x-session-id`）不会导致同一会话迁移；
  - 代价：不同含义但同名会互相污染，因此映射表必须显式且默认保守。
- 子代理类字段（`x-claude-code-agent-id`、`x-openai-subagent`、`x-codex-parent-thread-id`）不要做主 key；它们描述的是 session 内部的归属关系。

### 6.4 具体字段映射建议

```text
x-session-id                          -> session:{value}
x-claude-code-session-id              -> session:{value}
x-session-affinity                    -> session:{value}
x-opencode-session                    -> session:{value}
session-id / session_id               -> session:{value}
X-Task-ID                             -> session:{value}
thread-id / x-amp-thread-id           -> thread:{value}
metadata.user_id (Anthropic)          -> 提取 _session_<uuid> -> session:{uuid}；否则 user:{full}
client_metadata.thread_id (Responses) -> thread:{value}
client_metadata.session_id (Responses)-> session:{value}
conversation_id / session_params.session_id / session_id(body) -> session:{value}
prompt_cache_key                      -> session:{value}（因为主流 agent 就是拿 session id 当 cache key）
```

PD 模式下 prefill/decode 两个池都用**同一个规范化 key**（现有 `select_worker_pair_with_headers` 已共享同一 hash key 的调用方式，保留即可）。

### 6.5 代码落点建议（供实现阶段参考）

1. `src/policies/hash_key.rs`：
   - 把 `SESSION_HEADER_NAMES` 拆成“session 类 / user 类 / request 类”多组常量；
   - 新增 `extract_hash_key_from_headers` 的规范化输出（去 `header:` 名前缀、按类目输出）；
   - body 提取改为 JSON-aware：先按协议解析，或至少对已知嵌套路径用真正的 JSON parser（`serde_json` 在依赖里已有）；
   - `metadata.user_id` 增加 `_session_` 后缀解析。
2. `src/routers/http/router.rs` / PD 相关入口：
   - 保证所有走 consistent_hash 的路径（包括 discovery 模式）都能把 header 交给 policy；
   - `/v1/responses` 的 body 解析层把 `client_metadata` 等结构化字段传给 hash key 提取器，而不是只传原始文本。
3. 配置层：
   - 支持 CLI/config 指定“额外 session header 列表”和优先级（兼容 `x-session-id` 网关注入场景）；
   - 提供开关“request 类 header 是否参与 affinity”（默认关）。
4. 指标/日志：
   - 打点记录“本次 hash key 来自哪个层级（header/body-user/body-session/anchor/request）”，便于上线后观察 coding agent 流量的粘性命中率。

### 6.6 验证用例（建议测试矩阵）

```text
1. Claude Code 同会话两轮：
   header x-claude-code-session-id 相同 -> 同一 worker
   老版本（仅 metadata.user_id）:
   body 中 user_..._session_<uuidA> vs user_..._session_<uuidB> -> 不同 worker
   同一 uuid 但 account hash 不同 -> 仍应同一 worker（只取后缀）

2. Codex CLI root + subagent：
   session-id 相同、thread-id 不同 -> 按 session 路由 => 同一 worker
   同 thread 连续两轮（含 tool 调用历史增长）-> 同一 worker（header 相同即可）

3. OpenCode / Pi：
   x-session-affinity / X-Session-Id / session-id 相同 -> 同一 worker
   prompt_cache_key 与 header 值一致时不应产生第二条路由

4. 负例：
   同一会话但每次 x-request-id 不同 -> 不应影响 key（request 类头不参与）
   两个不同会话 -> 不同 worker（分布测试沿用 examples/simulate_consistent_hash.rs）

5. 回退：
   Cline/Roo 通用 OpenAI-compatible 不带任何 session 头时：
   取“首条用户消息锚点”而非整包 body hash；编辑历史首轮内容不应破坏后续轮次粘性
   （若无法提供锚点，允许退化为单请求路由并在日志标注）
```

---

## 7. 风险与注意事项

1. **版本漂移**：Claude Code 的 `x-claude-code-session-id` 是 v2.1.86 才加的；Codex 的 header 集合也在演进。header 名单必须可配置并保持“开放列表”心态。
2. **header 被网关剥离/改写**：很多部署在 agent 与 vllm-router 之间还有 LLM gateway（做鉴权、计量、协议转换）。若网关剥离了 `x-claude-code-session-id` 等 header，router 只能靠 body 或网关注入的 `x-session-id`。
3. **不要混淆“用户标识”与“会话标识”**：`user` / `x-user-id` / 完整 `metadata.user_id` 是用户粒度；同一用户并发多个对话会全部钉到一个 worker，可能造成热点，也无法区分对话。
4. **subagent 与 agent 标识**：`x-claude-code-agent-id`、`x-openai-subagent` 表示子代理；按它们路由会拆散同一会话，与 agent 上游自己的 session 聚合意图相悖。
5. **隐私与日志**：session header 值是明文且跨请求稳定；不要在 debug 日志里全量打印，hash key 也不要直接等于可识别个人信息（例如不要把完整 `user_id` 作为日志字段）。
6. **fork / 分支**：Codex fork、Claude Code 新开会话等场景下会话 ID 会变化，粘性会随之迁移到新 worker；这是符合预期的“新会话”行为，不算 bug。若希望 fork 继承粘性，需要额外维护 parent→child 映射，超出单请求 hash 能力。
7. **Responses API 服务端状态**：Codex 的 `x-codex-turn-state` 等服务端下发的粘性 token 需要状态存储或会话表；request 内无法推导，后续如要支持“turn-state 级粘性”需单独设计。

---

## 8. 参考资料

### 官方文档 / 上游 issue

- Claude Code — Gateway protocol reference（含 `x-claude-code-session-id` 等 header 表）：<https://code.claude.com/docs/en/llm-gateway-protocol>
- Claude Code changelog（v2.1.86 起新增 `X-Claude-Code-Session-Id`）与 docs issue #40119
- openai/codex（main @ 8d32abc，2026-09-02）：
  - `codex-rs/codex-api/src/requests/headers.rs`（`session-id`/`thread-id`）
  - `codex-rs/codex-api/src/endpoint/responses.rs`（`x-client-request-id`）
  - `codex-rs/core/src/responses_metadata.rs`（body `client_metadata`）
  - `codex-rs/core/src/session/session.rs`（session/thread 语义）
  - `codex-rs/core/tests/suite/prompt_cache_key.rs`（root/subagent 断言）
- sst/opencode（main @ 69c172e）：
  - `packages/core/src/session/runner/llm.ts`（`X-Session-Id`/`x-session-affinity`/`x-parent-session-id`）
  - `packages/opencode/src/session/llm/request.ts`（最终 header 组装）
  - `packages/opencode/src/provider/transform.ts`（`prompt_cache_key`/`promptCacheKey`）
- earendil-works/pi（main @ 256f630）：
  - `packages/ai/src/api/openai-completions.ts`、`openai-responses.ts`、`openai-codex-responses.ts`、`anthropic-messages.ts`
- RooCodeInc/Roo-Code（main @ b867ec9）：
  - `src/api/providers/openai-native.ts`、`openai-codex.ts`、`unbound.ts`、`constants.ts`
- cline/cline（main @ be59305）：
  - `sdk/packages/llms/src/providers/request-headers.ts`

### 社区抓包 / 第三方实现（交叉验证）

- “claude code prompt详解”（2026-03，抓包展示 `metadata.user_id` 的 `user_*_account__session_*` 结构）：<https://developer.cloud.tencent.com.cn/article/2640838>
- CLIProxyAPI `ExtractSessionID` 的优先级（`metadata.user_id` 的 `_session_{uuid}` → `X-Session-ID` → Codex `session_id` header → `X-Amp-Thread-Id` → PI `X-Client-Request-Id` → body `conversation_id` → 内容 hash）：<https://pkg.go.dev/github.com/router-for-me/CLIProxyAPI/v6/sdk/cliproxy/auth>
- vLLM Semantic Router 的 Session identification 文档（`x-session-id` → `x-claude-code-session-id` → `metadata.user_id` → 指纹 → `x-request-id`）：<https://vllm-sr.ai/docs/api/session-identification/>
- GoModel / claude-code-hub 等网关对 `metadata.user_id` / session 的提取说明

### 本地代码

- `src/policies/hash_key.rs`：当前 hash key 提取
- `src/policies/consistent_hash.rs`：环、回退、DP、PD pair
- `src/routers/http/router.rs`：header 转 `RequestHeaders`
- `notes/consistent-hash-main-analysis.md`：main 分支策略行为分析
