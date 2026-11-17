# 官方 SSH 插件 POC（ACT-052 F5）

状态：**POC 可用**（2026-11-17）。目标是在已有 transport seam 上跑通
“SSH pane 的字节后端是 `Backend::Plugin`”，而不是继续在本地 PTY 里跑系统
`ssh`。本 POC 不新增依赖（`Cargo.lock` 无 `russh`，因此用系统 ssh 客户端）。

## 1. 结构

```text
Profile(kind="ssh", transport="plugin")
  → create_pane 不 openpty，改为 spawn_with_transport(..., Backend::Plugin(..))
  → SshPluginTransport:
       spawn ssh -tt -o BatchMode=yes -o ConnectTimeout=… -o ServerAlive…
       stdout ──读线程──► [u32 LE len][payload] 帧（transport_io 协议）
                        ──► FrameReader ──► pane 的 vte/actor（与 PTY 同一条路径）
       stdin  ◄──────────── PaneTransport::write_all（终端按键）
       stderr ──独立线程──► 失败/超时文本（红色）送进 pane 后 EOF
```

- 数据面：`crates/app/src/transport_io.rs` 的帧协议（≤256 KiB、长度精确匹配；
  超限/半帧都是错误，不静默截断）。
- 状态机：`connecting → connected → closed / failed / timeout`，可用
  `SshConnectionStatus` 观察；连接状态只记 host/user/原因，不记 payload/密码。
- 超时：ssh 侧 `ConnectTimeout` + 20s watchdog（`TERVO_PLUGIN_SSH_CONNECT_TIMEOUT_MS`
  可覆盖）。超时会杀掉 ssh 并把黄色错误文本显示在 pane 里。
- 断开：关闭 pane → transport `close` → kill ssh；重复 close 幂等。

## 2. Profile 写法

```toml
# profiles.toml
[[profiles]]
id = 1
name = "remote-plugin"
kind = "ssh"            # 也接受旧的 kind = "plugin-ssh"（加载时归一化）
transport = "plugin"
host = "ssh.example.test"
port = 22
user = "demo"
auth = "agent"          # POC 支持 agent/key；password 会被明确拒绝
```

`transport` 缺省为空 = 原有 PTY 路径，行为不变。

## 3. 入口与复现命令

```bash
export CARGO_HOME=$PWD/.cargo CARGO_NET_OFFLINE=true

# 1) 无 GUI：走与 pane 完全相同的 transport，打印远端字节
TERVO_PROFILES=$PWD/.logs/ssh-poc/profiles.toml \
  ./target/debug/tervo --plugin-ssh-probe remote-plugin --duration 8

# 2) GUI：启动即开一个 plugin SSH pane（Xvfb 下可截图；TERVO_PROFILES 同上）
DISPLAY=:150 TERVO_PLUGIN_SSH_CONNECT=remote-plugin \
  ./target/debug/tervo --cols 100 --rows 28

# 3) 命令：
#    ssh.connect:<id|name>   （调色板里的 “SSH (plugin): <name>”）
#    ssh.connect             （第一个 SSH Profile）
```

## 4. 实测证据（2026-11-17）

- 本机 `ssh -o BatchMode=yes ssh.example.test` 可通；插件路径 probe 输出
  `Welcome to Ubuntu LTS …` + 远端提示符，退出码 0。见
  `docs/evidence/ssh-plugin-poc/probe-banner.log`。
- GUI pane（Xvfb :150，`TERVO_PLUGIN_SSH_CONNECT=remote-plugin`）显示
  `[plugin-ssh] 正在连接 demo@ssh.example.test:22 …`、Ubuntu banner 和提示符：
  `docs/evidence/ssh-plugin-poc/pane-banner.png`。
- 连不上（`127.0.0.1:1`，Connection refused）时 pane 显示红色
  `[plugin-ssh] 连接失败 …（ssh 退出码 255）` + ssh stderr：
  `docs/evidence/ssh-plugin-poc/pane-error.png` /
  `probe-refused.log`（probe 退出码 1）。
- 单测：`cargo test -p tervo-app --bin tervo ssh_plugin` 10 例（args/password
  拒绝/banner/失败可见/超时/close 幂等/resize/帧重组/超限帧/kind 归一化）。

## 5. POC 边界（下一步）

- 只支持 agent/私钥；密码/Vault/askpass 仍走旧 PTY 路径。
- 远端 PTY 用 `ssh -tt` 强制分配；管道没有 winsize，**resize 只记录不转发**。
- 不是独立插件进程：transport 在 app 进程内包 ssh 子进程；真正的
  `transport.open`/SCM_RIGHTS 协议与卸载留给后续。
- 无重连 UI / Connection Manager；pane 关闭即断开。
- 未知 host key 时 `BatchMode=yes` 会让 ssh 直接失败（错误在 pane 可见），
  首次信任仍用现有 known_hosts 预检对话框。
