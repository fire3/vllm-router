# Consistent Hash 策略分析（main 分支）

> 分析对象：本地 `main`（`1d10e71`），重点代码：
> `src/policies/consistent_hash.rs`、`src/policies/hash_key.rs`、
> `src/policies/registry.rs`、`src/core/worker_registry.rs`、
> `src/routers/http/router.rs`、`src/routers/http/pd_router.rs`、
> `src/routers/http/vllm_pd_router.rs`。

## 1. 一句话结论

`consistent_hash` 是一个**基于 64 位哈希环 + 每物理 worker 160 个虚拟节点**的会话粘性调度策略：同一路由 key（session/user/header 等）总是被映射到同一个 worker，目标 worker 不可用或集合变化时做“最小扰动”重映射。它**不是**负载感知策略，worker 的可用性判定、故障转移、重试等“服务质量保障”由 router 的通用层（健康检查、熔断、重试、并发限制）提供，策略本身只负责确定性选路和退避式 fallback。

---

## 2. 实现形态与核心算法

### 2.1 数据结构

`ConsistentHashPolicy` 只有两个内部状态（`src/policies/consistent_hash.rs:28`）：

- `hash_ring: RwLock<BTreeMap<u64, String>>`：有序哈希环，`u64` 哈希值 -> worker URL；
- `current_workers: RwLock<Vec<String>>`：上一次参与建环的 worker URL 集合缓存，用于 diff。

没有 per-worker 权重、没有负载/容量记录，因此是典型“软粘性、无负载感知”实现。

### 2.2 虚拟节点与环构建

- 常量 `VIRTUAL_NODES_PER_WORKER = 160`（`consistent_hash.rs:21`）；
- 每个 worker 生成 160 个虚拟 key：`"{worker_url}:{i}"`，再取 `fbi_hash` 得到环上位置（`consistent_hash.rs:249` 起）；
- `PolicyConfig::ConsistentHash { virtual_nodes }` 虽然能被配置和校验（>0），但 factory 明确注释“参数未使用、目前硬编码 160”（`src/policies/factory.rs:36`）。**配置项实际不生效**。

### 2.3 哈希函数

- 直接移植 Facebook mcrouter 的 `furc_hash` + `MurmurHash64A`（`consistent_hash.rs:44/153/233`）：
  - `fbi_hash(key) = murmur_hash_64a(furc_hash(key, 2^23-1).to_le_bytes(), seed)`；
  - `furc_hash` 保证在 worker 集合变化时环上绝大多数 key 位置稳定（这是“一致性”的根本来源）。
- 确定性：相同 key + 相同 worker 集合 => 相同结果，单元测试覆盖（`tests/test_consistent_hash_policy.rs`）。

### 2.4 查找

`find_worker_by_hash`（`consistent_hash.rs:290`）：

1. 计算 key 的 `fbi_hash`；
2. 在 `BTreeMap.range(hash..)` 中找第一个 `>= hash` 的虚拟节点；
3. 找不到则回绕到环首（最小 hash 的节点）。

复杂度 O(log(160N))，但重建是 O(160N log(160N))。

---

## 3. 路由 key 的提取（谁决定粘性）

`hash_key::extract_hash_key`（`src/policies/hash_key.rs:29`）优先级为 **Header > Body > 请求内容回退**：

### 3.1 HTTP Header（先到先得，代码顺序）

`SESSION_HEADER_NAMES`（`hash_key.rs:11`）实际顺序：

1. `x-session-id`
2. `x-user-id`
3. `x-tenant-id`
4. `x-correlation-id`（注意：排在 `x-request-id` 前，见 commit `4f56d66`）
5. `x-request-id`
6. `x-trace-id`

匹配后 key 形如 `header:{name}:{value}`。

### 3.2 Body 字段

1. `session_params.session_id` -> `session:{value}`
2. 顶层 `user`（OpenAI 格式）-> `user:{value}`
3. 顶层 `session_id`（legacy）-> `session:{value}`
4. 顶层 `user_id`（legacy）-> `user:{value}`

提取器是“轻量文本扫描”而非完整 JSON 解析（支持双引号/单引号/无引号值），注意与 docs 中优先级表（`docs/load_balancing/README.md`）的 Header 顺序略有出入——代码为准。

### 3.3 回退

- body 长度 > 100：`request_hash:{fbi_hash(body)}`；
- 否则 `request:{body 原文}`。

