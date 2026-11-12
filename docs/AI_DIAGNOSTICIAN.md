# Tervo AI 诊断器（不是聊天框）

状态：方向文档（2026-11-11）。  
目标不是“在终端里放一个 ChatGPT 输入框”，而是：

```text
观察终端 -> 形成结构化证据 -> 分层定位 -> 给出可验证优化/修复
-> 生成 issue/回归测试 -> 分析反馈聚类 -> 闭环
```

AI 是诊断/推理层，不是事实来源。事实由 Tervo 的确定性诊断工具产生。

## 1. 用户场景

### A. 终端变慢/花屏/图片异常

用户描述：

```text
滚动后旧字符残留
图片在错误行
输入延迟高
GPU 打开后更慢
```

Doctor 不先“猜”，而是：

```text
收集环境/版本
采集 frame / damage / renderer / GPU / soak 指标
跑分层探针
给出第一处分歧
提出配置/代码/回归测试建议
验证前后指标
```

### B. 提交 issue

用户点击“诊断并准备 issue”：

```text
自动生成脱敏证据包
复现步骤
版本/commit/renderer
frame/日志/指标摘要
AI 生成 issue 草稿
用户确认后才提交
```

### C. 分析已有 issue/反馈

输入 issue 列表/反馈导出：

```text
按层聚类：字节/Filter、Term、Frame、renderer、GPU、UI、配置
提取复现模式
找重复 root cause
生成回归测试建议/优先级
```

### D. 终端优化

不是泛泛建议，而是基于测量：

```text
frame p95 高 -> CPU raster / upload / Slint / GPU 哪一段
full redraw 多 -> damage 契约
RSS 涨 -> texture/atlas/cache
GPU fallback -> adapter/log
字体慢 -> fallback/atlas
```

## 2. AI Chat 是插件，不是核心

产品分层：

```text
核心（first-party，可信）:
  DiagnosticCase / evidence / probes / Doctor
  分层定位 / 优化验证 / issue 草稿
  tool permission / approval
  host-rendered plugin slots

插件（third-party，可选）:
  AI Chat 面板
  模型 provider
  领域助手/诊断解释
  外部 issue/反馈集成
```

因此：

```text
没有安装 AI 插件 -> Doctor 仍能离线工作
安装 chat 插件   -> 多一个聊天入口，不替代 Doctor
插件 provider    -> 只提供模型调用，不拥有诊断事实
```

插件约束：

```text
不能直接读任意 PTY/scrollback
只能通过 host context API 获取用户批准的范围
不能绕过 tool approval 执行命令
不能自动提交 issue
不能带任意 Slint/native UI
```

宿主提供的插件能力（建议 v3）：

```text
contributes.chat_panel
chat.message / ai.chunk / ai.done 流式协议
context.request -> host 返回脱敏/裁剪后的 ContextBlock
diagnostics.read -> 仅限当前 DiagnosticCase
issue.draft -> 返回草稿，提交由宿主确认
```

## 3. 非目标

```text
不做通用聊天框作为主界面
不让 AI 直接执行 shell
不让 AI 自动提交 issue
不让 AI 编造 metrics/根因
不把整段 scrollback 默认上传
不做“AI 重写终端”的不可审查 patch
```

允许有“追问/解释”的聊天，但它是诊断 case 的辅助，不是产品主体。

## 4. 数据模型

```text
DiagnosticCase
  id
  symptom
  environment
  evidence[]
  probes[]
  hypotheses[]
  findings[]
  recommendations[]
  verification[]
  issue_draft
  regression_test
```

Evidence 必须是结构化、可哈希、可复现：

```text
EnvironmentEvidence
  version/commit/build time
  OS/display/session type
  renderer: skia/wgpu/software
  GPU adapter/driver
  font config
  terminal size/DPI
  config redacted

RuntimeEvidence
  frame snapshot / frame dump
  damage rows / scene diff
  raw PTY (opt-in, bounded)
  render profile
  metrics counters
  logs (bounded)

PerformanceEvidence
  frame p50/p95
  CPU raster / GPU upload / Slint render
  upload bytes/frame, draw calls
  RSS/PSS/threads/fds
  soak samples

ImageEvidence
  placement rows/source_rect/cells/z
  texture lifetime/bytes
  image protocol events

IssueEvidence
  repro steps
  expected/actual
  screenshots
  known similar issues
```

## 5. 分层探针

必须按 [`POSTMORTEM_2026-11-11_RENDER_RESIDUE.md`](POSTMORTEM_2026-11-11_RENDER_RESIDUE.md)
的阶梯：

```text
1. raw bytes vs filter bytes
2. filter vs Term
3. Term vs partial/full Frame
4. Frame vs incremental/full framebuffer
5. framebuffer vs Slint/snapshot
6. config/GPU/driver outside
```

