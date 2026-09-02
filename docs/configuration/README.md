# Router 配置文件

> 分支：`feat/coding-agent-session-affinity`
> 目标：把“目前只能通过命令行/构造参数设置的控制项”全部收进一个 JSON 配置文件。

## 1. 快速开始

Python 启动器（pip 安装的 `vllm-router` 命令）：

```bash
vllm-router --config examples/configs/router_config.example.json
```

原生 Rust 二进制：

```bash
./target/release/vllm-router --config examples/configs/router_config.example.json
```

命令行参数仍然可用；标量参数以命令行优先：

```bash
vllm-router --config router.json --port 32000   # port 使用 32000，其余来自 router.json
```

## 2. 配置文件的形态

配置文件是**扁平 JSON 对象**。Key 使用各 CLI 的 snake_case 选项名（也就是
`RouterArgs` / `CliArgs` 字段名），Value 就是该选项的值：

```json
{
  "host": "0.0.0.0",
  "port": 30001,
  "worker_urls": ["http://worker1:8000", "http://worker2:8000"],
  "policy": "consistent_hash",
  "retry_max_retries": 3,
  "service_discovery": true,
  "selector": ["app=worker", "env=prod"]
}
```

支持的取值形式：

| JSON 类型 | 含义 | 例子 |
|---|---|---|
| string / number | 标量选项 | `"policy": "consistent_hash"`、`"port": 30001` |
| boolean | 开关 | `"vllm_pd_disaggregation": true` |
| array of scalars | 可重复/多值选项 | `"worker_urls": ["http://a", "http://b"]` |
| array of arrays | `prefill` 条目 | `"prefill": [["http://p:8000", "9000"], ["http://p2:8000"]]` |
| `null` | 可选值留空 | `"log_dir": null` |

`selector` / `prefill_selector` / `decode_selector` 除 `["k=v"]` 数组外，也可以
直接写成对象（`{"app": "worker"}`），Python 入口会自动展开。

开关写 `false` 等价于不写（所有 CLI 开关默认都是 false）；命令行显式给出同一
选项时，配置里的同名单项会被跳过（CLI 优先）。

## 3. 覆盖哪些选项

实现方式不是单独维护一份影子配置，而是把配置文件“翻译”回 CLI token，再交给
现有的 `argparse` / `clap` 解析：

- Python 入口支持 `RouterArgs.add_cli_args` 暴露的**全部**选项；
- Rust 原生入口支持 `CliArgs` 暴露的**全部**选项（含 tracing、backend 等）；
- 两个入口接受的 key 都是各自 CLI 的 snake_case 选项名。

Python 入口的完整字段与
`py_src/vllm_router/router_args.py` 的 dataclass 字段一一对应（少数别名兼容：
`decode_urls` -> `decode`、`prefill_urls` -> `prefill`、
`eviction_interval` -> `eviction_interval_secs`）。
原生入口的字段与 `src/main.rs` 的 `CliArgs` 一一对应。

完整示例见：

- Python launcher：`examples/configs/router_config.example.json`
- 原生 Rust CLI：`examples/configs/router_config.native.example.json`
- consistent_hash 会话路由：`examples/configs/consistent_hash_router_config.example.json`

## 4. 与 `--hash-key-config` 的关系

`hash_key_config` 本身也是一个普通配置项，可以直接写在 router 配置里：

```json
{
  "policy": "consistent_hash",
  "hash_key_config": "examples/configs/consistent_hash_session_config.minimal.json"
}
```

这样“会话标识名单”这类策略级配置与全局 router 配置可以放在同一个文件里引用。

## 5. 优先级规则

1. 先加载配置文件并转成 CLI token；
2. 配置文件 token 在前，真实命令行参数在后；
3. 因此：
   - 标量：命令行 > 配置文件 > 内置默认；
   - 重复/列表：命令行给出同一选项时，命令行整体替换配置文件值；
   - 没有出现在任一处的选项使用内置默认。

## 6. 错误处理

- 文件不存在 / 不是合法 JSON / 顶层不是对象：启动报错并给出文件路径；
- Python 入口遇到未知 key：报 `unknown config option '...'` 并列出可接受选项；
- Rust 入口遇到未知 key：由 clap 报未知参数；
- `config` 不能嵌套在配置文件内部（避免递归加载）。

## 7. 实现位置

| 层 | 文件 |
|---|---|
| Python 加载/翻译 | `py_src/vllm_router/config_file.py` |
| Python 入口接线 | `py_src/vllm_router/launch_router.py` |
| Rust 加载/翻译 | `src/main.rs`（`config_path_from_args`、`config_to_cli_args`、`parse_cli_and_prefill`） |
| 示例 | `examples/configs/router_config.example.json` |
| 测试 | `py_test/unit/test_config_file.py`、`src/main.rs` tests |

## 8. 当前限制

- 配置文件面向**单个入口**：Python 入口不认识 backend/tracing 等仅原生 CLI
  有的选项，原生入口不认识 `mini_lb` 等仅 Python launcher 有的选项；
- `prometheus_port` 等在 Python argparse 里有默认值，写 `null` 不会改变该默认。
