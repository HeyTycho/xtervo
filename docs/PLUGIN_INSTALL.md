# xtervo 插件 v3：本地安装 / 更新 / 回滚 / 签名校验

本文只覆盖**本地**插件包管理，不包含托管市场、云同步、在线 key 分发或信任链。
安装器不会执行插件代码；启用后的权限确认仍由 v3 host 负责。

## 1. 范围与非目标

已实现：

- 从本地目录或 `.zip` / `.tar` / `.tar.gz` 安装 v3 插件。
- 只接受 `api = 3`、`host_abi = 3` 且 manifest 合法的插件。
- 目标目录 `$XDG_CONFIG_HOME/xtervo/plugins/<id>/`（默认 XDG；可用
  `XTERVO_PLUGINS` 覆盖整个插件根目录）。
- 先写 staging，校验通过后原子替换；失败保留旧版本。
- 更新时保留旧版本目录，默认保留最近 3 个；支持 `rollback`。
- 可选的 Ed25519 本地签名校验：`plugin.toml.sig` + 本机公钥。
- Settings → Plugins 的安装/更新/回滚按钮，来源与签名状态展示。

明确非目标：

- 不实现市场、远程索引、自动下载、云同步、评分/评论。
- 不实现公钥分发、证书链、key 轮换服务或在线信任链。
- 不提供插件 OS 沙箱。安装器只保证“写入哪里、写什么”，插件仍以当前用户
  身份运行并拥有该用户的系统权限。
- 不实现插件依赖解析、版本约束求解或 semver 升级/降级判断。
- 不实现 MCP / 渲染 / AI 的插件化。

## 2. 目录布局

```text
$XDG_CONFIG_HOME/xtervo/               # 默认；XTERVO_PLUGINS 可覆盖 plugins 根
├── plugin-signing.pub                # 可选默认 Ed25519 公钥
├── plugin-signing.key                # keygen 生成的私钥（0600，切勿分发）
└── plugins/
    ├── <id>/                         # 已安装插件目录
    │   ├── plugin.toml
    │   ├── plugin.toml.sig           # 可选签名
    │   ├── .xtervo-install.toml       # 安装器元数据（来源/版本/签名状态）
    │   └── entry...
    ├── .staging/                     # 安装/校验临时目录（不会被发现为插件）
    ├── .backup/<id>/                 # 旧版本备份
    └── .install.lock                 # 跨进程安装锁
```

- 发现插件时会跳过所有以 `.` 开头的目录；`.staging` / `.backup` 不会出现在
  设置页或 `xtervo plugins list`。
- `.xtervo-install.toml` 只用于展示来源/签名/文件数，**不被签名覆盖**。签名
  校验永远重新读取 `plugin.toml` 原始字节；本地元数据被改不会把坏签名变成
  好签名。
- 备份目录名格式：`<epoch-ms 19 位>-<sanitized-version>`，按目录名从新到旧。

## 3. 安装包要求

### 3.1 Manifest

```toml
api = 3
host_abi = 3
id = "dev.example.echo"
name = "Echo"
version = "1.0.0"
runtime = "process"
entry = "main.sh"        # 相对插件目录、必须存在且可执行
```

安装器额外检查：

- `plugin.toml` ≤ 64 KiB，合法 TOML，`Manifest::validate()` 通过。
- `id` 只允许 ASCII 字母/数字开头，以及 `A-Za-z0-9._-`；目标是单个目录名，
  不允许 `/`、`..`。
- `version` 非空、≤ 64 字符（不解释 semver）。
- `entry` / `theme` 等相对路径不允许绝对路径、`..`、`.` 段、反斜杠。
- entry 必须是插件目录内的普通可执行文件（Unix 需要执行位）。
- `plugin.toml` 不能是符号链接。

### 3.2 包根布局

支持两种布局：

```text
# 1) 根目录直接放 plugin.toml
plugin.toml
main.sh

# 2) 单一顶层目录包装（zip 常见）
my-plugin/
  plugin.toml
  main.sh
```

