# Vault：加密凭据存储与 SSH 密码 askpass（MVP）

> 实现：`crates/app/src/vault.rs`、`crates/app/src/askpass.rs`；设置页在 `main.slint` 的 **Vault**
> 分类。本文同时是安全限制说明与手工验证步骤。GUI 真机验证本轮受共享显示器约束未执行，
> 已用 `cargo test` / `cargo build --release` 覆盖纯逻辑与编译路径。

## 1. 能力

- 加密文件 `~/.config/tervo/vault.enc`（`XDG_CONFIG_HOME` 生效；`TERVO_VAULT=/path` 可覆盖）。
- 主密码 → **Argon2id**（19 MiB / t=2 / p=1，16B 随机 salt，32B key）。
- **ChaCha20-Poly1305** AEAD 加密；nonce 12B 随机；文件头（magic/version/KDF 参数/salt/nonce）
  作为 AAD 一并认证，篡改头会解密失败。
- 内存 map：`profile_id (u64) → password`；提供 `initialize/unlock/lock/change_master/set/get/remove/clear`。
- 锁定/未初始化时 `get` 返回错误；错密码、截断、乱码、版本不符都返回错误，不 panic。
- 文件 0600、临时文件 + `rename` 原子写；文件里不含主密码/明文密码。
- 设置 → Vault 页：状态、路径、Master 密码输入（masked）、Set/Change、Unlock、Lock、
  Clear（两次点击确认）、错误/提示。
- Profile 表单：`auth=password` 时出现 masked 密码输入 +「Save password to Vault」勾选；
  profiles.toml **只**保存 `save_password` 布尔值，永不保存密码。

## 2. 文件格式（版本 1）

```text
TERVO-VAULT 1
kdf=argon2id
m=19456
t=2
p=1
salt=<base64, 16 bytes>
nonce=<base64, 12 bytes>
ciphertext=<base64(ChaCha20-Poly1305 密文 + 16B tag)>
```

明文是 JSON 对象 `{ "<profile_id>": "<password>", ... }`。`ciphertext=` 之前的整个头部
（含换行）是 AEAD 的 AAD。解析器对 magic/version/KDF 名称、参数范围、salt/nonce 长度、
base64、大小上限（4 MiB）做校验，未知字段/缺少字段一律拒绝。

## 3. SSH 密码认证链路

1. Profile `auth=password` 且勾选保存：保存时 `vault.set(profile.id, password)`。
2. 打开该 Profile：若 Vault 已锁定 → 设置页跳到 Vault 并提示先解锁；解密后取出密码。
3. **先确认 host key**：用 `ssh -G` 取实际生效的 `stricthostkeychecking` 与
   known_hosts 路径，再用 `ssh-keygen -F` 查主机条目。
   - 未确认（默认 `ask` 且查不到）：本次**不用 askpass**，交给 ssh 在终端显示指纹并让用户
     手输一次密码；确认后 host key 进入 known_hosts，下次连接才自动填充。
   - `StrictHostKeyChecking=no/accept-new`：不会弹确认，可直接用 askpass。
4. 启动系统 `ssh` 前创建临时目录（`$TMPDIR/tervo-askpass-<pid>-<n>/`，0700）与脚本
   （0700），脚本内容：

   ```sh
   #!/bin/sh
   # 只回答真正的密码 prompt；host key 确认等一律 no（防 OpenSSH force 模式死循环）
   case "$1" in
     *[Pp]assword*|*密码*) printf '%s\n' "$TERVO_ASKPASS_SECRET" ;;
     *) printf 'no\n' ;;
   esac
   ```

   给 ssh 子进程设置：
   - `SSH_ASKPASS=<临时脚本路径>`
   - `SSH_ASKPASS_REQUIRE=force`
   - `TERVO_ASKPASS_SECRET=<密码>`

5. ssh 需要密码时执行脚本，从环境变量取密码；密码不会进入命令行/脚本/文件。
   非密码 prompt（host key 确认等）回答 `no`：OpenSSH 实测会把任何 askpass 输出当成
   host key 的 yes/no 答案；若把密码返回去，`SSH_ASKPASS_REQUIRE=force` 下会在 3 秒内
   调用脚本数千次并卡死。`no` 让 ssh 快速失败（可读的 `Host key verification failed.`）。
6. Pane 关闭时 `Askpass` guard 删除临时目录并尽力清零环境变量副本。
7. 未勾选保存：不创建 askpass，由 ssh 在终端里正常交互式询问（不经过 Vault）。

要求系统 OpenSSH ≥ 8.4（`SSH_ASKPASS_REQUIRE` 支持）。旧版 ssh 会忽略 force；若已有
`DISPLAY` 仍可能用 askpass，否则回退到终端询问。

## 4. 安全限制（必须读）

1. **askpass 密码经子进程环境变量传递**。同一用户（以及 root）可通过
   `/proc/<ssh-pid>/environ` 看到明文密码，也可能从进程内存/core dump 获得。这比写明文
   文件好（不落盘），但**不是最终方案**。
   - 后续：Tervo 主进程把密码放进继承 fd/pipe（memfd 或 pipe），askpass 从 fd 读取；
     环境只留 fd 号。注意当前 PTY 启动链（portable-pty）会在 `pre_exec` 里关闭所有
     `fd > 2`，需要给 `spawn` 增加 fd 白名单或改由 Tervo 主进程用 Unix socket 按次下发。
2. **解锁后主密码/凭据在进程内存**。本 MVP 未引入 `mlock`/完整 `zeroize`；正常
   `lock()`/Drop 会覆盖 key 与密码字符串，但分配器/崩溃转储仍可能残留。不要在不信任的
   机器上长时间保持解锁。
