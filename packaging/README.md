# 打包、安装与升级

本文覆盖 W-05 的交付：可复现的 tar.gz / `.deb` / `tervo-stripped`、安装/升级/
回滚/卸载、版本信息与配置迁移，以及明确列出的未完成项。

## 产物

`scripts/package.sh` 一次生成：

| 产物 | 内容 |
|---|---|
| `dist/tervo-stripped` | strip 后的发布二进制；`dist/tervo` 是指向它的兼容软链 |
| `dist/tervo-stripped.sha256` | 单文件 SHA-256 |
| `dist/SHA256SUMS` | 二进制 + tar.gz + .deb 的 SHA-256 |
| `dist/tervo-<version>-linux-<arch>.tar.gz` | `tervo`、`install.sh`、`LICENSE`、`README.md`、`THIRD_PARTY_NOTICES.md`、`LICENSING.md`、`BUILD-INFO`、`share/applications/tervo.desktop`、512/64 图标 |
| `dist/tervo_<version>-1_<arch>.deb` | `/usr/bin/tervo`、desktop、512/64 图标、`/usr/share/doc/tervo/*`；含 `Depends`/`Installed-Size`，`dpkg-deb -x` 无需 root 即可解包 |

`target/release/tervo` 仍是开发/性能分析二进制（`debug = 1`，保留符号），打包只
strip 复制出来的一份；`dist/`、`*.stripped` 已在 `.gitignore`。

### 可复现

同 commit、同工具链、clean worktree 下重复执行 `package.sh`，`tervo-stripped`、
tar.gz、`.deb` 和 `SHA256SUMS` 逐字节一致（`scripts/test_packaging.sh` 会实测）。
默认把 `HEAD` 的提交时间设为 `SOURCE_DATE_EPOCH`：

- 二进制里 `--version` 的 `built` 时间；
- tar/deb 内所有文件 mtime；
- gzip 时间戳与 ar/deb 时间戳（`dpkg-deb` 支持 `SOURCE_DATE_EPOCH`）。

可用 `SOURCE_DATE_EPOCH=<unix 秒>` 覆盖。默认拒绝 dirty worktree（产物里的
commit 会与内容不符）；临时开发包用 `ALLOW_DIRTY=1`，`--version` 会标 `(dirty)`。

### 用法

```bash
./scripts/package.sh                            # → dist/
TERVO_DIST=/tmp/dist ./scripts/package.sh       # 自定义输出目录（相对仓库根）
./scripts/package.sh /tmp/dist                  # 同上，位置参数
SKIP_BUILD=1 ./scripts/package.sh               # 复用 target/release/tervo
SKIP_DEB=1 ./scripts/package.sh                 # 只出 tar.gz/二进制
SKIP_TAR=1 ./scripts/package.sh                 # 只出 .deb/二进制
MAX_MB=64 ./scripts/package.sh                  # 二进制体积门禁（默认 64 MiB）
ALLOW_DIRTY=1 ./scripts/package.sh              # 未提交修改也打包（产物会标 dirty）
DEB_VERSION=0.1.0-2 ./scripts/package.sh        # 覆盖 .deb 版本
```

环境变量全表见脚本头部注释与 `-h`。

### `.deb`

```bash
dpkg-deb -I dist/tervo_0.1.0-1_amd64.deb        # 查看控制字段
dpkg-deb -x dist/tervo_0.1.0-1_amd64.deb /tmp/root   # 无需 root 解包
sudo apt install ./dist/tervo_0.1.0-1_amd64.deb      # 真机安装（需 sudo）
```

`Depends` 由 `dpkg-shlibdeps` 从 ELF 的 `NEEDED` 生成，再补 Slint/winit 运行时
dlopen 的 X11/XKB 库。`libgl1`、`fonts-dejavu-core` 放在 `Recommends`：没有 GPU
时程序会回退 `winit-software`，缺 GL 不影响基本使用。`DEB_DEPENDS` 可整体覆盖。

## 安装 / 升级 / 回滚 / 卸载

仓库内或 tar 解压后都使用同一个 `install.sh`：

