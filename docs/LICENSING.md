# 许可证与合规说明（Licensing）

> 本文是工程性说明，不构成法律意见；对外发布前建议让律师按发行地区复核。

## 1. 结论

| 项目 | 选择 |
|---|---|
| 项目名称 | **xtervo** |
| 代码许可证 | **GPL-3.0-only**（全文见 [`../LICENSE`](../LICENSE)） |
| 版权 | Copyright (C) 2026 xtervo contributors |
| 依赖许可 | 全部为 GPL-3.0 兼容的宽松许可（MIT / Apache-2.0 / Zlib / BSD） |
| 第三方声明 | [`../THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md)（369 个运行时 crate，198 份许可证文本） |
| 商标 | 与 Tabby / Terminus 无关联；“Tabby”仅作功能参考（指名性使用） |

## 2. 为什么是 GPL-3.0-only

UI 依赖 **Slint 1.18**，其许可证为：

```
GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0
```

本项目使用其中的 **GPL-3.0** 选项，因此整个作品按 GPL-3.0-only 分发。
这意味着：

- 分发二进制时，必须同时提供**对应的完整源码**（本仓库）与 `LICENSE`、`THIRD_PARTY_NOTICES.md`；
- 修改后的版本也必须以 GPL-3.0 分发；
- 不能把本项目代码直接用于闭源产品，除非另行取得 Slint 的商业/royalty-free 许可，
  并且由**本仓库版权人**（而非使用者）决定是否对你的使用另行授权。

如果未来希望改成 MIT/Apache-2.0：

1. 先向 Slint GmbH 购买商业或 royalty-free 许可（或替换 UI 框架）；
2. 由版权人把本仓库重新许可（本项目从 MIT 改为 GPL 时已由仓库所有者执行；
   之后若再改回宽松许可，只能由版权人做）。

> 注意：`GPL-3.0-only` 不是 `-or-later`。因为 Slint 的 GPL 选项是 “only”，
> 我们不能把整体标成 “or later”，否则等于替 Slint 授予了它没有授予的未来版本权限。

## 3. 依赖兼容性

| 依赖 | 许可证 | 与 GPL-3.0 兼容 |
|---|---|---|
| Slint 1.18 | GPL-3.0-only / Royalty-free / Commercial | 是（选 GPL 选项） |
| alacritty_terminal 0.26 | Apache-2.0 | 是（FSF 认定 Apache-2.0 与 GPLv3 兼容） |
| portable-pty / arboard / x11rb / serde / toml / log / env_logger / roxmltree / fontdue / bytemuck | MIT 或 MIT/Apache-2.0 双许可 | 是 |
| libc / crossbeam / parking_lot / tokio | MIT 或 MIT/Apache-2.0 | 是 |
| skia-safe（Slint 渲染后端） | MIT | 是 |

完整列表与每份许可证原文见 `THIRD_PARTY_NOTICES.md`（由运行时依赖生成；
开发依赖不进二进制，因此未列入）。

重新生成：

```bash
python3 scripts/gen_third_party_notices.py   # 需要本地 .cargo/registry
```

（脚本会读取 `cargo tree -e normal` 与 registry 里的 crate 许可证文件。）

## 4. 商标与品牌

- **Tabby / Terminus** 是其权利人的商标。xtervo 是独立项目，不做任何关联、赞助或背书暗示；
  文档中提及 Tabby 仅用于功能参考与互操作说明（指名性使用）。
- 项目**更名**：曾用工作名 “Rust Tabby”，为避免与 Tabby 品牌混淆，现统一为 **xtervo**；
  二进制、配置目录（`~/.config/xtervo`）、环境变量（`XTERVO_*`）、桌面项均已改名。
- **图标**：`assets/icon*.png`、`icon-64.rgba` 由 `scripts/gen_icon.py` 程序化生成
  （深色圆角块 + 渐变 “T” + 琥珀光标），不含猫形象、终端红绿灯等 Tabby
  识别性元素，也不含任何第三方美术资源。
- **名称检索**：“xtervo” 是芬兰地名，也可能存在其他商业使用；公开商用前请自行做
  商标检索（本项目不声称拥有该名称的商标权）。

## 5. 资产与内容来源

- 代码：本仓库原创实现（VT 后端使用 alacritty_terminal 作为库，非拷贝代码）。
- 字体：**不随仓库分发字体文件**，只引用系统路径（JetBrains Mono / DejaVu / Noto CJK 等），
  字体许可由系统发行版负责。
- 配色：内置主题只包含颜色数值（Tokyo Night、Dracula、Solarized、Yaru 等社区调色板）；
  名称可能属于各自作者，仓库不包含其代码或美术资源。
- 文档与截图：本项目自产。
- 不包含 Tabby 的源码、图标、截图或主题文件。

## 6. 分发者检查清单

发布二进制 / 安装包时，请一并提供：

- [ ] `LICENSE`（GPL-3.0 全文）
- [ ] `THIRD_PARTY_NOTICES.md`（依赖许可与文本）
- [ ] 对应源码（本仓库的 tag/commit，或书面 offer，GPLv3 §6）
- [ ] 安装后的 `share/doc/xtervo/`（`scripts/install.sh` 已自动安装前两项）
- [ ] 不做 “xtervo 是 Tabby 官方版本” 之类的表述

## 7. 免责声明

软件按 GPL-3.0 的 “AS IS” 条款提供，不附带任何担保。本文不构成法律意见。