3. **没有 Secret Service/Keyring 后端**，也没有主密码找回。忘记主密码 = 无法解密。
4. **没有空闲自动锁定/解锁弹窗**；锁定只能手动点 Lock 或退出进程。
5. 取消「Save password to Vault」时：Vault 已解锁会同步删除旧条目；**锁定时无法删除**，
   旧密文会留在 vault.enc 中直到解锁后重新保存/取消，或使用 Clear。已在日志中提示（不打印密码）。
6. 错密码与密文损坏在 AEAD 层不可区分，统一报「主密码错误或文件损坏」。
7. Clear vault 清空全部凭据但保留文件与主密码；不会删除文件本身。
8. 备份/同步工具复制的 vault.enc 同样受主密码保护；但旧版本文件不会自动轮换主密码。
9. 文件 `vault.enc` 0600；配置目录/`profiles.toml` 的权限没有统一收紧（Linux 默认受
   `umask` 影响）。
10. 主机密钥校验仍完全交给系统 ssh（known_hosts），Tervo 没有额外 UI/告警。首次连接
    不会自动信任新 key：本次回退到终端确认；askpass 对 host key prompt 回答 `no`。
    known_hosts 判定是 best-effort：`KnownHostsCommand` 等动态配置、或 known_hosts 只有
    host 条目但服务器换了 key 类型时可能误判；最坏结果是快速失败，不会自动接受或死循环。
11. askpass 脚本只识别含 `password` / `密码` 的 prompt。其它语言的密码提示（或
    keyboard-interactive 自定义问题）可能被当成非密码 prompt 回答 `no` 而导致认证失败；
    此时可取消勾选保存、按终端提示交互式输入，或补脚本匹配规则。

## 5. 测试

```bash
export CARGO_HOME=/home/yt/AIworkspace/rusttabbyterminal/.cargo
export CARGO_TARGET_DIR=/home/yt/AIworkspace/rusttabbyterminal/target
cargo test -p tervo-app --bin tervo
```

覆盖：
- `vault::tests::set_get_remove_roundtrip_survives_lock_and_reload`
- `vault::tests::wrong_password_and_corrupt_file_fail_cleanly`
- `vault::tests::file_never_contains_plaintext_and_permissions_are_private`
- `vault::tests::change_master_reencrypts_and_old_password_stops_working`
- `vault::tests::initialize_refuses_to_overwrite_and_clear_keeps_file`
- `vault::tests::corrupt_header_params_are_rejected_before_argon2`
- `profile::tests::profile_serialization_has_no_plaintext_password`
- `askpass::tests::*`（密码 prompt 输出、非密码 prompt 回答 no、force 环境、Debug 脱敏、
  0700 权限、真实 `ssh -G`/`ssh-keygen -F` known_hosts 判定）

所有 Vault 测试使用 `std::env::temp_dir()` 下独立目录，不读写真实 `$HOME`。

## 6. 手工验证（GUI）

前置：系统有 `ssh` 服务端/账号；`ssh -V` ≥ 8.4。

1. 启动：`./target/release/tervo`（共享显示器环境请让有权限的 agent 操作）。
2. 打开齿轮 → 左侧 **Vault**：
   - 状态应显示「未初始化」；输入 ≥8 字符主密码 → **Set/Change** → 状态「已解锁 · 0 条凭据」。
   - 点 **Lock** → 状态「已锁定」；再输入主密码 → **Unlock** 恢复。
3. 打开 Profiles → New/Edit：
   - Kind=SSH，填 Host/Port/User，Auth 填 `password`；
   - 出现 **Password**（masked）与 **Save password to Vault**；输入密码并勾选 → Save。
   - 若 Vault 未解锁，表单应显示「Vault 未解锁：请先在 Settings → Vault 解锁…」；解锁后重试。
4. 连接：从 “+” 菜单选该 Profile。
   - **首次连接**（host key 不在 known_hosts）：终端应正常显示指纹并允许输入 `yes`，随后
     要求手输密码（本次不自动填充）；只想保存 host key 的话，确认指纹后可直接 Ctrl+C，
     重新打开该 Profile 就会走 Vault + askpass。
   - **之后连接**：Vault 已解锁时 ssh 不应再询问密码，直接登录；锁定时应跳到
     Settings → Vault 并提示先解锁。
   - 若终端出现 `Host key verification failed.` 而不是指纹确认，说明 known_hosts 判定
     与 askpass 兜底生效（未自动接受新 key）；先在终端手动 `ssh` 一次确认 host key。
5. 安全核对：
   - `grep -a '你的密码' ~/.config/tervo/vault.enc` 应无结果；
   - `stat -c '%a' ~/.config/tervo/vault.enc` 应为 `600`；
   - `grep -a '你的密码' ~/.config/tervo/profiles.toml` 应无结果（只有 `save_password = true`）；
   - 进程日志（`~/.local/state/tervo/logs/`）不得出现密码。
6. 错误路径：错主密码 → 错误提示且保持锁定；截断 vault.enc → 报损坏且原文件不被覆盖；
   Clear 第一次点提示确认，第二次清空且主密码/文件保留。

## 7. 后续工作

- [ ] askpass 改继承 fd/pipe（去掉 `/proc/<pid>/environ` 暴露）。
- [ ] Secret Service/Keyring 后端 + 后端选择说明。
- [ ] `zeroize`/`mlock`、空闲自动锁定、锁屏联动。
- [ ] 解锁弹窗（从连接动作直接弹出，而不是跳设置页）。
- [ ] 凭据逐条查看/删除 UI；密钥口令（key passphrase）也走 Vault。
- [ ] 文件格式升级策略（KDF 参数变更/主密码轮换已经是新 salt+key 重加密）。