含义：没有 session/user 的请求，若每次 prompt 都不同，粘性并不成立（按请求内容哈希或原文映射）。

策略声明 `needs_request_text() == true` 且 `needs_headers() == true`（`consistent_hash.rs:455/462`），所以调用侧会把 body 文本和 header map 都传进来。

---

## 4. 调度策略（select 路径）

### 4.1 单 worker 选择

`select_worker_with_headers`（`consistent_hash.rs:331`）流程：

1. 用 `get_healthy_worker_indices` 过滤 `is_healthy() && circuit_breaker().can_execute()`；
2. `update_hash_ring(workers)`：与 `current_workers` diff，集合变化才整体重建环（懒更新，不在 worker 增删事件里即时重建）；
3. 提取 key -> 环上查找目标 URL；
4. 把目标 URL 换算成当前 worker 数组下标（DP 场景见 §5）；
5. 命中且 worker 健康/熔断器放行 -> 返回；否则：
   - 目标不可用、不在当前集合、或环为空 -> 回退到 **healthy 列表中的第一个** worker；
6. 命中/回退都会 `increment_processed()` 并记录 `RouterMetrics`。

**注意**：回退目标不是“哈希上最近可用节点”，而是 `healthy_indices[0]`，取决于调用侧数组顺序；大量 worker 同时不健康时，流量会集中到“第一个健康” worker。

### 4.2 与通用重试的相互作用

regular 路由的 `route_typed_request`（`src/routers/http/router.rs:540`）把“选择 worker + 发请求”整体放进 `RetryExecutor`，**每次重试都重新 select**：

- 若 worker 集合没变、目标 worker 可用，重试仍会命中同一个 worker（保持粘性）；
- 若首次失败把该 worker 的熔断器打开（或健康标记翻转），下一次 select 看到的可用集合变化 -> 环重建，该会话可能被**永久迁移**到别的 worker（这是故障场景下的正确行为，但对“会话内前几轮请求”可能破坏 KV cache 局部性）。

### 4.3 PD（Prefill-Decode）模式

PD 模式里 prefill/decode 是**两套独立的 worker 池、各自独立的 policy 实例**（`PolicyRegistry::set_prefill_policy/set_decode_policy`，创建见 `src/routers/factory.rs:69`）：

- 同一个 key 在两个池分别做一次 consistent hash（`vllm_pd_router.rs` route_chat/completion/transparent 三处，如 `1938`、`2109`、`2331`）；
- `ConsistentHashPolicy::select_worker_pair_with_headers`（`consistent_hash.rs:487`）就是两个独立 select 的组合；**没有实现 prefill/decode 必须落同一物理机的亲和约束**（注释里说的 “share state efficiently” 只是共享同一 hash key）；
- discovery 模式走 `select_worker_with_policy -> policy.select_worker`（`vllm_pd_router.rs:552`），**不传 HTTP header**，只能依赖 body 里的 key；
- direct URL 模式会把 header 转成 `RequestHeaders` 后传给 policy（`vllm_pd_router.rs:1930` 附近）。

### 4.4 环的成员口径不一致（重要实现细节）

`update_hash_ring` 接收的是调用方传入的整个 `workers` 切片，**环不一定只包含健康 worker**：

- regular 模式：`Router::select_worker_for_model`（`router.rs:506`）预先用 `is_available()` 过滤，所以环只含可用 worker；
- PD direct `route_chat`：直接把 registry 的 prefill/decode 全量列表交给 policy（未预过滤），policy 内部再查健康；此时**环里可能包含不健康/熔断 worker**，命中后走 fallback；
- PD direct `route_transparent`：又预先过滤 `is_available()`；
- discovery 模式：每次把实例 map 转成新的 `BasicWorker`（默认健康、无历史熔断），环成员 = 当前未过期的实例集合。

因此“坏 worker 是否立刻离开环”取决于入口路径，而不是策略内部统一保证。

---

## 5. DP-aware（数据并行 rank）支持

- worker URL 形如 `http://host:port@0`、`@1`…，每个 rank 是独立 worker 参与建环（160 vnode 属于该 URL）；
- `extract_dp_info`（`consistent_hash.rs:317`）：
  - URL 带 `@rank` -> 回查时按**完整 URL 精确匹配**；
  - URL 不带 `@rank` -> 按去掉 rank 的 base URL 匹配，命中该物理机上任一可用 rank。
