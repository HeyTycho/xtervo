# SSH 证书 / `@cert-authority` 调研（2026-11-20）

结论：**Tervo native SSH 暂不支持主机证书信任链**。使用
`@cert-authority` 的 Profile 会继续回退系统 `ssh`；native 不会把证书当作
普通主机公钥接受，也不会“看起来支持、实际跳过校验”。

## 库能力

- `russh 0.63.3` 的 `PublicKeyOrCertificate` 已区分
  `PublicKey` / `Certificate`，客户端 `check_server_key` 能拿到
  `ssh_key::Certificate`。
- `ssh-key 0.7.0-rc.11` 提供：
  - `cert_type()`、`valid_principals()`、`valid_after()` / `valid_before()`
  - `signature_key()`、`signature()`、`verify_signature()`
  - `validate()` / `validate_at()`：校验时间窗口、证书签名和 CA SHA-256
    指纹；文档明确要求调用方再校验 principals 与 critical options。
- 因此密码学原语足够，不需要自己解析证书签名。

## 当前 Tervo 的状态

- `known_hosts_rs::lookup` 明确跳过 `@cert-authority` 行；`Lookup` 只返回
  plain/hashed 主机 key 与 `@revoked` key。
- `HostKeyHandler::check_server_key` 对 `Certificate` 直接返回 `false`。
- `can_use` 要求 known_hosts 里有普通主机 key；只有 CA 行的 Profile 不会走
  native。
- known_hosts 首次确认流程（`ssh-keyscan` + 指纹弹窗 + `trust_keys`）只处理
  普通主机 key；不会把 CA 行当作已确认条目。

## 安全实现所需工作（未做）

1. `known_hosts_rs` 保存匹配当前 host 的 `@cert-authority` CA key，并保留
   host pattern 语义（plain/hashed/wildcard/negation）。
2. `check_server_key` 的 Certificate 分支：
   - 必须是 `CertType::Host`；
   - `valid_principals` 为空或匹配目标 hostname（glob）；
   - 时间窗口有效；
   - `validate(&ca_fingerprints)` 通过（签名 + CA 指纹）；
   - `critical_options` 只接受已实现的项（当前应保守拒绝非空）；
   - 与 `@revoked` 的交互需要单独设计。
3. 主机密钥预检/首次确认流程识别 CA 证书（`ssh-keyscan -c` 或 in-process
   握手），否则每次连接都会先弹“未知主机”，无法发挥 CA 作用。
4. 用生成的 CA + host cert 做 native 回归，覆盖过期、principal 不匹配、
   错误 CA、critical option。

在上述完成前，只记录 unsupported，不把证书伪装成已校验的普通 key。
