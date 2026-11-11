# 过滤链审计 fixtures（2026-11-11）

这些不是合成的转义序列，而是**真实 dsh-tui 会话**的 PTY 输出切片，用于
`crates/core/tests/filter_conservation.rs` 的防回归测试。

## 来源

原始 capture（未提交，`.logs/` 被 gitignore）：

```text
.worktrees/agent-e2e/.logs/dsh-image-preview-e2e/pty/app-to-host.raw
```

来自 `docs/DSH_IMAGE_PREVIEW_E2E.md` 记录的隔离 e2e（Xvfb + 真实 dsh-tui，
2026-11-10 18:35，302517 bytes）。

| 文件 | 截取偏移 | 大小 | 内容 |
| --- | --- | --- | --- |
| `dsh-2026-11-10-text-cjk.raw` | `5693..8693` | 3000 B | 中文/框线/`ESC]8` OSC 8 链接等纯文本输出；含 `┐`(E2 94 90)、`面`(E9 9D A2)、`是`(E6 98 AF)、`域`(E5 9F 9F) 等“UTF-8 续字节落在 C1 区间”的字符 |
| `dsh-2026-11-10-kitty-chunked.raw` | `50000..65200` | 15200 B | 一次 `o=z`、`m=1`×3 + `m=0` 的 Kitty graphics 分片传输 + `a=p` placement，前后带 TUI 文本 |

## 唯一改动：长度保持的隐私替换

只把路径里的用户名替换成占位符，**长度不变、其余字节逐字节保留**：

```text
/home/user/  ->  /home/xx/
```

因此 escape 序列、UTF-8 字节、chunk 布局都与真实 capture 一致。

## 重新生成

原始 capture 存在时：

```bash
cd /home/user/AIworkspace/rusttabbyterminal
python3 - <<'EOF'
src='.worktrees/agent-e2e/.logs/dsh-image-preview-e2e/pty/app-to-host.raw'
raw=open(src,'rb').read()
for a,b,name in [(5693,8693,'dsh-2026-11-10-text-cjk.raw'),
                 (50000,65200,'dsh-2026-11-10-kitty-chunked.raw')]:
    d=raw[a:b].replace(b'/home/user/', b'/home/xx/')
    assert len(d)==b-a and d.count(b'/home/user/')==0
    open('crates/core/tests/fixtures/'+name,'wb').write(d)
EOF
```

## 断言

- 文本切片：任意 chunk 切分（含逐字节 drip）输出逐字节等于输入；
- Kitty 切片：`pass == 输入去掉 APC 区间`，且每个 `ESC \`/base64 中间切分
  结果不变，Kitty 命令数 ≥ 2（分片传输 + placement）。