```bash
./scripts/install.sh                            # ~/.local（仓库内按需构建 release）
PREFIX=/tmp/prefix ./scripts/install.sh          # 自定义 prefix
./install.sh --prefix /tmp/prefix                # tar 解压目录内
./scripts/install.sh --no-strip                  # 调试时保留符号
```

安装路径（`$PREFIX` 默认 `~/.local`）：

| 路径 | 用途 |
|---|---|
| `$PREFIX/bin/tervo` | 当前二进制 |
| `$PREFIX/lib/tervo/previous/tervo` | **上一版二进制**（`--rollback` 的明确路径） |
| `$PREFIX/lib/tervo/previous/BUILD-INFO` | 上一版版本/commit/构建时间 |
| `$PREFIX/lib/tervo/versions/<version>-<commit>/tervo` | 历史版本归档（同一版本只保留首次副本） |
| `$PREFIX/lib/tervo/manifest` | 安装清单（卸载按此删除） |
| `$PREFIX/lib/tervo/install.sh` | 生命周期脚本（从安装后的位置回滚/卸载） |
| `$PREFIX/share/applications/tervo.desktop` | 桌面项 |
| `$PREFIX/share/icons/hicolor/{512x512,64x64}/apps/tervo.png` | 图标 |
| `$PREFIX/share/doc/tervo/` | LICENSE/README/THIRD_PARTY_NOTICES/LICENSING/BUILD-INFO |

### 原子替换

新二进制先写到 `$PREFIX/bin/.tervo.new.XXXXXX`（同一文件系统），strip、冒烟、
`--version` 探测都通过后才 `mv -f` 覆盖 `$PREFIX/bin/tervo`。Linux 的 rename
不会破坏正在运行的旧进程：它继续持有旧 inode。e2e 会在旧进程运行时升级并检查
旧 pid 仍存活。桌面项/图标/文档也用同目录临时文件 + rename，不产生半截文件。

### 升级

```bash
# 用新 tar 包覆盖安装；旧二进制自动进入 previous/ 与 versions/
./tervo-0.1.1-linux-x86_64/install.sh --prefix /tmp/prefix
```

安装器只替换程序文件，**不碰** `~/.config/tervo`（或用 `TERVO_CONFIG`/
`TERVO_PROFILES` 指定的路径）。升级前后配置/Profile 校验和不变；迁移逻辑只在
程序启动读配置时发生，且迁移前会写 `.v<旧版本>.bak`：

- `config.toml` → `config.toml.v0.bak`（v0→v1）
- `profiles.toml` → `profiles.toml.v1.bak`（v1→v2）

### 回滚

```bash
$PREFIX/lib/tervo/install.sh --rollback --prefix $PREFIX
```

回滚把 `previous/tervo` 原子换回 `bin/tervo`，同时把被替换的当前版存回
`previous/`，所以再执行一次 `--rollback` 可以切回去。配置/Profile/迁移备份
完全不动。没有上一版时以非 0 退出并打印缺失路径。

### 卸载

```bash
$PREFIX/lib/tervo/install.sh --uninstall --prefix $PREFIX
# 或 tar 包内：./install.sh --uninstall --prefix $PREFIX
```

只删除 manifest 列出的程序文件与 `$PREFIX/lib/tervo` 状态目录，随后
`rmdir --ignore-fail-on-non-empty` 清空目录；不递归删除用户可能放进 prefix 的
其它文件。`~/.config/tervo` 的配置、Profile、主题与迁移备份全部保留。重复执行
幂等（退出码 0，打印“未发现已安装的 Tervo”）。

## 版本信息

```text
$ tervo --version
Tervo 0.1.0
commit:  4ae1886e8c39
built:   2026-11-06T14:07:47Z
target:  x86_64-unknown-linux-gnu
```

- `commit` 来自构建时的 git HEAD（工作区有未提交修改时加 `(dirty)`）；
- `built` 优先取 `SOURCE_DATE_EPOCH`（发布包＝提交时间），本地构建取当前 UTC；
- `--version`/`-V` 在 logging、Slint 后端、窗口初始化之前返回，无 DISPLAY 也可用，
  不写配置/日志。

