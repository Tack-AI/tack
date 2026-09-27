# Tack 发布流程

**[English](release.md) | 简体中文**

本文档描述 Tack 的版本发布与跨平台打包流程。打包由 GitHub Actions 自动完成
（`.github/workflows/tack-release.yml`），发布产物供 `tack update` 自更新命令
消费（约定见 `crates/tack-app/src/self_update.rs`）。

## 1. 发布约定

| 项目 | 约定 |
|---|---|
| tag 格式 | `tack-vX.Y.Z`（如 `tack-v0.8.0`） |
| 版本号位置 | `Cargo.toml` 的 `workspace.package.version`（单一事实来源） |
| 资产命名 | `tack-<target-triple>.tar.gz`（Linux/macOS）、`tack-<target-triple>.zip`（Windows） |
| 压缩包内容 | 根目录单个二进制：`tack`（Unix）或 `tack.exe`（Windows） |
| 校验文件 | `SHA256SUMS.txt` |

tag 中的版本号必须与 `workspace.package.version` **完全一致**，否则 workflow
的 prepare 阶段会直接失败。这是刻意的防呆设计：`tack update` 用资产名中的
target triple 匹配当前平台，用 tag 中的版本号比较新旧，两者都不能错。

目标平台矩阵（6 个 target）：

| target triple | runner | 说明 |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` | |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | 原生 ARM runner，公共仓库免费 |
| `x86_64-apple-darwin` | `macos-15-intel` | macos-13 已退役，勿用 |
| `aarch64-apple-darwin` | `macos-latest` | Apple Silicon |
| `x86_64-pc-windows-msvc` | `windows-latest` | |
| `aarch64-pc-windows-msvc` | `windows-latest` | 交叉编译（镜像自带 ARM64 MSVC 工具链） |

修改平台矩阵时必须同步更新 `self_update.rs` 中的 `target_triple()`，否则对应
平台的用户将无法自更新。

## 2. 标准发布步骤

```bash
# 1. 确认 main 分支是绿的（CI 通过、工作区干净）
git switch main && git pull

# 2. 本地预检，命令与 ci.yml 完全一致。tag 推送不触发 ci.yml，
#    不打 tag 就发现不了格式/lint 问题，所以必须在本地先跑。
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings

# 3. 提升版本号：编辑 Cargo.toml 的 workspace.package.version
#    如 0.8.0 → 0.9.0

# 4. 更新 CHANGELOG.md（用户可见的变更）

# 5. 提交版本提升
git add Cargo.toml CHANGELOG.md
git commit -m "release: v0.9.0"
git push

# 6. 打 tag 并推送（触发打包 workflow）
git tag tack-v0.9.0
git push origin tack-v0.9.0
```

推送 tag 后，workflow 自动执行三个阶段：

1. **prepare** — 校验 tag 版本与 `Cargo.toml` 一致、检查 CHANGELOG 条目、
   跑 fmt + clippy 预检（快速失败，避免浪费 6 平台并行构建），创建 GitHub
   Release（release notes 由 `--generate-notes` 自动生成，可事后在网页编辑）
2. **build** — 6 平台并行 `cargo build --release --locked --package tack-app`，
   各自打包为 tar.gz / zip
3. **publish** — 汇总产物、生成 `SHA256SUMS.txt`，上传到 Release

全程约 10–20 分钟（无缓存时更久）。完成后在
`https://github.com/<owner>/<repo>/releases` 检查 7 个资产是否齐全
（6 个压缩包 + `SHA256SUMS.txt`）。

## 3. 其他触发方式

### 重新发布已有 tag

tag 已存在但需要重新打包（如 workflow 修复后）：

1. 在 GitHub 网页删除旧的 Release（不需要删 tag）
2. Actions → "Tack release" → Run workflow → 输入 tag 名（如 `tack-v0.9.0`）

workflow_dispatch 会 checkout 该 tag 的代码进行构建，同样会校验版本号一致。

### 私有 fork 的注意点

- `ubuntu-24.04-arm` 原生 ARM runner 仅对公共仓库免费。私有仓库需将其替换为
  `ubuntu-latest` + [`cross`](https://github.com/cross-rs/cross) 交叉编译，
  或改用 cargo-zigbuild。
- `tack update` 默认从 `DEFAULT_UPDATE_REPO`（`Tack-AI/tack`）拉取。
  fork 发布自己的包时，用户需通过环境变量 `TACK_UPDATE_REPO=<owner>/<repo>`
  或 settings 中的 `updateRepo` 指向 fork 仓库。

## 4. 验证发布

发布完成后，用旧版二进制验证自更新链路：

```bash
tack update --check   # 应显示新版本与对应平台的资产名
tack update           # 下载、校验 --version 可运行、替换二进制
tack --version        # 应显示新版本号
```

也可手动验证压缩包完整性：

```bash
curl -sLO https://github.com/<owner>/<repo>/releases/download/tack-v0.9.0/tack-x86_64-unknown-linux-gnu.tar.gz
curl -sL https://github.com/<owner>/<repo>/releases/download/tack-v0.9.0/SHA256SUMS.txt | grep x86_64-unknown-linux-gnu
sha256sum tack-x86_64-unknown-linux-gnu.tar.gz   # 与上行比对
```

最后同步 ACP registry 草稿（`assets/acp-registry/tack/agent.json`）：将
`version` 与各 archive URL 中的 tag 更新为新版本，并用已发布
`SHA256SUMS.txt` 中的值替换 sha256，保持草稿处于可提交状态（见
`assets/acp-registry/README.md`）。

## 5. 故障排查

| 症状 | 原因与处理 |
|---|---|
| prepare 阶段报 "tag does not match workspace.package.version" | tag 版本号与 Cargo.toml 不一致。删 tag（`git push origin :tack-vX.Y.Z`）、改对后重打 |
| prepare 阶段 fmt/clippy 失败 | tag 的代码未通过预检（说明发布前没跑第 2 步）。修复合并进 main 后删 tag 重打，或修复后 workflow_dispatch 重发 |
| 某个平台 build 失败 | 单个矩阵 leg 失败不影响其他 leg（`fail-fast: false`），但 publish 会因缺产物而失败。修复后用 workflow_dispatch 重发 |
| `tack update` 找不到资产 | 检查资产名是否与 `self_update.rs` 的 `asset_name()` 一致；Release 是否属于 `tack-v*` 系列（`/releases/latest` 接口不会被用到） |
| macOS x86_64 leg 排队不动 | 确认用的是 `macos-15-intel` 而非已退役的 `macos-13` |
| Cargo.lock 相关构建失败 | workflow 使用 `--locked`，发布前确保本地 `cargo build --locked` 通过且 Cargo.lock 已提交 |
