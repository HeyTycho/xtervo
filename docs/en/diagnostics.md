# Diagnostics

`xtervo diag` is an offline diagnostics CLI. It does not create a Slint window and does not need `DISPLAY`, so it can be called from a plain shell, a CI job, or any agent harness. Outputs use the versioned `xtervo.diag.v1` schema; fields are additive, and readers should ignore unknown keys.

```text
xtervo diag env --json
xtervo diag snapshot --json
xtervo diag replay --input trace-raw.bin --json
xtervo diag trace --input trace-raw.bin --out trace.jsonl
xtervo diag trace-layer --input trace-raw.bin --json
xtervo diag doctor [--issue] [--frame FILE] [--metrics FILE]
xtervo diag metrics
xtervo diag observability
xtervo diag scene-diff --left before.json --right after.json --json
xtervo diag serve            # MCP stdio server
```

For live terminal control and the full tool list, see [MCP](mcp.md). For the running app's log path and `xtervo --diagnose`, see [Logging](logging.md).

## `env`

```bash
xtervo diag env --json
```

`xtervo diag --json` is equivalent. The command reports the diagnostic process and the paths it would use; it does not read environment-variable values, Vault contents, or API keys.

| Field | Meaning |
| --- | --- |
| `schema`, `kind` | `xtervo.diag.v1`, `env` |
| `version`, `commit`, `dirty`, `built`, `target` | Build identity |
| `pid` | Diagnostic process ID |
| `display`, `wayland_display`, `session_type`, `desktop` | Session metadata |
| `backend` | Requested `SLINT_BACKEND`, or `null` when unset; this is not the confirmed renderer |
| `platform_hint` | `x11`, `wayland`, or `unknown`, inferred from the environment |
| `xwayland` | Whether an XWayland session is likely |
| `dri` | Whether `/dev/dri` exists |
| `config`, `profiles` | Config and profile paths |
| `log` | Log file path when the process has initialized logging; normally `null` for this CLI process |

## `snapshot`

`snapshot` starts a one-shot PTY, runs a shell command, waits, and captures the current terminal frame. It does not attach to a running xtervo window.

```bash
xtervo diag snapshot --shell /bin/sh --wait-ms 300 -- -c 'printf "hello\n"'
```

| Option | Default |
| --- | --- |
| `--cols N` | `120` |
| `--rows N` | `32` |
| `--wait-ms N` | `250` (capped at 30,000) |
| `--shell PATH` | `/bin/bash` |
| `-- <args...>` | Passed to the shell |

The returned frame contains `text` (one entry per row; trailing whitespace is trimmed), `cursor` (`row`, `col`, `visible`, `shape`), `mode` (raw `TermMode` bits), `display_offset`, `images[]` (`id`, `row`, `col`, `width`, `height`; no RGBA data), `damage` (`full`, `region_count`, `rects`), `generation`, `cols`, `rows`, and build fields.

The command runs locally with your user privileges. Do not pass untrusted input to `snapshot`.

## `replay`

`replay` feeds a recorded raw-byte file through a real PTY/core path and captures the resulting frame:

```bash
xtervo diag replay --input bytes.bin --cols 120 --rows 32 --wait-ms 400
```

The input path is converted to an absolute path before the one-shot pane starts. `--cols`, `--rows`, and `--wait-ms` have the same defaults as `snapshot`, except `--wait-ms` defaults to `400`. The frame fields are the same, including image rectangles without pixel data.

## `trace`

`trace` replays raw bytes and writes one JSON line whenever the frame generation changes:

```bash
xtervo diag trace --input bytes.bin --out trace.jsonl --duration-ms 1000
```

| Option | Default |
| --- | --- |
| `--input FILE` | Required; raw PTY bytes |
| `--out FILE` | Required; JSONL output |
| `--duration-ms N` | `1000` (clamped to 50–30,000) |
| `--cols N` / `--rows N` | `120` / `32` |

Each line has `schema`, `kind: "trace"`, `seq`, `ts_ms`, `generation`, `input`, and the frame fields. If the frame never changes during the window, one final frame is written.

`trace` is a **frame-change stream**, not a full PTY event stream. The input file preserves the original bytes; PTY/damage events are not recorded yet.

## `trace-layer`: first divergence

`trace-layer` compares each layer of the pipeline and stops at the first divergence. Use it for render residue, missing text, image placement, or a filter that appears to swallow bytes.

```bash
# Synthetic fixtures, no input file needed
xtervo diag trace-layer --fixture clean
xtervo diag trace-layer --fixture images --json

# Real recorded bytes
xtervo diag trace-layer --input .logs/run/pty/app-to-host.raw --chunk 4096 --json
```

