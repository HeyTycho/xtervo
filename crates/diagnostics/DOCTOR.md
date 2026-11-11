# Tervo Doctor Core（确定性诊断底座，D0 slice）

状态：2026-11-11 晚，worktree `.worktrees/agent-doctor-core`，分支 `agent/doctor-core`。
范围：只改 `crates/diagnostics`。不接模型、不建窗口、不读整段 scrollback、不访问网络、不改 app UI、不碰 `:1`。

## 1. 交付面

| 组件 | 位置 | 说明 |
|---|---|---|
| 库 API | `tervo_diagnostics::doctor` | `Doctor` / `DoctorInput` / `DoctorConfig` / `DiagnosticCase` |
| CLI | `cargo run -p tervo-diagnostics --bin tervo-doctor -- ...` | `check` / `issue` / `schema` / `env` / `version` |
| Schema | `tervo-doctor schema`，常量 `DIAGNOSTIC_CASE_SCHEMA` | `tervo.diagnostic_case.v1` |
| Issue 模板 | `tervo_diagnostics::doctor::IssueDraft` | 固定八节：现象/环境/复现/证据/第一处分歧/已排除/建议/回归测试 |
| 证据 | `EvidenceBundle` | 脱敏、上限、逐项 SHA-256、bundle SHA-256、`verify()` |

`tervo diag` 的接线方式（本 slice 不改 app）：

```rust
use tervo_diagnostics::doctor::{Doctor, DoctorConfig, DoctorInput, Environment};

let input = DoctorInput {
    symptom: tervo_diagnostics::doctor::Symptom::reported("滚动后残影"),
    environment: Environment::from_process(),
    frame: None,          // 由 diag snapshot JSON 解析后填入
    metrics: None,        // 由 diag metrics JSON 解析后填入
    render_profile: None, // 由 TERVO_RENDER_PROFILE 日志解析后填入
    attachments: vec![],
};
let case = Doctor::new(DoctorConfig::default()).run(&input)?;
println!("{}", case.to_json_pretty()?);
```

现有 `tervo.diag.v1` 适配器：

```rust
use tervo_diagnostics::doctor::{Environment, FrameSummary, MetricsSummary, RenderProfileSummary};

let env = Environment::from_diag_env_json(&tervo_diag_env_value)?;
let frame = FrameSummary::from_diag_json(&tervo_diag_snapshot_value)?;
let metrics = MetricsSummary::from_diag_json(&tervo_diag_metrics_value)?;
let profile = RenderProfileSummary::from_text("render.log", &log_text, Some(true), None);
```

## 2. DiagnosticCase schema（v1）

顶层字段固定为：

```text
schema          "tervo.diagnostic_case.v1"
case_id         "case-<16 hex>"，输入确定性哈希，无时间戳/随机数
symptom         reported/summary/details/repro_steps/expected/actual
environment     version/commit/dirty/build_time/target/os/display/session/renderer/config/log...
evidence        EvidenceBundle
probes          Probe[]（id/layer/status/summary/evidence/command）
findings        Finding[]（id/probe/severity/title/detail/evidence/confidence/falsify/next_probe）
recommendations Recommendation[]（id/title/action/kind/evidence/command/related_findings）
issue_draft     title/markdown/labels
regression_test status/name/command/expected/evidence/note
```

`tervo-doctor schema` 输出完整 JSON Schema（draft 2020-12）。`additionalProperties: true`，后续字段可以增量添加；顶层 `schema` 版本号不变时表示兼容扩展。

### 缺失数据规则

- `Option` 字段缺失就是 `null`，不会填 `0`、`unknown`、`false` 或“默认 renderer”。
- 缺 frame / metrics / profile：对应 probe 为 `unknown` 或 `skipped`，不是 `pass`。
- CLI `tervo diag metrics` 的 `initialized=false/source=cli` 占位数据会被明确标记并跳过，零值不参与健康判断。
- `damage.full` 缺失时不计算整屏覆盖率；`frame.text` 只接受有界视口摘要。

## 3. 健康检查（固定顺序）

