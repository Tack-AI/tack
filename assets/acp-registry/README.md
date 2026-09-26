# ACP Registry 提交材料

把 Tack 提交到 [ACP Registry](https://agentclientprotocol.com/get-started/registry)
后,Zed / JetBrains 等客户端的 agent 列表会显示本目录的 `icon.svg` 作为 Tack 的
图标,并支持一键安装(协议本身没有 icon 字段,图标由 registry 托管)。

## 文件

- `tack/icon.svg` — 单色(`currentColor`)16×16 图标,符合 registry 校验规则
  (硬编码颜色会被 CI 拒)。由 `assets/logo.lini` 派生:编译后取路径、归一化到
  16×16 viewBox、fill 改为 `currentColor`。
- `tack/agent.json` — 条目草稿。平台键(registry 侧)与 target triple(release
  资产侧)的对应关系见下表;URL 模式:
  `https://github.com/sufar/tack/releases/download/tack-vX.Y.Z/tack-<triple>.<tar.gz|zip>`

| registry 平台键 | release 资产 triple | 格式 |
|---|---|---|
| `darwin-aarch64` | `aarch64-apple-darwin` | tar.gz |
| `darwin-x86_64` | `x86_64-apple-darwin` | tar.gz |
| `linux-aarch64` | `aarch64-unknown-linux-gnu` | tar.gz |
| `linux-x86_64` | `x86_64-unknown-linux-gnu` | tar.gz |
| `windows-aarch64` | `aarch64-pc-windows-msvc` | zip |
| `windows-x86_64` | `x86_64-pc-windows-msvc` | zip |

## 提交步骤

1. Fork <https://github.com/agentclientprotocol/registry>。
2. 把 `tack/` 整个目录复制到 fork 仓库根目录(目录名必须与 `id` 一致)。
3. 把 `agent.json` 里所有 `FILL_FROM_SHA256SUMS` 替换为对应 release
   `SHA256SUMS.txt` 中的实际哈希;`version` 与 URL 中的 tag 版本同步。
4. 提 PR;CI 会校验 `agent.json` schema 和图标规则。

## 注意

- `agent.json` 的 `version` 和下载 URL 指向**某一个具体 release**;之后每次发版
  需要向 registry 提 PR 更新(可考虑把这一步并入 `docs/release.md` 的发布清单)。
- 客户端显示名来自 `name` 字段；ACP `initialize` 握手里的 `agentInfo`
  (name/title/version)已由 Tack 上报（随下一 release 发布），用于客户端的
  about/调试界面。
