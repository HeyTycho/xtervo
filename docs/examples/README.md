# 示例插件

当前宿主（Plugin API v3）只加载 `api = 3` 且 `host_abi = 3` 的清单；
v1/v2 清单只会显示 `unsupported`，不会执行。

| 目录 | 状态 | 说明 |
|---|---|---|
| [`plugin-v3-echo/`](plugin-v3-echo/) | **当前** | v3 process 示例：命令、状态、事件、面板、设置、需确认的终端读取 |
| [`plugin-wasm-filter/`](plugin-wasm-filter/) | **当前** | v3 WASM 输出过滤器（只有 `terminal.output_filter` 权限） |
| [`ai-echo/`](ai-echo/) | **LEGACY** | v2 AI Chat 示例，宿主已不加载；仅作历史参考 |

完整协议见 [`docs/PLUGIN_V3.md`](../PLUGIN_V3.md)；v1/v2 语义见
[`docs/PLUGIN_API.md`](../PLUGIN_API.md)（已废弃，仅历史参考）。

安装方式：把插件目录复制到 `~/.config/xtervo/plugins/<name>/`，重启 xtervo，
在 设置 → 插件 里确认/启用。process 插件的 `entry` 必须在插件目录内且可执行。
