# Consistent Hash 下的服务质量排队（per-worker admission gate）

> 适用场景：`NewAPI -> vllm-router -> vLLM worker`，router 使用
> `consistent_hash` 保会话/首轮 prompt 的 KV locality，但 worker 算力有限，
> 突发请求会让已经开始的 agent 对话整体变慢。

## 1. 要解决的问题

vLLM 的 `--max-num-seqs` 只是 scheduler 的运行上限，不是服务质量目标。请求一多，
scheduler 会把 running 队列填满，`vllm:num_requests_running` 一直贴着上限，
每个会话的 TTFT/TPOT 都会劣化。router 在转发前排队，可以把“已经进入 vLLM 的
请求数”压到体验良好的并发值以下，让新请求在 router 侧等待，而不是挤占正在推理
的会话。

router 里原有的 `max_concurrent_requests / queue_size` 是全局 token bucket，
并且 streaming 场景下 token 在 worker 返回响应头后就被释放（真正的 SSE 流还在
后台转发），所以它无法保护正在 decode 的会话，也无法按 worker 隔离排队。新增的
per-worker admission gate 弥补这两个缺口。

## 2. 工作原理

```
NewAPI
  └─ vllm-router (consistent_hash)
      1) 提取 session key（header / body / first-user-prompt）
      2) consistent hash 选定 worker（不破坏 KV locality）
      3) 向该 worker 申请“在途席位”
           ├─ 未满  → 立刻转发
           └─ 满    → 进入该 worker 的有界队列（最多 worker_queue_size 个）
      4) 转发 / SSE
      5) body 读完 / SSE 结束 / 客户端断开 → 释放席位，放行队列中下一个
```

关键性质：

- **先选路，后限流**：排队不改变 consistent hash 的映射。worker 忙时请求等待这台
  worker，而不是 fallback 到别的 worker 丢掉 KV cache。
- **在途计数覆盖完整生命周期**：streaming 请求的席位要等 `[DONE]` / 流结束 /
  通道断开才释放，和 `send_typed_request` 中 worker load 的释放点一致。
- **每个 worker 独立排队**：一台 worker 满不会堵住其他空闲 worker。
- **有界队列 + 不限时等待（默认）**：超出 `worker_queue_size` 的请求直接返回
  429；队列内的请求默认无限等待（`queue_timeout_secs=0`），直到拿到在途席位或
  客户端断开。这样高负载时连接会被 router 一直保持，agent 客户端不会因为 router
  主动返回 408 而开始“超时-重试-再超时”的循环。
- **408 只是显式配置的兜底**：只有当给 `queue_timeout_secs` 配置了正数时，
  排队超过该时长才返回 408。429 / 408 都是 router 自身产生的响应，不会触发
  worker 熔断或 router 内部重试风暴。
- **默认关闭**：不设置 `max_concurrent_requests_per_worker` 时行为与之前一致。

## 3. 配置

命令行：

```bash
vllm-router \
  --policy consistent_hash \
  --worker-urls http://worker1:8000 http://worker2:8000 \
  --max-concurrent-requests-per-worker 24 \
  --worker-queue-size 100
```

> 排队等待时长通过 JSON 配置里的 `queue_timeout_secs` 控制（CLI 暂未暴露该
> 参数）：`0` 表示不限时等待（推荐），正数表示最多等待这么多秒后返回 408。

JSON 配置文件：

```json
{
  "policy": "consistent_hash",
  "worker_urls": ["http://worker1:8000", "http://worker2:8000"],
  "max_concurrent_requests_per_worker": 24,
  "worker_queue_size": 100,
  "queue_timeout_secs": 0
}
```

完整示例见
`examples/configs/consistent_hash_qos_config.example.json`。

选项说明：

| 选项 | 默认 | 说明 |
|---|---|---|
| `max_concurrent_requests_per_worker` | `null`（关闭） | 每台 worker 允许同时在途的请求数；设 `0` 等价关闭 |
| `worker_queue_size` | `100` | 每台 worker 的排队容量；`0` 表示满了直接 429 |
| `queue_timeout_secs` | `0` | 在 worker 队列中的等待上限（秒）；`0` 表示不限时等待、保持连接直到拿到席位或客户端断开，正数表示超时返回 408（兜底） |

Python `Router` 构造参数同名：

```python
Router(
    policy=PolicyType.ConsistentHash,
    worker_urls=["http://worker1:8000", "http://worker2:8000"],
    max_concurrent_requests_per_worker=24,
    worker_queue_size=100,
    queue_timeout_secs=0,
)
```

## 4. 如何把 24 这类值调出来

建议先看 vLLM worker 的 Prometheus：

```bash
curl -s http://worker:8000/metrics | grep '^vllm:num_requests_running'
curl -s http://worker:8000/metrics | grep '^vllm:num_requests_waiting'
```

在“体验还正常 / 开始劣化”两个状态下各记录一次 running 数量。取劣化前那个值作为
`max_concurrent_requests_per_worker` 的起点，再逐步调整。vLLM 的 `max_num_seqs`
建议保持为 scheduler 天花板（一般不小于该值），真正决定体验的是 router 侧的
在途上限。

## 5. 观测指标

启用后 router 在 Prometheus 上新增：

- `vllm_router_worker_inflight_requests{worker}`：当前在途请求数
- `vllm_router_worker_queued_requests{worker}`：当前排队请求数
- `vllm_router_worker_admission_rejects_total{worker,reason}`：
  `queue_full` / `queue_timeout` 计数（`queue_timeout_secs=0` 时不会有
  `queue_timeout` 计数）

配合 vLLM 侧的 `vllm:num_requests_running` 和 TPOT/TTFT，可以确认门禁是否把
running 压到了目标区间。

## 6. 注意事项

- 门禁作用于 router 转发给 worker 的常规生成请求和 transparent proxy 路径
  （包括 `/v1/responses`）。如果 embedings / rerank 流量很大，它们也会占用
  在途席位，请把上限统一视为“worker 并发请求数”。
- 原有的全局 `max_concurrent_requests / queue_size` 仍然生效，它是 router
  入口的“第一道闸”；per-worker gate 是选路后的“第二道闸”。调优 worker 上限时，
  建议把全局 `max_concurrent_requests` 保持在明显高于 `worker 数 ×
  max_concurrent_requests_per_worker` 的水平，否则请求会在选路前就被全局闸
  拒绝/排队。
- 这是 router 侧的第一版 per-worker 排队（FIFO 有界队列）。同一会话的
  续轮优先级、per-session 并发上限可以作为下一步扩展；当前仍能保证新请求
  不会直接压进一台满载 worker。
- 目前 PD 模式（prefill/decode disaggregation）还没有接入这个门禁，它面向
  regular / consistent_hash 模式。