- 这样 DP 各 rank 被当作独立可路由单元，避免所有流量钉在 rank 0。
- add/remove 端（regular `Router`、PD `PdRouterBase`）都会对裸 URL 做 `dp_size` 展开/前缀删除（`pd_router.rs:233`、`router.rs:1007/1156`），环上成员变化由下一请求懒重建感知。

---

## 6. Worker 添加 / 删除 / 健康变化如何影响 consistent hash

### 6.1 添加

添加入口：启动静态配置、REST（`POST /add_worker`、`POST /workers`）、K8s watcher（`src/service_discovery.rs`）、PD ZMQ discovery（`src/routers/http/vllm_service_discovery.rs` 的实例注册）。

通用流程：

1. 先做启动健康检查（`/health`，直到超时）后才注册（如 `router.rs:1007`、`pd_router.rs:233`）；
2. `WorkerRegistry::register`（`src/core/worker_registry.rs:76`）写 URL/model/type/connection 四个索引；
3. `PolicyRegistry::on_worker_added(model_id, hint)`（`src/policies/registry.rs:51`）只维护“model -> policy / worker 计数”，**对 consistent hash 没有主动更新环的动作**；
4. 环在**下一次请求的 select 路径上懒重建**：传入 worker 列表变化 -> diff 失败 -> 全量重建。

效果：新增一个 worker 后，只有哈希落在新增节点覆盖弧段上的 session 会迁移（理论上约 1/(N+1) 的 key），存量 session 保持粘性。

### 6.2 删除

- REST `DELETE /workers/{url}`、`/remove_worker`、K8s pod 删除、discovery 实例过期都会先从 registry 移除；
- `PolicyRegistry::on_worker_removed` 对 model 计数减一，**最后一个 worker 被移除时整个 model 的 policy 实例被 drop**（`registry.rs:99`），下次再添加会新建空环 policy；
- 若 model 仍剩其他 worker，policy 实例保留、环随后续请求懒重建，缺失 worker 的虚拟节点被移除，相关 session 重映射到剩余 worker；
- 删除是“软摘除”语义：worker 从 registry 消失后不再被 select，已在途请求不受影响。

### 6.3 健康变化（软摘除 / 恢复）

后台 `HealthChecker`（`worker_registry.rs:355`、`server.rs:954`）周期健康检查：

- 连续失败达到阈值 -> `is_healthy=false`（registry 不删除，只是状态翻转）；
- 恢复需要连续成功达到阈值才重新标记健康。

对环的影响同样依赖调用方过滤：

- regular 模式（预过滤）下，健康翻转立刻改变 select 传入的列表 -> 触发环重建，session 被迁移/迁回；
- 未预过滤的路径（部分 PD direct 入口）环保留坏 worker，靠每次 fallback 规避。

### 6.4 备注

- `current_workers` 是 `Vec` diff（不是 Set），若上游列表顺序不稳定会触发不必要的重建（正确性无碍，只是 CPU 开销）；
- 环重建持有写锁并同步做 160N 次 hash，属于请求路径上的 O(N) 事件，仅在集合变化时发生。

---

## 7. 服务质量保障（QoS）相关机制

consistent hash 本身不提供以下能力，但 router 的通用层在它之上提供，理解“服务质量”时应整体看：

### 7.1 健康检查（worker 软可用性）

默认（`src/config/types.rs:335`）：`/health`，interval 60s、timeout 5s、失败 3 次下线、成功 2 次恢复。（core `HealthConfig` 的默认值不同：interval 30s/3/2，见 `src/core/worker.rs:243`；运行时以 RouterConfig 传入为主。）

### 7.2 熔断器（请求失败维度的保护）

`src/core/circuit_breaker.rs`：

- Closed -> 连续失败 `failure_threshold` 次 -> Open；
- Open 等待 `timeout_duration` 后 -> HalfOpen；
- HalfOpen 允许探测请求，连续成功 `success_threshold` 次 -> Closed；任一失败 -> 重新 Open。
- RouterConfig 默认（`config/types.rs:362`）：10 失败/3 成功/60s/120s；
- **注意口径差异**：core `CircuitBreakerConfig` 默认是 5/2/30s/60s；`RouterManager::add_worker` 等路径用 core 默认，regular/PD router 启动时用 RouterConfig 值，实际默认可能因入口而异。
- 4xx 客户错误不计为 worker 故障；只有 408/429/5xx 等才按失败记录（`router.rs` record_outcome 逻辑）。

### 7.3 重试与退避