设置页与 `--help` 仍显示 `CARGO_PKG_VERSION`；`--version` 是打包/升级脚本判断
版本的唯一入口。

## 测试

```bash
# 默认构建 + 打包 + 全套安装/升级/回滚/卸载；临时目录结束即清理
./scripts/test_packaging.sh

# 复用 release 二进制、保留临时目录排查
SKIP_BUILD=1 KEEP_TEST=1 ./scripts/test_packaging.sh

# 跳过二次打包字节对比（快速迭代）
SKIP_REPRO=1 ./scripts/test_packaging.sh
```

覆盖（当前 72 项断言）：

1. `package.sh` 产物存在、校验和自检；
2. 同 commit 二次打包，二进制/tar/deb/校验和逐字节一致；
3. tar 解压 + 干净 prefix 安装：bin/desktop/图标/文档/清单/生命周期脚本；
4. Xvfb :141 启动 smoke（`--exit-after-first`；找不到 Xvfb 时明确 SKIP）；
5. 配置 v0 / Profile v1 通过安装后的二进制迁移，旧值保留、`.bak` 存在；
6. 升级到伪造的 9.9.9 包：previous/、versions/ 生成；旧进程在 rename 后仍存活；
   配置/Profile 校验和不变；
7. 回滚到 0.1.0、再回滚到 9.9.9；每步配置/Profile/备份校验和不变；
8. `.deb` 用 `dpkg-deb -x` 解包，检查 Depends/desktop/图标/copyright，解包二进制
   `--version`/`--help`；
9. 卸载后程序文件与状态目录移除，配置/Profile/迁移备份保留；重复卸载幂等；
10. `bash -n`；有 `shellcheck` 时按 warning 级别检查（当前环境未安装，SKIP）。

## 未完成 / 明确不做

以下能力**没有**实现，也没有对应的假按钮或占位 UI：

- **自动更新**：不检查更新、不下载、不静默替换。升级方式就是重新运行新 tar 的
  `install.sh` 或安装新 `.deb`；回滚走 `--rollback`。整个程序没有新增网络依赖。
- **崩溃报告/上传**：不收集、不上传崩溃转储，也没有“发送报告”按钮。panic
  仍只写本地 stderr 与日志（`TERVO_LOG_DIR`，默认 `~/.local/state/tervo/logs`）。
  日志轮转见 `docs/LOGGING.md`。
- **AppImage / Flatpak / Snap**：本轮未产出，也没有脚本入口。
- **发布签名 / 仓库元数据**：`.deb` 未用 GPG 签名，未生成 `Release`/`Packages`
  仓库；SHA-256 只能校验完整性，不能证明来源。
- **自包含运行时**：发布二进制仍动态链接系统库（fontconfig/freetype/X11/XKB），
  不能直接拷到缺少这些库的机器；`Depends` 只覆盖 Debian/Ubuntu 系包名。
- **跨架构**：默认按本机 `rustc` host 生成（当前 x86_64）；交叉编译需自行设置
  `STRIP`/`SKIP_SMOKE=1`，`.deb` 架构名映射只覆盖 x86_64/aarch64 加本机
  `dpkg --print-architecture` 回退。
- **`.deb` 不含 install.sh/previous**：`.deb` 只装程序文件，升级/卸载交给
  `apt`/`dpkg`；需要“保留上一版 + `--rollback`”时用 tar 包里的 `install.sh`。

## 兼容性注意

- `install.sh` 现在会解析 `--prefix` / `--no-strip` / `--rollback` / `--uninstall` /
  `-h`；未知参数退出码 2（旧版会静默忽略）。
- 发布包内 `install.sh` 已 strip，重复安装不会再次 strip，保证与 `.sha256` 一致；
  仓库内安装默认 strip 复制出的副本，开发二进制不变。
- 旧版安装（只有 `bin/tervo`、没有 manifest）可被升级/卸载：升级会把它归档到
  `previous/`，卸载会按已知路径删除。
- 运行中的二进制可安全覆盖；但如果正在运行的是**更老、不支持 `--version`** 的
  版本，安装器不会去执行它探测版本（避免拉起 UI），该版在 `versions/` 里以
  `unknown` 标记。