| Option | Meaning |
| --- | --- |
| `--input FILE` | Raw PTY bytes; `-` reads stdin |
| `--fixture NAME` | `clean`, `images`, or `filter-edge` |
| `--options FILE` | JSON with `cols`, `rows`, `cell_width`, `cell_height`, `chunk`, `scrollback` |
| `--cols N` / `--rows N` | Override dimensions (default `120x32`; a nearby `frame-snapshot.json` is read automatically when no `--options` is given) |
| `--chunk N` | Filter/Term chunk size; default `16384` (use `4096` to stress a real dump) |
| `--cell-width` / `--cell-height` | Cell geometry for image-crop auditing; default `10x20` |
| `--no-images` | Filter image protocols without decoding them; the image layer is `SKIPPED` |
| `--json` | Machine-readable output |
| `--fault SPEC` | Self-test only; do not use for production diagnosis |

Layers:

```text
filter       raw bytes vs. an independent protocol ledger (Sixel DCS, OSC 1337;File,
             Kitty APC); non-image bytes must be preserved byte-for-byte, then
             chunked and one-shot filter outputs are compared
term         filtered bytes chunked vs. fed once; Term cells/cursor/mode compared
frame        production incremental frame vs. a fresh force-full frame
framebuffer  renderer cell model updated by damage vs. Term truth
image        decoded placement anchors and row-crop geometry
```

Layer status is `PASS`, `FAIL`, or `SKIPPED`; the overall `status` is `PASS` or `FAIL`. When no mismatch is found, `first_divergence` is `null`. When there is one, it identifies `layer`, `check`, `at` (byte/row/col/step plus the affected chunk range), `left`/`right` (value, hex, cell, or missing), `context`, and a plain-language `explain`.

The CLI always exits `0`; scripts should read `status` from the output instead of relying on the exit code.

`--fault` injects a known defect to prove the locator works, for example:

```bash
xtervo diag trace-layer --fixture clean --fault filter-drop=6 --json
xtervo diag trace-layer --fixture clean --fault frame-residue=1:2 --json
xtervo diag trace-layer --fixture clean --fault damage-drop=0 --chunk 7 --json
```

## Doctor

Doctor is a deterministic, offline, read-only health check. It does not start a shell or PTY, does not call a model or network service, and does not write files.

```bash
xtervo diag doctor                       # xtervo.diagnostic_case.v1 JSON
xtervo diag doctor --issue               # eight-section issue markdown
xtervo diag doctor --frame /tmp/frame.json --metrics /tmp/metrics.json
```

The CLI uses the current process environment and accepts optional `--frame` and `--metrics` JSON files. It has no symptom flag; to provide a symptom, repro steps, expected/actual behavior, or inline data, call `xtervo.doctor_check` through MCP (see [MCP](mcp.md)).

The doctor runs these probes in a fixed order:

| Probe | Layer | Checks |
| --- | --- | --- |
| `probe.build` | build | version/commit/build time/target presence; dirty working tree warning |
| `probe.renderer` | renderer | requested/active/fallback renderer and fallback reason; unexpected fallback or mismatch |
| `probe.gpu` | gpu | adapter/backend/driver; software adapters such as llvmpipe/swiftshader are warnings |
| `probe.config` | config | path exists, readable, UTF-8; secret-like value scan without retaining values |
| `probe.paths` | files | config/profiles/log/log-dir type, readability, size, and mode |
| `probe.frame` | frame | dimensions, rows, text truncation, out-of-bounds images, skipped input |
| `probe.damage` | damage | full/region count/rectangles/area/coverage; full-screen first frame vs. contract |
| `probe.render_profile` | profile | stage timings in ms/µs/ns/s; slow stages above 33 ms |
| `probe.metrics` | environment | only live metrics with `initialized = true` participate; zero placeholders are skipped |
| `probe.evidence` | evidence | bundle item count, bytes, truncation, omissions |

Each finding has an ID, probe, severity (`info`, `warning`, or `error`), title, detail, evidence references, confidence, and optional `falsify`/`next_probe` fields. A finding must cite at least one item that exists in the evidence bundle; the case validator rejects findings without evidence.

`first_divergence` is the first `fail` probe in layer order, or the earliest `warn` when there is no fail. If no layered probe data was supplied, the issue says the problem is not localized rather than pretending that a layer was identified.

Missing inputs stay missing: a missing frame, metrics snapshot, or render profile produces `unknown` or `skipped`, never `pass`. Placeholder metrics from `xtervo diag metrics` (`initialized = false`, `source = cli`) are skipped instead of being treated as healthy zeros. A missing `damage.full` does not produce a full-screen coverage result, and `frame.text` is only a bounded viewport summary.

The output is a `xtervo.diagnostic_case.v1` document with:

```text
schema, case_id
symptom          reported/summary/details/repro_steps/expected/actual
environment      version/commit/build/time/target/os/display/session/renderer/config/log/paths
evidence         bounded, redacted EvidenceBundle
probes[]         id, layer, status, summary, evidence, command
findings[]       id, probe, severity, title, detail, evidence, confidence, falsify, next_probe
recommendations[] id, title, action, kind, evidence, command, related_findings
issue_draft      title, markdown, labels
regression_test  status, name, command, expected, evidence, note
```

