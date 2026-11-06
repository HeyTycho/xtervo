# Vault：加密凭据存储与 SSH 密码 askpass（MVP）

> 实现：`crates/app/src/vault.rs`、`crates/app/src/askpass.rs`；设置页在 `main.slint` 的 **Vault**
> 分类。本文同时是安全限制说明与手工验证步骤。GUI 真机验证已在本轮完成（独立
> `TERVO_CONFIG/TERVO_VAULT/TERVO_PROFILES/TERVO_LOG_DIR` + 窗口移到空白区）：
> 服务端记录 `password auth ok`，pane 出现 `SSH-TEST-OK` / `remote$ `。

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
- **P1-08：Argon2id 不跑 UI 线程**。`initialize/unlock/change_master` 的派生/加解密在
  `VaultTask` 后台线程执行，完成后经 `WakeSignal` 唤醒 UI；期间 Vault 页显示
  「正在后台派生密钥」并保持可重绘/可滚动。UI 线程只做 `adopt()`，不重新派生。
  串行状态机：同一时间只允许一个 Vault 任务，busy 时的按钮调用被拒绝并提示；任务线程
  异常结束时恢复 busy=false，不会永久卡住。
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
3. **先确认 host key（W-02）**：打开 SSH Profile 时启动 `known_hosts::HostKeyJob`
   后台预检（最多 10s，可取消，UI 不冻结）。用 `ssh -G` 取实际生效的
   `stricthostkeychecking`、`hostname`、`port`、`hostkeyalias`、known_hosts 路径与
   `proxyjump`，再用 `ssh-keygen -F` 查主机条目、用 `ssh-keyscan -T 5` 取服务器当前公钥：
   查找键必须与 OpenSSH 自己的 known_hosts 匹配一致：
   - 默认用 `ssh -G` 的 **`hostname`**（`Host alias` 经 `HostName` 解析后的真实主机），
     非 22 端口写成 `[host]:port`；直接用 Profile 里的别名会误判“未确认”。
   - 配置了 `HostKeyAlias` 时，OpenSSH 使用**别名原文**查表（不附加 `[alias]:port`，
     已用真实 ssh 验证）。
   - **Known**：直接连接，password Profile 可用 Vault + askpass。
   - **Unknown（首次）**：弹窗显示 `SHA256:` 指纹，用户可选「Trust & connect」（写
     user known_hosts 后连接）或「Once in terminal」（本次不用 askpass，由 ssh 在终端
     显示指纹/密码提示）。没有“静默信任”路径。
   - **Changed**：known_hosts 已有条目但同类型公钥不一致时，弹窗并列显示旧/新指纹与
     WARNING；只有用户显式点「Update key & connect」才删除旧条目并写入新 key，Cancel 不改文件。
   - **Unverifiable**（keyscan 失败/ProxyJump/无共同 key 类型）：不自动信任，弹窗提示
     回退到终端人工确认；`Continue (terminal)` 本次禁用 askpass。
   - `StrictHostKeyChecking=no/off/accept-new`：用户配置显式接管，不弹应用内确认
     （与系统 ssh 行为一致）。
   - 进程内对已确认的连接配置（profile id/host/port/jump/extra）做缓存，避免每次连接都
     keyscan；Profile 配置或 host 变化后缓存键不同，会重新预检。
4. 启动系统 `ssh` 前，对 `auth=password` 的 Profile **强制只走密码认证**：
   `-o PubkeyAuthentication=no -o PreferredAuthentications=password,keyboard-interactive
   -o KbdInteractiveAuthentication=yes`
   （`keyboard-interactive` 仅作 PAM 服务器兜底，顺序在 `password` 之后）。
   否则 ssh 会先尝试 agent/密钥：只要服务器接受某个公钥，就根本不会出现密码 prompt，
   Vault 密码和 askpass 都不会被用到（GUI 实测表现为服务端 `password auth ok` 为 0、
   askpass 调试文件为空）。`auth=agent/key` 不注入这些参数。
