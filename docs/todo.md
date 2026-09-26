# 待办清单

Tack 的人工/跨仓库待办事项(代码内的 TODO 见源码注释,这里只记需要在仓库之外
操作或跨步骤协调的事)。

## 待办

### 1. 提交 Tack 到 ACP Registry(手工 PR)

**背景**:ACP 协议本身没有 icon 字段,Zed/JetBrains 等客户端的 agent 图标和
一键安装都来自 [ACP Registry](https://agentclientprotocol.com/get-started/registry)。
提交材料已备好:`assets/acp-registry/tack/`(合规单色图标 `icon.svg` +
6 平台 `agent.json` 草稿 + 详细说明见 `assets/acp-registry/README.md`)。

**步骤**:

1. Fork <https://github.com/agentclientprotocol/registry>
2. 把 `assets/acp-registry/tack/` 整个目录复制到 fork 仓库根目录
   (目录名 `tack` 必须与 `agent.json` 的 `id` 一致)
3. 把 `agent.json` 里所有 `FILL_FROM_SHA256SUMS` 替换为目标 release
   `SHA256SUMS.txt` 中的实际哈希;`version` 与下载 URL 中的 tag 版本对齐
4. 提 PR,等 CI 校验(schema + 图标规则)与人工审核

**后续(每次发版)**:registry 条目指向固定版本,发新版后需再提 PR 更新
`version` / URL / sha256。稳定几次后考虑把"更新 ACP registry 条目"编入
`docs/release.md` 的发布清单(或写脚本半自动化)。

**需要**:repo owner 身份操作(fork + PR 以项目名义提交)。

## 已完成

| 日期 | 事项 |
|---|---|
| 2026-09-25 | ACP `initialize` 上报 `agentInfo`(name/title/version),随下一 release 生效(217bd14) |