Deterministic probes 先跑，LLM 只做：

```text
选择下一个探针
解释证据
排序假设
生成修复/测试建议
```

## 6. 工具接口（AI 可调用，但受控）

```text
env.collect()
diag.snapshot()
diag.trace_layer(input)
diag.frame_diff(a,b)
diag.metrics()
perf.benchmark(scenario)
soak.run(duration)
image.audit()
logs.query(filter)
issue.search(query)
test.run(crate/test)
git.diff()
```

约束：

```text
只读工具默认允许
benchmark/soak 在隔离 Xvfb/临时 HOME 运行
patch/test 只在 worktree，不自动 commit
issue.submit 需要用户确认
任何网络/凭据操作显式
```

## 7. 诊断流程

```text
symptom
  -> scope: 何时/哪个 pane/什么操作/最近变更
  -> collect bounded evidence
  -> deterministic probes
  -> first divergence
  -> hypothesis ranking
  -> verification probe
  -> recommendation:
       config change
       code fix plan
       regression test
       issue draft
  -> user approval
  -> benchmark/soak before/after
  -> issue submit / PR
```

每个 finding 必须带：

```text
evidence refs
confidence
what would falsify it
next probe
```

没有证据的结论标 `hypothesis`，不能标 `PASS`。

## 8. Issue 生成

模板：

```markdown
## 现象
## 环境
## 复现
## 证据
- commit/build
- renderer/GPU
- frame dump / trace / metrics
- screenshot
## 第一处分歧
## 已排除
## 建议
## 回归测试
```

默认脱敏：

```text
API key/token/password
SSH/URL 凭据
用户目录可配置
可选 raw bytes/截图
```

提交方式：

```text
本地 markdown
剪贴板
GitHub/GitLab（显式授权）
```

## 9. 反馈/issue 聚类

输入：

```text
issue export / feedback JSON / crash reports
```

输出：

```text
按层聚类
重复 root cause
受影响版本/平台
回归测试建议
优先级/修复 owner
```

不是“AI 随便总结”，而是规则 + 嵌入/关键词 + 人工可审。

## 10. UI

不是 Chat 首屏，而是 **Doctor 面板**：

```text
Health checks
Findings（带证据）
Evidence viewer
Recommendations（Apply/Preview）
Benchmark before/after
Issue draft
```

聊天只作为“追问这个 finding”的辅助入口。

## 11. 分阶段

### D0：确定性 Doctor（先做）

```text
live metrics/诊断数据接线
DiagnosticCase schema
tervo doctor CLI
环境/渲染/GPU/配置健康检查
证据 bundle（脱敏）
```

验收：

```text
无网络也能诊断常见问题
每个 finding 有证据
不伪造 metrics
```

### D1：AI 诊断 loop（mock provider）

```text
AiProvider（mock/真实）
case -> tools -> hypothesis -> verify
issue draft
```

验收：

```text
能基于真实证据定位一个已知 bug
错误假设会被探针否证
不自动执行/提交
```

### D2：终端优化验证

```text
benchmark/soak before/after
配置建议
代码修复 worktree
回归测试生成
```

### D3：issue 提交与反馈聚类

```text
用户确认后提交
issue/反馈聚类
回归测试/优先级
```

### D4：插件 AI provider

```text
第三方模型/诊断 provider
权限/凭据隔离
```

## 12. 和 AI 对话的关系

```text
Chat = 辅助解释/追问
Doctor = 产品主体
```

用户说“终端卡”，Doctor 不应该回一段泛泛建议；应该：

```text
采证 -> 对比 CPU/GPU/upload/atlas -> 给出具体 finding
-> 提供可验证修改 -> 前后 benchmark
```

## 13. 成功标准

```text
1. 用户报“滚动残影”，Doctor 能定位到第一处分歧并生成回归测试
2. 用户报“GPU 变慢”，Doctor 能区分 CPU raster/upload/Slint/adapter
3. issue 草稿包含可复现证据，不需要维护者反复问版本/环境
4. 一批 issue 能聚类出重复 root cause 和回归缺口
5. 所有建议有证据、可验证、需用户批准执行
```

## 14. 与当前诊断系统

已有：

```text
tervo.diag.v1 env/snapshot/replay/trace
metrics/observability/scene-diff
live socket/MCP
TERVO_FRAME_DUMP / TERVO_LINE_NUMBERS
TERVO_RENDER_PROFILE
soak/e2e/CI gate
```

缺口：

```text
live app metrics/trace 接线
DiagnosticCase schema/bundle
trace-layer CLI
AI tools + provider
issue draft/feedback pipeline
Doctor UI
```

因此 AI 诊断器的第一步不是接模型，而是把 **Doctor 确定性层** 做扎实。