5. 创建临时目录（`$TMPDIR/tervo-askpass-<pid>-<n>/`，0700）与脚本（0700），脚本内容：

   ```sh
   #!/bin/sh
   prompt="$1"
   lower=$(printf '%s' "$prompt" | tr '[:upper:]' '[:lower:]')
   # 只回答真正的密码 prompt；host key 确认/OTP 等一律 no（防 force 模式死循环/误回填）
   case "$lower" in
     *verification*|*one-time*|*onetime*|*otp*|*passcode*|*auth\ code*|*验证码*|*ワンタイム*)
       printf 'no\n' ;;
     *password*|*passwd*|*passwort*|*mot\ de\ passe*|*contraseña*|*senha*|*wachtwoord*|*пароль*|*Пароль*|*ПАРОЛЬ*|*パスワード*|*비밀번호*|*密码*|*密碼*|*口令*)
       if [ -n "$TERVO_ASKPASS_DEBUG" ]; then
         printf '%s\n' "match=password prompt=$prompt" >>"$TERVO_ASKPASS_DEBUG" 2>/dev/null
       fi
       printf '%s\n' "$TERVO_ASKPASS_SECRET" ;;
     *)
       if [ -n "$TERVO_ASKPASS_DEBUG" ]; then
         printf '%s\n' "match=no prompt=$prompt" >>"$TERVO_ASKPASS_DEBUG" 2>/dev/null
       fi
       printf 'no\n' ;;
   esac
   ```

   给 ssh 子进程设置：
   - `SSH_ASKPASS=<临时脚本路径>`
   - `SSH_ASKPASS_REQUIRE=force`
   - `TERVO_ASKPASS_SECRET=<密码>`

   可选诊断：Tervo 进程环境里设置 `TERVO_ASKPASS_DEBUG=<文件>` 后，脚本每次被调用都会
   追加一行 `match=password|no prompt=...`（**不含密码**），用于确认 askpass 是否真的
   被 ssh 调用、prompt 是否被识别。生产环境不要长期开启。

6. ssh 需要密码时执行脚本，从环境变量取密码；密码不会进入命令行/脚本/文件。
   非密码 prompt（host key 确认等）回答 `no`：OpenSSH 实测会把任何 askpass 输出当成
   host key 的 yes/no 答案；若把密码返回去，`SSH_ASKPASS_REQUIRE=force` 下会在 3 秒内
   调用脚本数千次并卡死。`no` 让 ssh 快速失败（可读的 `Host key verification failed.`）。
7. 连接时的诊断日志（只记布尔/长度/退出语义，不记密码）：
   - `[vault] 已取出 Profile <id> 的密码（len=<n>）`
   - `[askpass] 已启用 profile=<id> require=force debug=<bool>`
   - `[askpass] 注入 ssh 子进程环境: keys=["SSH_ASKPASS","SSH_ASKPASS_REQUIRE", ...] secret_set=true`
   - known_hosts 未命中时的 lookup/strict/候选路径。
   结合 `TERVO_ASKPASS_DEBUG` 可以区分「askpass 没启用」「脚本没被调用」「prompt 未识别」
   「SSH 环境没传到」四类问题。
8. Pane 关闭时 `Askpass` guard 删除临时目录并尽力清零环境变量副本。
9. 未勾选保存：不创建 askpass，由 ssh 在终端里正常交互式询问（不经过 Vault）。
   注意 `auth=password` 的 `PubkeyAuthentication=no` 对它同样生效：不会偷偷回退到密钥。

要求系统 OpenSSH ≥ 8.4（`SSH_ASKPASS_REQUIRE` 支持）。旧版 ssh 会忽略 force；若已有
`DISPLAY` 仍可能用 askpass，否则回退到终端询问。

## 4. 安全限制（必须读）

1. **askpass 密码经子进程环境变量传递**。同一用户（以及 root）可通过
   `/proc/<ssh-pid>/environ` 看到明文密码，也可能从进程内存/core dump 获得。这比写明文
   文件好（不落盘），但**不是最终方案**。
   - 后续：Tervo 主进程把密码放进继承 fd/pipe（memfd 或 pipe），askpass 从 fd 读取；
     环境只留 fd 号。注意当前 PTY 启动链（portable-pty）会在 `pre_exec` 里关闭所有
     `fd > 2`，需要给 `spawn` 增加 fd 白名单或改由 Tervo 主进程用 Unix socket 按次下发。
2. **解锁后主密码/凭据在进程内存**。本 MVP 未引入 `mlock`/完整 `zeroize`；后台 KDF
   线程会覆盖自己持有的主密码副本，`Unlocked` 的 Drop 会覆盖 key 与密码字符串，AEAD
   明文字节也在解析后清零；但分配器/崩溃转储仍可能残留。不要在不信任的机器上长时间
   保持解锁。
3. **没有 Secret Service/Keyring 后端**，也没有主密码找回。忘记主密码 = 无法解密。
4. **没有空闲自动锁定/解锁弹窗**；锁定只能手动点 Lock 或退出进程。
5. 取消「Save password to Vault」时：Vault 已解锁会同步删除旧条目；**锁定时无法删除**，
   旧密文会留在 vault.enc 中直到解锁后重新保存/取消，或使用 Clear。已在日志中提示（不打印密码）。