`src/core/retry.rs` + `RetryConfig`（`config/types.rs:302`）：

- 默认最多 5 次尝试（含首次，即 4 次重试）；`disable_retries` 可关；
- 仅重试 `408/429/500/502/503/504`（`is_retryable_status`）；
- 指数退避 + 抖动：50ms 起步、1.5 倍、上限 30s、jitter 0.2；
- 4xx 直接返回不重试。

### 7.4 并发/入站限流

`src/middleware.rs:494` 的并发限制中间件：

- Token bucket + 可选队列；桶满时默认返回 429，可排队（队列满也 429）；
- RouterConfig 默认：`max_concurrent_requests=32768`、`queue_size=100`、
  `queue_timeout_secs=0`（不限时等待；正数才在超时后返回 408）。

这与一致哈希无关，但对整体 SLO 有效。

### 7.5 请求超时/指标

- `request_timeout_secs` 默认 1800s（30min）；
- 每次策略决策记录 `vllm_router_policy_decisions_total`、worker processed/health/circuit state 等指标（`src/metrics.rs`）；
- `consistent_hash` 在 select 命中时打 info 级日志（key/hash/worker），方便排查粘性是否被打破。

### 7.6 服务质量结论

该策略能提供的 QoS 本质上是：

- 会话级“尽力粘性”（软亲和），最大化 KV cache 命中概率；
- 故障时自动摘除/fallback（依赖通用健康检查与熔断）；
- **不提供**负载均衡、容量感知、per-worker 排队/限流、权重分配；
- 热点风险：fallback 全部指向“第一个健康 worker”；无用户级 fairness 控制。

---

## 8. 发现的问题 / 边界情况（供后续改进参考）

1. **`virtual_nodes` 配置被忽略**：所有配置值都建成 160 vnode（`factory.rs:36`）。
2. **动态 worker 的 `policy=consistent_hash` hint 不生效**：`PolicyRegistry::create_policy_from_type`（`registry.rs:169`）没有 `consistent_hash` 分支，遇到该 hint 会 warn 并回退默认 policy。
3. **环成员口径不一致**：regular 预过滤 vs PD direct 未预过滤 vs discovery 临时 worker，导致健康 worker 离开环的时机不统一。
4. **PD “同 session prefill/decode 共享状态”不成立**：只是两个池各自用同一 key 独立选 node，无协同；discovery 路径甚至不读 header。
5. **fallback 无粘性、无均衡**：目标不可用时直接选 healthy 第一个，集合变化后 fallback 目标可能漂移。
6. **重试会打破粘性**：熔断/健康翻转导致环重建时 session 永久迁移。
7. **discovery 模式每次 select 新建 worker 对象**：无真实健康/熔断历史，策略决策依赖“当前实例是否过期”。
8. **多 model PD 场景**：`VllmPDRouter` direct route 直接取全量 prefill/decode worker，`model_id` 入参未参与过滤/建环（`_model_id`），多模型混布时不同模型的 worker 会互相争抢哈希空间。
9. 环重建发生在请求路径且全量 O(160N)，超大集群 + 频繁增删时应评估。

---

## 9. 验证与测试资产

- Rust 单元/集成：`src/policies/consistent_hash.rs` 内测、`tests/test_consistent_hash_policy.rs`（粘性、分布、DP、PD pair、不健康 fallback）；
- Router 级 header 粘性：`src/routers/http/router.rs:1917`；
- 端到端脚本：`py_test/test_consistent_hash_policy.py`；
- 均衡度模拟：`examples/simulate_consistent_hash.rs`（ring vs rendezvous、CV/imbalance、trials）；
- 官方文档：`docs/load_balancing/README.md`（其中 header 优先级表与代码顺序有出入，以 `hash_key.rs:11` 为准）。

---

## 10. 建议使用方式（代码现状下的最佳实践）

- 用 `X-Session-ID` / `X-User-ID` header 携带 key（比 body 扫描快且与协议无关）；
- 单 worker 池用 `--policy consistent_hash`；PD 用 `--prefill-policy consistent_hash --decode-policy consistent_hash`；
- 对“软粘性+KV cache 命中”诉求合适；对负载均衡、容量均衡诉求应选 `power_of_two` / `cache_aware` 或在策略上层叠加；
- 大集群注意默认 160 vnode 的环内存与重建成本；
- 若需“同节点 prefill/decode 亲和”或“header 驱动 discovery 路由”，当前 main 分支未覆盖，需要后续扩展。