如果既没有根目录 `plugin.toml`，也不是“恰好一个顶层目录且其中含
plugin.toml”，安装会被拒绝。多顶层目录、散落文件不会猜测。

### 3.3 归档格式

- `.zip`：只支持 store/deflate；拒绝加密条目、符号链接、特殊文件。
- `.tar` / `.tar.gz` / `.tgz`：拒绝 symlink、hardlink、设备、FIFO、稀疏等
  非普通文件/目录条目。
- 所有条目名必须是 UTF-8；路径先规范化再写入。
- 目录源：递归复制，拒绝任何符号链接、设备/FIFO/socket、非 UTF-8 文件名。

### 3.4 安全限制（默认值）

| 限制 | 默认 |
|---|---:|
| 文件数 | 2 048 |
| 总条目数（含目录） | 4 096 |
| 单文件解压后大小 | 16 MiB |
| 包解压后总大小 | 64 MiB |
| manifest | 64 KiB |
| 签名文件 | 4 KiB |
| 路径长度 | 512 字节 |
| 路径层级 | 16 |
| 保留备份数 | 3（CLI `--keep` 可设 1–10） |

解压/复制时按实际读到的字节数计数，不信任归档头声称的大小；声明大小与
实际不符会拒绝。

### 3.5 路径逃逸防护

安装器对目录源和归档统一执行：

- 拒绝绝对路径（`/...`、Windows prefix）。
- 拒绝 `..`（zip slip / tar traversal）、含反斜杠的路径。
- 规范化后必须留在 staging 内；staging 创建在插件根目录下、权限 0700。
- 拒绝符号链接和硬链接；解包后再做一次全树扫描，任何 symlink/特殊文件都
  会导致回滚并删除 staging。
- 拒绝重复路径、文件/目录同名冲突、保留文件名 `.xtervo-install.toml`。
- 不向插件目录之外写任何内容；失败时删除 staging，不碰旧版本。

## 4. 安装、更新、回滚

### 4.1 命令行

```bash
# 本地目录
xtervo plugins install /path/to/plugin

# zip / tar / tar.gz
xtervo plugins install /path/to/plugin.zip
xtervo plugins install /path/to/plugin.tar.gz

# 更新：要求同 id 已安装；成功后旧版本进入 .backup/
xtervo plugins update /path/to/new-version.zip

# 只校验，不安装（同样跑 manifest/路径/大小/签名检查）
xtervo plugins verify /path/to/plugin.zip

# 查看已安装插件、来源、签名状态
xtervo plugins list

# 查看某插件的回滚备份
xtervo plugins backups dev.example.echo

# 回滚到最新一个“与当前版本不同”的备份
xtervo plugins rollback dev.example.echo

# 指定版本号或备份目录名
xtervo plugins rollback dev.example.echo --to 1.0.0
xtervo plugins rollback dev.example.echo --to 0000000000000000000-1.0.0

# 第一次安装也保留备份；--keep 控制数量（1–10）
xtervo plugins update pkg.zip --keep 2
```

`install` 与 `update` 的区别：`update` 要求 `<plugins>/<id>` 已存在，且磁盘上
旧目录的 manifest id 必须与新包一致；`install` 可以首次安装，也可以对同 id
重装（旧目录仍会备份）。版本号不排序：同版本按重装处理，降级也会被接受，
调用方需自己确认包内容。`--force` 只用于目标目录存在但属于**另一个 id** 或
无法解析 manifest 的情况；它会先备份再覆盖，普通升级不需要。

退出码：

- `0`：成功。
- `1`：校验/安装/回滚失败（错误写到 stderr）。
- `2`：用法错误（缺少路径、未知选项等）。

CLI 在初始化 Slint/窗口之前直接退出，无 `DISPLAY` 的打包/CI 环境可安全使用。
它不会启动插件进程。

### 4.2 原子替换与失败语义

Linux 上优先使用 `renameat2(..., RENAME_EXCHANGE)` 原子交换 staging 和目标
目录；交换后旧版本完整转移到 `.backup/<id>/`。文件系统不支持 exchange 时
回退为两段 `rename`：先把旧目录 rename 到备份，再把 staging rename 到目标；
第二段失败会尝试把备份 rename 回原位。