6. 错密码与密文损坏在 AEAD 层不可区分，统一报「主密码错误或文件损坏」。
7. Clear vault 清空全部凭据但保留文件与主密码；不会删除文件本身。
8. 备份/同步工具复制的 vault.enc 同样受主密码保护；但旧版本文件不会自动轮换主密码。
9. 文件 `vault.enc` 0600；配置目录/`profiles.toml` 的权限没有统一收紧（Linux 默认受
   `umask` 影响）。
10. **known_hosts 预检是 best-effort，不是第二套 SSH 实现**：最终认证/主机密钥握手仍由
    系统 ssh 完成。Tervo 在连接前额外做一次 `ssh-keyscan` 预检，首次连接/密钥变更时弹窗，
    用户必须显式选择「Trust & connect / Update key & connect / Once in terminal」，没有
    静默信任。限制：
    - 配置了 `ProxyJump` 时 `ssh-keyscan` 无法穿透跳板机 → 判为 Unverifiable，回退到
      终端确认（`Continue (terminal)` 本次禁用 askpass）。
    - keyscan 失败/超时（10s）同样回退终端确认；不会自动信任。
    - `KnownHostsCommand`、`Match exec` 等动态配置可能让本地查找与 ssh 实际行为不一致。
    - `StrictHostKeyChecking=no/off/accept-new` 由用户显式配置接管，不弹应用内提示。
    - known_hosts 只有 host 条目但服务器换了 key 类型时，没有共同类型 → Unverifiable，
      交给 ssh 终端确认；不会静默放行。
    - 写 known_hosts 只写 `UserKnownHostsFile` 的第一个文件（不写系统 `/etc/ssh`），
      变更场景先用 `ssh-keygen -R` 删除该主机旧条目，文件权限 0600。
11. askpass 识别常见本地化密码 prompt（password/passwd/passwort/mot de passe/contraseña/
    senha/wachtwoord/пароль/パスワード/비밀번호/密码/密碼/口令）并显式排除 OTP/验证码/
    one-time code 等 keyboard-interactive 问题；未识别的自定义问题回答 `no`，认证会快速
    失败而不会把账户密码回填给验证码。此时可取消勾选保存、按终端提示交互式输入。
12. `TERVO_ASKPASS_DEBUG` 诊断文件会记录服务器发来的 prompt 原文（通常含用户名/主机名，
    **不含密码**）。只在排查时临时开启；文件路径由用户环境变量指定，Tervo 不自动创建。
13. `auth=password` 会禁用公钥认证。若服务器实际只接受公钥、或用户想回退到 agent，
    应把 Profile 的 Auth 改成 `agent`/`key`，而不是依赖密码 Profile 的隐式回退。

## 5. 测试

```bash
export CARGO_HOME=/home/yt/AIworkspace/rusttabbyterminal/.cargo
export CARGO_TARGET_DIR=$PWD/target
cargo test -p tervo-app --bin tervo
```

覆盖（W-02 后 app 351 例；Vault/askpass/known_hosts 相关节选）：
- `vault::tests::set_get_remove_roundtrip_survives_lock_and_reload`
- `vault::tests::credential_is_keyed_by_profile_store_id`
  （`ProfileStore::add` 分配的 id → `vault.set(id)` → 用 store 中 `Profile.id` 取回，
  两个 Profile 的密码不串、未保存 id 返回 NotFound、重启后仍能取回）
- `vault::tests::background_unlock_moves_kdf_off_caller_and_adopts_session`
  （后台 `VaultTask::unlock` + `WakeSignal`，成功才 `adopt`，UI 线程不等 Argon2）
- `vault::tests::background_unlock_wrong_password_keeps_ui_locked`
- `vault::tests::background_change_master_reencrypts_without_leaking_old_key`
- `vault::tests::background_initialize_state_machine_and_errors`
- `vault::tests::background_task_drop_does_not_block_or_leak_into_ui`
- `vault::tests::wrong_password_and_corrupt_file_fail_cleanly`
- `vault::tests::file_never_contains_plaintext_and_permissions_are_private`
- `vault::tests::change_master_reencrypts_and_old_password_stops_working`
- `vault::tests::initialize_refuses_to_overwrite_and_clear_keeps_file`
- `vault::tests::corrupt_header_params_are_rejected_before_argon2`
- `profile::tests::profile_serialization_has_no_plaintext_password`
- `profile::tests::password_auth_ssh_command_is_password_only`
  （`auth=password` 注入 `PubkeyAuthentication=no` +
  `PreferredAuthentications=password,keyboard-interactive` +
  `KbdInteractiveAuthentication=yes`；agent/key 不注入）
