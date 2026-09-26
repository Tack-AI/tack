# Ubuntu check 卡死排查记录（"幽灵 wedge" 事件）

2026-09 下旬，CI 的 ubuntu `check` job 连续多次在 Test 步**完全静默地卡死**：
不报错、不超时、不留日志，直到被 job timeout 或手动取消收割。macOS/Windows
的 test-cross 跑同一份测试**永远全绿**，本机也从不复现。本文记录症状、
排查路径、最终实锤的**四个连环真凶**与修复，以及可复用的取证方法。

最终修复见：`aec16b2`（git 子进程）、`9637f88`（kill_process_tree）、
`9960e88`/`7490d99`（缓存与构建资源）、`6fd492f` 起（Docker 牢笼）、
`19d5995`（拆除临时脚手架）。

## 症状

- `check` job（ubuntu）的 Test 步打出若干测试行后**骤停**，之后数十分钟
  零输出，step 永远停在 in_progress，约 50 分钟后被 GitHub 以
  `lost communication with the server` 收割。
- **挂点逐次漂移**：vertex_adc_tests 3/5 处、tack-app extension_host 一带、
  甚至全量编译期——位置不固定，但都在 ubuntu。
- **日志全灭**：事后下载日志 zip 永远没有 check job；直接拉 job 日志 API
  返回 `BlobNotFound`。实时视图里能看到流出去的最后几行，仅此而已。

## 真凶（四个，互相掩护）

连环多虫是这次排查昂贵的根本原因：每个 bug 都会掩盖或伪造下一个 bug
的现场。

### 1. git 测试：裸 `.status()` + 继承 stdio → 孤儿持管（`aec16b2`）

`extension_host` 的 git 测试（clone/checkout/init/commit）用裸
`std::process::Command::status()` spawn git：**无超时，且 stdout/stderr
继承自测试进程**。git 子进程一旦不退出：

1. 测试线程永远阻塞在 `.status()` 上；
2. 测试框架/nextest/timeout 杀掉测试进程或 supervisor 后，**孤儿 git
   子进程仍握着继承来的 stdout 管道**；
3. 管道永不 EOF → 排水循环/runner/`tee` 全部干等 → 完全静默。

这一机制同时解释了"GNU timeout 为什么不生效"（它只杀直接子进程）和
"手动取消为什么日志全丢"。修复：全部改走
`sync_process::output_with_timeout`（管道隔离 + null stdin + 硬超时 +
超时即杀）。

**教训：凡是 spawn 子进程，要么管道隔离，要么保证子进程有界；永远不要在
可能被 kill 的进程里用继承 stdio 的 `.status()` 等一个没有超时的子进程。**

### 2. kill_process_tree 依赖外部 `kill` 二进制（`9637f88`）

`tack-tools::shell::kill_process_tree` 是 shell 出去调 `kill` **二进制**
（procps 包）实现的。精简环境（slim docker 镜像、Termux）没有 procps，
组杀和单杀**全部静默失败**——exec 超时后子进程照样跑满全程
（`ext_headless::exec_timeout_kills_the_child` 因此在容器里挂 60 秒，
且**从未在任何失败 run 的日志里出现过**，正是这点暴露了它）。

这不只是 CI 问题：所有在容器/精简系统里用 Tack 的用户，exec 超时语义
都是坏的。修复：改走 `nix` 的 `kill`/`killpg` 系统调用，不依赖外部
二进制。

**教训：排查 exec 类超时问题，先确认目标环境有没有 procps；判断"某测试
从未完成"，对完整日志 `grep -c <测试名>`。**

### 3. 构建缓存死循环（`9960e88`）

`Swatinem/rust-cache` **默认只在 job 成功时保存缓存**
（`CACHE_ON_FAILURE: false`）。check 连续红/被取消 → 缓存停留在远古
→ 每次运行都全量下载+编译 wasmtime 量级的依赖树（4 vCPU/16GB debug）
→ 更慢、更容易超时/被取消 → 缓存永远暖不起来。