- 校验失败：旧版本完全不动，staging 删除。
- exchange 成功但旧版本归档 rename 失败：新版本已就位，旧版本保留在
  `.staging`（不会被删除），报告里给出路径 warning。
- 进程在 exchange 与归档 rename 之间崩溃：目标目录是新版本；旧版本完整留在
  `.staging/<...>`。安装器不会自动猜测/删除它，需人工确认后清理。
- 安装/回滚用 `flock` 做跨进程互斥；并发操作会明确报错（短暂 EWOULDBLOCK
  会重试最多 500 ms，避免插件 supervisor fork/exec 的瞬时 fd 继承误报）。

### 4.3 回滚语义

- 默认选择“最新且版本与当前不同”的备份；这样可以连续回滚到更早版本，而
  不会在两份相同版本间来回切换。
- `--to <version>` 或 `--to <backup-dir-name>` 可指定备份；仍会重新校验该
  备份的 manifest、entry 和签名（按当前签名策略），损坏/不满足当前策略的
  备份会被拒绝。
- 回滚前的当前版本会归档为新的 `.backup/<id>/...`，因此可以在需要时再用
  `--to <旧版本>` 回滚回去。
- 回滚后只保留最近 N 个备份（默认 3）。

## 5. Ed25519 本地签名

### 5.1 格式

签名覆盖 `plugin.toml` 的**原始字节**（不规范化换行/空白）：

```text
plugin.toml.sig
  xtervo-plugin-signature-v1
  <base64(64 字节 Ed25519 signature)>
```

公钥文件：

```text
xtervo-plugin-pubkey-v1
<base64(32 字节 Ed25519 public key)>
```

也接受裸 base64、64 位 hex，或 PEM `-----BEGIN PUBLIC KEY-----`
（Ed25519 SubjectPublicKeyInfo）。私钥文件由 keygen 生成：

```text
xtervo-plugin-signing-key-v1
<base64(32 字节 seed)>
```

### 5.2 生成与签名

```bash
# 默认写到 <config>/xtervo/plugin-signing.{key,pub}
xtervo plugins keygen

# 或指定目录；--force 覆盖已有 key
xtervo plugins keygen --out-dir ~/.config/xtervo --force

# 给插件目录的 plugin.toml 生成 plugin.toml.sig
xtervo plugins sign /path/to/plugin --key ~/.config/xtervo/plugin-signing.key

# 然后再打 zip/tar（签名文件必须在包内与 plugin.toml 同级）
cd /path/to && zip -r plugin.zip plugin
```

私钥文件权限为 0600；公钥为 0644。私钥不要放进插件包，也不要提交到仓库。

### 5.3 公钥查找顺序

1. 代码内显式传入（`InstallOptions::signature_config` / `--pubkey` 测试入口）。
2. 环境变量 `XTERVO_PLUGIN_PUBKEY`：可以是 key 内容、文件路径或 `@路径`。
3. 默认文件：`<plugins 根目录的父目录>/plugin-signing.pub`，默认即
   `$XDG_CONFIG_HOME/xtervo/plugin-signing.pub`。使用 `XTERVO_PLUGINS` 时，默认
   公钥文件位于该目录的父目录。

### 5.4 强制签名策略

满足任一即“要求签名”：

- 环境变量 `XTERVO_PLUGIN_REQUIRE_SIGNATURE=1`（也接受 true/yes/on；0/false/
  no/off 关闭）；设置页 Plugins 的 “Require Ed25519 signature” 开关；
- `config.toml` 的 `plugin_require_signature = true`。

行为：

| 包内签名 | 本机公钥 | 要求签名 | 结果 |
|---|---|---|---|
| 无 | 无 | 否 | 允许，状态 `unsigned` |
| 无 | 有 | 否 | 允许，状态 `unsigned` |
| 无 | 任意 | 是 | 拒绝 |
| 有且有效 | 有 | 任意 | 允许，状态 `verified` |
| 有但无效/被篡改 | 有 | 任意 | 拒绝（坏签名不会被“可选”绕过） |
| 有 | 无 | 任意 | 拒绝（无法验证的签名不能当通过） |