- `profile::tests::old_profiles_toml_without_new_ssh_fields_still_loads`
- `known_hosts::tests::*`（ssh -G 解析、HostKeyAlias、`[host]:port`、首次/变更分类、
  keyscan 超时/取消、trust 写入权限 0600、假 keyscan Changed/Unknown/Known）
- `askpass::tests::*`（本地化密码 prompt、OTP/验证码回答 no、force 环境、Debug 脱敏、
  0700 权限、真实 `ssh -G`/`ssh-keygen -F` known_hosts 判定）
- `askpass::tests::applied_envs_reach_real_child_process`
  （`apply()` 后真的 spawn 子进程，验证 `SSH_ASKPASS`/`SSH_ASKPASS_REQUIRE=force`/
  `TERVO_ASKPASS_SECRET` 到达子进程，脚本路径存在）
- `askpass::tests::debug_file_records_prompt_without_secret`
  （`TERVO_ASKPASS_DEBUG` 记录 prompt/匹配分支，但文件里没有密码）
- `askpass::tests::host_key_known_matches_nonstandard_port_entry`
  （Profile 级 `[127.0.0.1]:2222` 命中，端口/主机不同必须 miss）
- `askpass::tests::host_key_known_uses_resolved_hostname_from_ssh_config`
  （`Host alias` + `HostName 127.0.0.1`：用解析后的主机名查表）
- `askpass::tests::host_key_known_uses_host_key_alias_verbatim`
  （`HostKeyAlias=myalias`：按别名原文，不附加端口）
- GUI e2e：`scripts/e2e_ssh_sftp_vault.sh`（Xvfb :139；SSH 表单/known_hosts 首次+变更/
  Vault 后台 KDF busy/完成/SFTP 队列/取消/chmod/retry 截图，见 `.logs/ssh-sftp/e2e/shots`）

所有 Vault 测试使用 `std::env::temp_dir()` 下独立目录，不读写真实 `$HOME`。

无 GUI 的密码自动登录探针（不启动 Tervo）。**必须在 PTY 里跑**：GUI 里的 ssh 有 tty，
只有 `SSH_ASKPASS_REQUIRE=force` 才会走 askpass；`</dev/null` 的旧探针会因 stdin 不是
tty 而“碰巧”走 askpass，无法覆盖真实 GUI 路径。

```bash
d=$(mktemp -d)
ssh-keygen -t ed25519 -f "$d/host_ed25519" -N "" -q
python3 scripts/ssh_test_server.py --host-key "$d/host_ed25519" --port 2222 --password s3cret \
  >"$d/server.log" 2>&1 &
srv=$!
printf '[127.0.0.1]:2222 %s\n' "$(cut -d' ' -f1,2 "$d/host_ed25519.pub")" > "$d/known_hosts"

# 与 Askpass::new 相同的脚本（只回答密码 prompt，其它回答 no；可写调试文件）
cat > "$d/askpass.sh" <<'EOF'
#!/bin/sh
prompt="$1"
case "$prompt" in
  *[Pp]assword*|*密码*)
    [ -n "$TERVO_ASKPASS_DEBUG" ] && printf '%s\n' "match=password prompt=$prompt" >>"$TERVO_ASKPASS_DEBUG"
    printf '%s\n' "$TERVO_ASKPASS_SECRET" ;;
  *)
    [ -n "$TERVO_ASKPASS_DEBUG" ] && printf '%s\n' "match=no prompt=$prompt" >>"$TERVO_ASKPASS_DEBUG"
    printf 'no\n' ;;
esac
EOF
chmod 700 "$d/askpass.sh"

# 已知 host：在 PTY 里跑真实 ssh（与 GUI 相同：stdin 是 tty，靠 REQUIRE=force 走 askpass）。
# 期望 server.log 有 "password auth ok" + "pty request"，终端出现 SSH-TEST-OK；
# timeout 退出是因为测试服务不会主动关闭 shell，不是认证失败。
SSH_ASKPASS="$d/askpass.sh" SSH_ASKPASS_REQUIRE=force TERVO_ASKPASS_SECRET=s3cret \
  TERVO_ASKPASS_DEBUG="$d/askpass-debug.log" \
  timeout 10 script -qec "ssh -o PubkeyAuthentication=no \
    -o PreferredAuthentications=password,keyboard-interactive \
    -o UserKnownHostsFile=$d/known_hosts -o StrictHostKeyChecking=yes \
    -p 2222 -l test 127.0.0.1" /dev/null
grep -F 'password auth ok' "$d/server.log"
cat "$d/askpass-debug.log"          # match=password prompt=test@127.0.0.1's password:
! grep -q s3cret "$d/askpass-debug.log" && echo 'no secret in askpass debug'

# 未知 host（空 known_hosts）：即使强制 askpass 也只回答一次 no，快速
# "Host key verification failed."（rc=255），不会把密码当 yes/密码重试。
: > "$d/unknown_known_hosts"
SSH_ASKPASS="$d/askpass.sh" SSH_ASKPASS_REQUIRE=force TERVO_ASKPASS_SECRET=s3cret \
  timeout 10 script -qec "ssh -o PubkeyAuthentication=no \
    -o PreferredAuthentications=password,keyboard-interactive \
    -o UserKnownHostsFile=$d/unknown_known_hosts -p 2222 -l test 127.0.0.1" /dev/null

kill "$srv" 2>/dev/null
```