| Probe | 层 | 检查 |
|---|---|---|
| `probe.build` | build | version/commit/build_time/target 是否存在；dirty 告警 |
| `probe.renderer` | renderer | active/requested/fallback/fallback_reason；fallback/mismatch |
| `probe.gpu` | gpu | adapter/backend/driver；软件 adapter（llvmpipe/swiftshader…）告警 |
| `probe.config` | config | 路径存在/可读/UTF-8；疑似明文凭据扫描（不保存值） |
| `probe.paths` | files | config/profiles/log/log_dir 类型、可读性、大小/mode |
| `probe.frame` | frame | 尺寸/行数/text 截断/图片越界/skipped |
| `probe.damage` | damage | full/region_count/rects/area/coverage；首帧整屏与契约冲突 |
| `probe.render_profile` | profile | `key=number(ms/us/ns/s)` 阶段统计；慢阶段阈值 33 ms |
| `probe.metrics` | environment | 仅 `initialized=true` 的 live metrics 参与 p95/damage/stale 判断 |
| `probe.evidence` | evidence | bundle 项数/字节/截断/省略 |

每个 `finding` 必须有非空 `evidence`，且引用必须存在于 bundle；`DiagnosticCase::validate()` 会拒绝无证据 finding。所有结论都带 `confidence` 与可选的 `falsify` / `next_probe`。

`first_divergence()` 先按层顺序找 `fail`，没有 fail 再找最早 `warn`。没有分层探针数据时 issue 明确写“未定位”，不假装完成 bytes→frame 的定位。

## 4. 证据 bundle 与脱敏

- 默认上限：单项 16 KiB、总量 64 KiB、最多 64 项；超出会 `truncated` 或 `omissions`，不静默丢弃。
- 逐项 `sha256` 是**实际存储内容**的哈希；bundle `sha256` 覆盖有序 manifest、总字节与 omissions。
- `verify()` 可检测内容篡改、项删除、总量/上限不一致。
- 脱敏规则：`api_key/token/secret/password/authorization/cookie/private_key...` 键值、Bearer/Basic、JWT、常见 token 前缀、URL userinfo + query 凭据、PEM/PGP 私钥块、嵌套 JSON 敏感键。
- 报告只保留规则名与计数，不保留匹配值；标签与 issue 动态字段再次脱敏。
- 不读 scrollback：只接受显式传入的 frame JSON（视口文本再裁剪到 40 行 × 200 列）和显式附件（头/尾最多 64 KiB，入库再受 16 KiB 上限）。
- config 扫描只保存元数据、计数和哈希，不保存配置内容。

## 5. CLI 示例

```bash
# 环境 + 视口帧 + profile + 日志尾（全部只读）
tervo diag env --json > /tmp/env.json
tervo diag snapshot --json > /tmp/frame.json
TERVO_RENDER_PROFILE=1 tervo 2>&1 | tee /tmp/render.log

target/debug/tervo-doctor check \
  --diag-env /tmp/env.json \
  --frame /tmp/frame.json \
  --render-profile /tmp/render.log \
  --attach-tail app=/path/to/tervo.log \
  --symptom "滚动后残影" \
  --out /tmp/case.json

target/debug/tervo-doctor issue --case /tmp/case.json --out /tmp/issue.md
target/debug/tervo-doctor schema --out /tmp/tervo.diagnostic_case.v1.json
```

返回码：`0` 正常；`1` 输入/IO 错误；配置 `--fail-on warn|error` 时达到阈值返回 `2`。

## 6. 验证命令

```bash
CARGO_HOME=.cargo-home-scene-cpu cargo test -p tervo-diagnostics --offline
CARGO_HOME=.cargo-home-scene-cpu cargo clippy -p tervo-diagnostics --all-targets --offline

# 可选 2 分钟库级内存门禁（不启动窗口，不接 app 主循环）
CARGO_HOME=.cargo-home-scene-cpu cargo test -p tervo-diagnostics \
  --test doctor_soak --offline -- --ignored --nocapture
```

2026-11-11 实测：43,605 次 case 生成，RSS 4048→4144 KiB，threads 2→2，FD 4→4。

## 7. 明确限制（本 slice）

- 没有 app 内接线：`tervo diag doctor` 子命令尚未添加；本 slice 提供库 API + 独立 CLI。
- 没有 live renderer/GPU 适配器字段：现有 `tervo.diag.v1 env` 只有 backend 请求；GPU adapter/active/fallback 需调用方补充（CLI flags 或 additive env 字段），缺失就报 Unknown。
- 没有接 live app metrics：`tervo diag metrics` 是 CLI 占位，doctor 会跳过而不是把 0 当健康。
- config 只做可读性/UTF-8/凭据扫描，不做完整 TOML 语义解析；避免在诊断器里重复 app 的迁移逻辑。
- 不生成/提交 issue，不自动创建回归测试文件；`regression_test` 只是草案/状态，避免伪造测试名。
- 无网络、无 AI provider、无 shell 执行；不读 scrollback，不创建窗口/PTY。
