# Tervo Observability（P0）

状态：**backend-neutral crate + CLI schema 已落地（2026-11-10）**。  
分支：`agent/observability`。

本切片只做可复用、可测试的诊断原语，**不接 OpenTelemetry/tracing 平台**，也
不改渲染/主循环路径。CLI 输出里的计数器在 CLI 进程内是空的：`initialized=false`、
`source=cli`，这是真实的“未接线”状态，不是伪造的 app 指标。

## Crate：`crates/diagnostics`（`tervo-diagnostics`）

不依赖 wgpu / Skia / Slint / `tervo-core`；只有 `serde`/`serde_json`。

```text
MetricsRegistry   原子指标 + 固定 frame-time 直方图
TraceBuffer       有界 trace ring（固定容量，记录 drop/seq）
ResourceTracker   image resources / placements / glyph atlas / GPU textures 计数+bytes
SceneDiff         两个轻量 SceneSummary 的 text/image/damage diff
```

### MetricsRegistry

记录路径 `record_frame()` 只有 relaxed atomic 操作：**无锁、无分配**。字段：

```text
frame_count
frame_time.last_us / avg_us / p95_us / samples / overflow_samples / histogram[12]
damage.regions_last / regions_total / area_last / area_total /
damage.viewport_area_last / area_ratio_last(_ppm) / area_ratio_max(_ppm)
scene_revision
images.instances / images.placements
textures.created / destroyed / live_count_estimate /
textures.live_bytes_estimate / peak_bytes_estimate
presenter_revision
stale_frames
```

`p95_us` 是固定直方图桶上界（近似值），不是精确分位数；超过最后一桶时
`p95_overflow=true`。`snapshot()` 返回 additive `serde_json::Value`，调用方加
`schema`/`kind`。

示例：

```rust
use tervo_diagnostics::{FrameSample, MetricsRegistry};

let metrics = MetricsRegistry::new();
metrics.record_frame(FrameSample {
    cpu_time_us: 1_200,
    damage_regions: 3,
    damage_area: 120,
    viewport_area: 3_840,
    scene_revision: 42,
});
metrics.set_image_counts(2, 2);
metrics.record_stale_frame();
let json = metrics.snapshot();
```

### TraceContext / TraceBuffer

```rust
TraceContext { session_id, update_id, scene_revision, frame_id, placement_id? }
TraceBuffer::new(capacity)  // clamp 1..=4096, default 256
```

- 固定容量；push 满时丢最旧事件并累加 `dropped`。
- `allocated_slots <= capacity`，测试覆盖 50k 次 push。
- `snapshot_json()` 输出 `capacity/allocated_slots/len/dropped/next_seq/events`。
- 只用于低频 trace/error；**不要**对每个字符/每一帧写 message。

### ResourceTracker

```rust
pub trait ResourceTracker: Send + Sync {
    fn snapshot(&self) -> ResourceSnapshot;
}
```

`AtomicResourceTracker` 是默认实现：

```rust
tracker.set_image_resources(count, bytes);
tracker.set_image_placements(count, 0);
tracker.set_glyph_atlas(glyphs, bytes);
tracker.set_gpu_textures(textures, bytes);
```

GPU backend 可以直接实现 trait，避免重复计数。`rgba_bytes(w, h)` 提供饱和
的 `w*h*4` 估算。

### SceneDiff

输入是轻量 `SceneSummary`（cols/rows/text/images/damage），不依赖 renderer：

```text
text    changed_rows / changed_cells / added_rows / removed_rows + 最多 32 行预览
images  added / removed / moved / resized / source_changed
damage  region_count / union area / coverage ppm / coverage delta / 新增删除 rect
```

`SceneDiff::to_json()` 输出 `kind=scene_diff`（additive `tervo.diag.v1`）；
`to_text()` 输出有界文本（行预览和输出大小都有上限）。

## CLI

```bash
tervo diag metrics            # metrics + resources + trace schema
tervo diag observability      # 同上，kind=observability，含 integration 点
tervo diag scene-diff --left before.json --right after.json
tervo diag scene-diff --left before.json --right after.json --json
```

`scene-diff` 同时接受 frame JSON（`generation`、image `id`）和 SceneSummary
JSON（`revision`、image `placement_id`）。`frame_json` 新增了 additive 的
`damage.full/region_count/rects` 字段，旧读取方忽略即可。

`metrics`/`observability` 的真实 app 数据需要下一步接线；当前输出明确标注
`initialized=false`，不返回假计数。

## 集成点（下一步，不在本切片）

```text
crates/app/src/render.rs  Renderer::draw_without_cursor
  -> MetricsRegistry::record_frame(...)
crates/app/src/main.rs    redraw_pane_inner
  -> set_image_counts / record_stale_frame
crates/app/src/main.rs    live_diag_snapshot
  -> merge snapshot()/trace/resources into kind=live
crates/app/src/render.rs  Renderer::image
  -> set_presenter_revision
crates/graphics           GraphicsManager
  -> ResourceTracker counts/bytes
未来 GPU backend           texture create/destroy
  -> record_texture_created/destroyed + set_gpu_textures
```

这些点也可以从 `tervo diag observability` 的 `integration.points` 机器读取。

## 测试

```bash
cargo test -p tervo-diagnostics
cargo check -p tervo-app
```

覆盖：ring 上限/并发写/无界增长、metrics snapshot 字段、texture bytes 饱和、
damage union/coverage、scene diff 计数、diff 输出有界。

## 非目标

```text
OpenTelemetry / tracing 平台
每帧同步 GPU 查询/fence wait
per-character / per-frame info 日志
伪造 live app metrics
```
