# SSH / SFTP / Vault 产品化（W-02）

> 实现：`profile.rs`、`known_hosts.rs`、`sftp.rs`、`vault.rs`、`main.rs`、`main.slint`。
> 本页记录用户可见行为、测试证据与仍存在的限制；安全细节另见 [`VAULT.md`](VAULT.md)。

## 1. SSH Profile

`profiles.toml` v2 新增 **additive** 字段（旧文件缺字段自动补空/false，无需迁移）：

| 字段 | 表单 | 传给 ssh |
|---|---|---|
| `proxy_jump` | Jump | `-J [user@]host[:port]`（支持 OpenSSH 逗号多级跳板） |
| `remote_forwards` | R-Forwards | 每条 `-R [bind:]port:host:port` |
| `dynamic_forwards` | D-Forwards | 每条 `-D [bind:]port`（SOCKS） |
| `agent_forward` | Agent forwarding (-A) | `-A` |
| `local_forwards` | L-Forwards | 每条 `-L port:host:port` |
| `extra_args` | Extra | 原样追加，可覆盖 ssh_config |

`auth=password` 生成
`-o PubkeyAuthentication=no -o PreferredAuthentications=password,keyboard-interactive
-o KbdInteractiveAuthentication=yes`；askpass 支持常见本地化密码 prompt，并显式拒绝
OTP/验证码 prompt（回答 `no`，不会把账户密码填给一次性验证码）。

## 2. known_hosts 首次/变更提示

连接 SSH Profile 前，`HostKeyJob` 在后台（10s 硬超时、可取消）执行：

1. `ssh -G <profile args>` 取生效的 strict/hostname/port/known_hosts/ProxyJump/HostKeyAlias；
2. `ssh-keygen -F <key> -f <file>` 查本地条目；
3. `ssh-keyscan -T 5 -p <port> <host>` 取服务器当前公钥（1 MiB 上限、进程组 kill）。

结论：

- **Known**：直接连接（process 内缓存，避免重复 keyscan）。
- **Unknown**：显示 `SHA256:` 指纹。`Trust & connect` 写第一个 user known_hosts
  （目录 0700/文件 0600）后连接；`Once in terminal` 本次禁用 askpass，由 ssh 在终端确认。
- **Changed**：并列旧/新指纹 + WARNING。只有 `Update key & connect` 会先
  `ssh-keygen -R` 删旧条目再写新 key；Cancel 不改文件。
- **Unverifiable**：ProxyJump / keyscan 失败 / 无共同 key 类型 → 回退终端人工确认。
- `StrictHostKeyChecking=no/off/accept-new` 由用户配置显式接管，不弹应用内提示。

## 3. SSH 退出状态与重连

- `PaneHandle::try_exit_code()` 非阻塞取退出码/信号；SSH pane 退出后右上角横幅显示
  「SSH session ended · 退出码/信号」。
- `Reconnect` 在原 pane 格子按当前 Profile 重新拉起 ssh（不新建 Tab、不自动循环重连）。
- 重连失败（Profile 删除/Vault 锁定/创建失败）在横幅中显示错误或跳转 Vault 解锁。

## 4. SFTP

- 上传/下载队列：`TransferQueue` FIFO，当前传输结束后自动取下一项；UI 显示前 2 行 +
  失败重试行与队列计数；`Clear queue` 只清未开始项。
- 进度：沿用 OpenSSH progress 行解析；stdout 非 tty 时通常为“不确定进度”，不伪造百分比。
- Cancel：传输与元数据操作都支持（标记 + kill 进程组）；取消传输会留下可能的部分文件。
- Retry：失败请求保留，Retry 插到队首/立即重试；错误信息来自 sftp stderr。
- chmod：Mode 输入八进制（0–7777）→ `chmod 0644 <remote>`，失败回显错误。
- 超时：所有 sftp 命令带 `-o ConnectTimeout=10 -o ServerAliveInterval=5
  -o ServerAliveCountMax=2`（用户 extra_args 在前，可覆盖）；元数据命令另有 10s 硬超时。
  传输本身不设总时长上限（避免误杀大文件），靠 Cancel + keepalive 检测死链。
- ProxyJump：SFTP 复用 Profile 的 `-J`；**不注入 Vault 密码/askpass**，password-only
  Profile 需系统 ssh 交互或改用 agent/key。

## 5. Vault（P1-08）

- `VaultTask` 后台线程执行 initialize/unlock/change_master（Argon2id + AEAD）；
  `WakeSignal` 唤醒 UI，UI 线程只 `adopt()`。Vault 页 busy 时显示
  「正在后台派生密钥」并保持重绘；busy 时操作被拒绝、job 异常会恢复状态。
- worker 主密码、AEAD 明文字节、`Unlocked` key/凭据尽力清零；日志/错误/文件不含密码。
- `XTERVO_VAULT_KDF_DELAY_MS` 仅供 GUI e2e 截取 busy 状态（默认 0）。

## 6. 测试与验收

```bash
CARGO_HOME=$PWD/.cargo CARGO_TARGET_DIR=/home/user/AIworkspace/rusttabbyterminal/target \
  cargo test --offline --workspace
./scripts/e2e_ssh_sftp_vault.sh   # Xvfb :139
```

- app：351 passed；core：108 单元 + smoke 25（含新 `child_exit_status_is_available_after_exit`）。
- e2e：19 张截图（`.logs/ssh-sftp/e2e/shots`）覆盖 SSH 表单、known_hosts Unknown/Changed、
  退出横幅/Reconnect、Vault busy/完成、SFTP 列表/队列/Cancel/元数据 Cancel/chmod/失败/Retry。
- 本机无 sshd，未跑真实 sshd 登录；e2e 用假 ssh/sftp/keyscan 驱动 UI，known_hosts 逻辑
  复用真实 `ssh -G`/`ssh-keygen -F`/`ssh-keygen -lf`。真实服务器仍可用既有
  `scripts/ssh_test_server.py` + `xtervo --sftp-probe`。

## 7. 未完成 / 限制

- 无应用内 known_hosts 条目管理（查看/删除）；ProxyJump 主机不能预检。
- 无自动重连/重试循环；重连需用户点击。
- SFTP 无覆盖确认、断点续传、递归删除、特殊字符 batch 转义；无权限/大小列。
- Vault 无 keyring/Secret Service、无空闲自动锁定；askpass 仍经子进程环境变量传密码。
- SSH 表单在 <700px 高窗口可能超出底部；无通用滚动表单。