**"挂点漂移"的统计学解释**：缓存热度逐次不同——缓存热时冲进测试阶段
（死于 bug 1/2），缓存冷时死于编译期资源耗尽。修复：
`cache-on-failure: true`（红 run 也暖缓存），并给冷构建留足时限
（job timeout 45→60 分钟）。

### 4. 巨型 debug 构建 vs 迷你 runner（`7490d99`）

本仓库 dev 测试二进制约 **765–816MB 一个**（debug=2 的 DWARF），
20+ 个测试二进制加中间产物，全量构建需要 GH ubuntu runner ~14GB 可用
SSD 的数倍。磁盘写满 → 写阻塞 → **VM 假死、runner 失联**——这就是
"lost communication" 且日志/artifact 全灭的直接原因。

修复：Free disk space 步（删镜像自带的 dotnet/android/ghc/powershell/
swift，回收 20–30GB）；Test 步 `CARGO_PROFILE_DEV_DEBUG=0`（不带
DWARF，二进制降到 ~100MB 量级；panic/assert 消息自带静态 file:line，
不影响排查）+ `CARGO_BUILD_JOBS=3`（压并行链接峰值内存）。

## 最终 CI 形态（为什么长这样）

见 `.github/workflows/ci.yml` 的 check job 与 `.config/nextest.toml`：

- **Docker 牢笼**：测试负载跑在 `ubuntu:24.04` 容器里，硬 cgroup 上限
  `--memory=12g --cpus=3.5 --pids-limit=2048`。资源炸弹只炸容器，
  runner agent 活着把失败流出来——**此后任何失败都带日志**。容器内
  apt 装 `git build-essential pkg-config`（镜像没有 git/C 工具链；
  git 必须 `--no-install-recommends`，否则 Recommends 拉来
  ca-certificates，其 postinst 会写只读挂载的 `/etc/ssl/certs` 而失败）。
- **cargo-nextest**：每测试独立进程 + `profile.ci` 的 slow-timeout
  （60s 报 SLOW，180s+grace 杀），卡死测试会被**点名**而不是无声悬挂。
  doctest 不在 nextest 覆盖内，单独 `cargo test --doc`。
- **日志双通道**：`tee test-output.log` + `actions/upload-artifact`
  （`if: always()`）。即使 step 失败，完整日志也必留下。
- **不要手动取消 job**：取消会丢弃该 job 全部日志（BlobNotFound）；
  `if: always()` 的 artifact 步只在"温和"路径（失败/手动取消）后执行，
  GitHub 超时强杀后不执行。

## 排查方法复盘（可复用）

有效的：

1. **先保证"失败必留日志"，再谈定位**。裸 runner 上一写一个没；
   容器化之后一次 run 一个可见失败，三轮即破案。
2. **实时流是唯一幸存者**：VM 死后，只有活着时流进 GitHub 实时视图的
   行还在（API 拉不到，但用户能看到/粘贴）。
3. **`if: always()` 的 artifact 步**：温和失败/手动取消后仍会执行，
   是稳定取证通道。
4. **"某测试从未完成"判定法**：对完整日志 `grep -c <测试名>`；0 次即
   凶嫌。exec_timeout 就是这样落网的。
5. **区分"测试卡死"与"runner 死亡"**：前者 timeout 机制能留下尸体
   （SLOW/FAIL 行），后者连 watchdog 的 `sleep` 都调度不动——
   watchdog 到点不响，就是 VM 死了。
6. **二分排除要匹配对维度**：nextest 的 `test(wasm)` 只匹配测试名，
   排除整个 crate 的 wasmtime 测试需要
   `not (test(wasm) or package(tack-ext-wasm))`。

无效的（别再试）：

- 在 job 内加任何"超时/watchdog"对抗 runner 死亡——它和凶手死在同一
  台 VM 里。
- 本机 stress 循环复现——架构（aarch64/x86_64）、核数、proot 分支、
  负载都不同，并发/资源类 bug 是概率事件，"本机 30 遍全过"没有证明力。
- 猜机制打补丁——前四次修复（current-thread 饥饿、RSA keygen、
  reqwest 超时、100-continue）全是未实证的猜测，全都"修了没好"。