## 6. 手工验证（GUI）

前置：系统有 `ssh` 服务端/账号；`ssh -V` ≥ 8.4。

1. 启动：`./target/release/tervo`（共享显示器环境请让有权限的 agent 操作；独立
   `TERVO_CONFIG/TERVO_VAULT/TERVO_PROFILES/TERVO_LOG_DIR`，窗口移到空白区）。
2. 打开齿轮 → 左侧 **Vault**：
   - 状态应显示「未初始化」；输入 ≥8 字符主密码 → **Set/Change** → 状态「已解锁 · 0 条凭据」。
   - 点 **Lock** → 状态「已锁定」；再输入主密码 → **Unlock** 恢复。
3. 打开 Profiles → New/Edit：
   - Kind=SSH，填 Host/Port/User，Auth 填 `password`；
   - 出现 **Password**（masked）与 **Save password to Vault**；输入密码并勾选 → Save。
   - 若 Vault 未解锁，表单应显示「Vault 未解锁：请先在 Settings → Vault 解锁…」；解锁后重试。
4. 连接：从 Profile 图标菜单选该 Profile（`Manage Profiles…` 左侧那行）。
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
   - 进程日志（`~/.local/state/tervo/logs/`）不得出现密码；`TERVO_ASKPASS_DEBUG`
     文件也不得出现密码。
6. 错误路径：错主密码 → 错误提示且保持锁定；截断 vault.enc → 报损坏且原文件不被覆盖；
   Clear 第一次点提示确认，第二次清空且主密码/文件保留。

### 6.1 本轮真机结果（2026-11-05）

- 环境：paramiko 测试服务（`--password s3cret`，`127.0.0.1:2222`），Profile
  `id=1 auth=password save_password=true`，`extra_args` 指定独立 known_hosts
  （`StrictHostKeyChecking=yes`），Vault 经 GUI 设置主密码并用 Profile 表单保存密码。
- 连接时应用日志（不含密码）：

  ```text
  [ui] profile pick=0
  [vault] 已取出 Profile 1 的密码（len=6）
  [askpass] 已启用 profile=1 require=force debug=true
  [profile] ssh 命令: ssh -o PubkeyAuthentication=no \
    -o PreferredAuthentications=password,keyboard-interactive -p 2222 -l test \
    -o UserKnownHostsFile=.../known_hosts -o StrictHostKeyChecking=yes 127.0.0.1
  [askpass] 注入 ssh 子进程环境: keys=["SSH_ASKPASS", "SSH_ASKPASS_REQUIRE", "TERVO_ASKPASS_SECRET"] secret_set=true
  [profile] open "vaultssh" kind=ssh
  ```

- `TERVO_ASKPASS_DEBUG`：`match=password prompt=test@127.0.0.1's password:`（脚本确实被调用）。
- 服务端日志：`[ssh-test] password auth ok` + `[ssh-test] pty request 60x20 term=b'xterm-256color'`。
- pane 内容：`SSH-TEST-OK` / `remote$ `（远端 shell 提示符出现）。
- 安全检查：app 日志、askpass 调试文件、profiles.toml、vault.enc 中均搜不到明文密码；
  `vault.enc` 权限 0600。

## 7. 后续工作

- [ ] askpass 改继承 fd/pipe（去掉 `/proc/<pid>/environ` 暴露）。
- [ ] Secret Service/Keyring 后端 + 后端选择说明。
- [ ] `zeroize`/`mlock`、空闲自动锁定、锁屏联动。
- [ ] 解锁弹窗（从连接动作直接弹出，而不是跳设置页）。
- [ ] 凭据逐条查看/删除 UI；密钥口令（key passphrase）也走 Vault。
- [ ] 文件格式升级策略（KDF 参数变更/主密码轮换已经是新 salt+key 重加密）。