`--issue` prints the fixed eight-section template: symptom, environment, reproduction, evidence, first divergence, ruled out, recommendations, and regression test. Doctor does not create or submit an issue, push code, or write a regression test file; `regression_test` is a draft/status only.

## Metrics, observability, and scene diff

```bash
xtervo diag metrics
xtervo diag observability
xtervo diag scene-diff --left before.json --right after.json --json
```

`metrics` and `observability` emit the additive schema for atomic frame/damage/image counters, resources, and the bounded trace buffer. `observability` also includes integration points. The CLI process has no app render loop, so its counters are empty registry defaults (`initialized = false`, `source = cli`); values are not live app measurements.

`scene-diff` compares two frame JSON documents or lightweight scene summaries. It reports changed text rows/cells, image additions/removals/moves/resizes/source changes, damage-region additions/removals, and coverage in ppm. Text is the default output; `--json` returns `kind: "scene_diff"`.

## What to use

| Question | Command |
| --- | --- |
| Which version/build and paths am I running? | `env` |
| Reproduce one command's terminal frame? | `snapshot` |
| Replay recorded raw bytes once? | `replay` |
| See the frame sequence over time? | `trace` |
| Which layer first diverges? | `trace-layer` |
| Produce a structured, evidence-backed bug case? | `doctor` |
| Compare before/after frames? | `scene-diff` |
| Inspect counter/schema wiring? | `metrics` / `observability` |
| Give a local agent diagnostics or live control? | `xtervo diag serve` (see [MCP](mcp.md)) |

## Privacy and redaction

- `env` does not return environment-variable values, Vault contents, or API keys. It returns build metadata, selected session variables, paths, and hardware/session flags.
- `snapshot` executes the shell command you provide and can capture terminal text, including secrets printed by that command. `replay` and `trace` operate on raw bytes that may contain terminal content, escape sequences, paths, or credentials. Sanitize recordings before sharing them.
- Doctor evidence is bounded and redacted by default: at most 16 KiB per item, 64 KiB total, and 64 items. Over-limit content is marked `truncated` or listed in `omissions`; it is not silently dropped.
- Each evidence item carries a SHA-256 of the stored content, and the bundle carries a SHA-256 over the ordered manifest, total bytes, and omissions. These hashes let a recipient detect edits.
- Redaction covers secret-like keys (`api_key`, `token`, `secret`, `password`, `authorization`, `cookie`, `private_key`, and nested equivalents), Bearer/Basic credentials, JWTs, common token prefixes, URL userinfo and query credentials, and PEM/PGP private-key blocks. Reports retain rule names and match counts, not matched values. Issue labels and dynamic fields are redacted again.
- Configuration scanning stores only metadata, counts, and hashes; it does not copy configuration values into the case.
- Doctor does not read scrollback. It accepts explicit frame JSON and crops viewport text to 40 rows × 200 columns; the current CLI and MCP entry points do not expose file attachments.
- JSON inputs can be inline objects or files under `$HOME` or `/tmp`. Paths are canonicalized; `..` components, non-regular files, and symlink escapes outside those roots are rejected. Each file is limited to 1 MiB, and an MCP request line is limited to 1 MiB.
- `metrics_read` (MCP) considers only regular `xtervo-*.log` files under `$HOME` or `/tmp`, at most eight files, and scans only the last 256 KiB of each. It returns names, sizes, counts, and error-like line counts, never raw lines.
- Doctor, issue drafting, and metrics counting make no network requests. MCP stdio is local only.

## Limits

- `env` describes the diagnostic process, not a running app. In particular, `backend` is the requested value, and `log` is usually `null` because the CLI does not open a log file.
- The CLI does not create a window. `trace-layer` compares cell-level frames and the renderer's cell model, not the complete pixel framebuffer or the Slint compositor.
- The Term layer compares chunked vs. one-shot feeding of the same bytes; it is not a second terminal emulator.
- The image layer audits decoded placement/crop geometry only. Filter protocol ordering within a chunk has a known edge case; it is outside this diagnostic slice.
- The protocol ledger is an independent whole-input scanner. It answers which bytes belong to an image protocol; it does not run the production filter.
- `trace` records frame changes, not PTY or damage events. There is no screenshot, RGBA image dump, or PTY-event trace yet.
- `metrics`/`observability` are schema probes in the CLI; the app render loop does not feed a live registry yet.
- Doctor does not submit issues, push code, or generate regression test files.
- Doctor's generated prose is currently Chinese; JSON keys, schema names, and field identifiers remain stable English identifiers.
- MCP has no per-harness authorization tokens or rate limits; treat the local bridge as trusted code.

---

Chinese reference: [docs/DIAGNOSTICS.md](https://github.com/HeyTycho/xtervo/blob/main/docs/DIAGNOSTICS.md).