要求签名但没有配置公钥时，安装/回滚直接报错并提示设置
`XTERVO_PLUGIN_PUBKEY` 或默认公钥文件。

### 5.5 签名边界

- 只证明“持有对应私钥的人签过这份 `plugin.toml` 的原始字节”。
- 不证明作者身份、不提供撤销、过期、key 轮换或信任链。
- 不覆盖 entry 二进制、主题文件等其他资源；如需完整性，发布方应把摘要写进
  manifest 或对整个包做外部校验。
- `.xtervo-install.toml` 来源展示只是本地记录，不是签名证据。
- 公钥来源完全由本机用户决定；安装器不会联网获取 key。

## 6. 设置页 Plugins

Settings → Plugins 现在提供：

- **Require Ed25519 signature** 开关（写入 `plugin_require_signature`）。
- **Install from folder or package** 路径框 + “Choose file…” / “Choose
  folder…” + “Install”。文件选择器仍走 zenity（`XTERVO_FILE_DIALOG` 可替换），
  无法启动时可手动输入路径。
- 每个已安装插件行显示 `Source:` 与 `Signature:`（实时校验）。
- `Update…`：使用路径框中的包，更新同 id 插件；路径为空时提示先选择。
- `Rollback to <version>`：仅在存在与当前版本不同的备份时显示。
- `Enable` / `Disable` / `Reload` / `Restart` / `Open folder` 仍走既有 v3 host；
  安装/更新/回滚不会重启 xtervo。

安装/更新/回滚在 UI 线程同步执行本地文件操作；不会执行插件代码。启用状态下
更新成功后，store 只 reload 受影响的插件（权限变化会使旧确认失效并要求重新
确认）；不会重载无关插件。

## 7. 测试

```bash
# 安装/签名/逃逸/限额/回滚单元测试（20 个）
cargo test -p xtervo-app --bin xtervo plugin_v3::install_tests

# v3 host + 安装全部测试
cargo test -p xtervo-app --bin xtervo plugin_v3::

# 全量 app 测试
cargo test -p xtervo-app --bin xtervo
```

覆盖：目录/zip/tar/tar.gz 安装、坏 manifest、zip slip、tar traversal、
symlink、超限、更新失败保留旧版本、更新备份、回滚、备份裁剪、exchange
回退路径、签名通过/坏签名/缺少公钥/强制签名。

CLI 手工验收（无需 UI）：

```bash
export XTERVO_PLUGINS=/tmp/xtervo-plugins
xtervo plugins keygen --out-dir /tmp/xtervo-keys
xtervo plugins sign /path/to/plugin --key /tmp/xtervo-keys/plugin-signing.key
XTERVO_PLUGIN_PUBKEY=@/tmp/xtervo-keys/plugin-signing.pub \
  xtervo plugins verify /path/to/plugin
XTERVO_PLUGIN_PUBKEY=@/tmp/xtervo-keys/plugin-signing.pub \
  xtervo plugins install /path/to/plugin
xtervo plugins list
```

## 8. 已知限制

- 不校验插件文件（entry/资源）的签名或哈希，只签 `plugin.toml`。
- 不做版本排序：`update` 不阻止降级；同版本会重装备份。
- 归档只支持 zip store/deflate、tar/tar.gz；非 UTF-8 条目名直接拒绝。
- zip 未保留 Unix 执行位时，entry 会因不可执行而拒绝；发布方需保留 0755。
- 不做崩溃后的自动恢复：exchange 后崩溃会在 `.staging` 留下旧版本，需要人工
  清理；不会自动删除旧数据。
- 备份默认只保留 3 个，且 `--keep` 上限 10。
- 签名策略是“本机公钥 + 可选强制”，不是市场信任模型；无撤销/轮换。
- 安装器不是沙箱：插件启用后仍以当前用户权限运行。
- 不实现市场、远程更新检查、云同步、key 分发。
