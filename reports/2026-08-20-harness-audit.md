# q-harness 代码审查（2026-08-20，agent 自审）

范围：全 workspace 源码（~45k 行，5 crate）+ 本 agent 实际可用工具链的活体验证。
结论：**编译 / clippy（仅 warning）/ 402 个测试全部通过**。发现 2 个真实崩溃级 bug（已修复+回归测试），
另有若干健壮性问题与配置/环境项，按严重度排列如下。

## 一、已修复（本次落盘）

### 1. `slash.rs::history_text` — CJK 历史必然 panic（高危，已修）
`/history`（web 控制台、TUI、sidecar 三条活路径都走这里）超过 8000 字符时做
`&body[..half]` / `&body[body.len()-half..]` **字节**切片。中文每字 3 字节，
切点落在字符中间的概率约 2/3 → `byte index is not a char boundary` panic。
对中文用户，`/history` 基本必炸。
- 修复：改为 char-safe 的 head/tail 截取（`chars().take/skip`）。
- 回归测试：`slash::tests::history_text_cjk_does_not_panic_on_clip`（40 行 CJK 强制触发 clip）。

### 2. `q38-web/hub.rs::redact_key` — 非 ASCII key panic（低概率，已修）
`&t[t.len()-4..]` 字节切片，非 ASCII 的 API key 会 panic（web 路由同步路径）。
- 修复：char-safe 取尾 4 字符。回归测试 `redact_ascii_and_unicode`。

## 二、发现但未改（建议排期）

### 3. git 仓库 0 个 commit
`main` 分支没有任何提交，全部文件 untracked。目前没有任何历史/回滚点。
建议：`git add -A && git commit`（先确认 .gitignore 覆盖 target/、node_modules/、.q38 运行时数据）。

### 4. MCP 客户端 stderr 只 pipe 不读（mcp/mod.rs `spawn`）
`stderr(Stdio::piped())` 后从不读取：子进程 stderr 写满 ~64KB 管道缓冲即阻塞 →
永远不响应 → 30s 超时。且失败时报 `Error: MCP timeout.`，**零诊断信息**。
建议：drain stderr 到带上限的 buffer，超时/出错时附最后几百字节；或至少 `Stdio::null()`。

### 5. tavily MCP 冷启动超时（环境+配置，实测复现）
`~/.q38-agent/mcp.toml` 用 `npx -y tavily-mcp@latest`，而 `~/.npm/_npx` 缓存不存在：
每次调用都现场下载包，超过 30s 超时（本次用 python 直接探针，60s 都没跑完握手）。
即本 agent 的「网页搜索」当前实际不可用。
- 快速修复：`npm i -g tavily-mcp`，mcp.toml 改 `command = "tavily-mcp"`（秒级启动）；
  或手动预热一次 npx 缓存。
- 顺带核对：配置里 `methods = ["tavily-search", ...]` 用连字符，npm 包实际工具名多为
  下划线（`tavily_search`）。建议跑一次 `mcp(server, "tools/list")` 核对并改对，
  否则目录卡片的 method 提示会误导模型。

### 6. 工作区 confinement 可被 symlink 绕过（tools/path.rs `resolve`）
`resolve()` 只做词法归一化（`lexical_normalize`），最终路径不 `canonicalize`。
工作区内建一个指向外部的 symlink，`read/write/edit` 即可读写工作区外文件。
缓解因素：`bash`/`run_code` 本身不受 confinement 限制（等于整机的软护栏），
所以不是硬隔离漏洞；但若 `confined` 旗标是安全承诺，应在 resolve 末尾
`canonicalize` 后重新 `is_within` 检查（或至少对已存在路径做 `symlink_metadata` 检查）。

### 7. `edit` 全量替换且不校验唯一性（tools/fs.rs）
`content.replace(&old, &new)` 会替换**所有**出现处。多数 agent harness 的语义是
"替换第一处；old_string 不唯一时报错让模型加上下文"。当前行为可能悄悄改掉多处。
建议：`match old 出现次数` → 0 报错（已有）、>1 报错要求更精确、==1 替换。

### 8. Coordinator 的 per-tool CancelFlag 是死代码（agent/mod.rs `dispatch_one`）
`coordinator.execute(..., move |_c| async move { run_tool(.., self.cancel.clone(), ..) })`
丢弃了 coordinator 传入的 `_c`，工具内部只监听 agent 级 cancel。
所以 `ToolCoordinator::cancel(id)` 的按工具取消能力实际不存在，
超时时靠 `join.abort()` + `kill_on_drop` 兜底（能杀，但语义不一致）。
建议：把 `_c` 与 agent cancel 合并（任一触发即取消）或删掉 coordinator.cancel 的对外语义。

### 9. Web cron 先 mark 后执行（q38-web/hub.rs tick loop）
`g.cron.mark(&id, now)` 在 `start_turn` 之前就把 `last_run` 推走：
turn 失败/被取消也不会重试，要等下个 interval。可接受但应知晓。
另外 `last_run: None` 的新任务会在下一个 1s tick 立即触发（agent 刚写完 cron.json
就会马上跑一次）——如果这不是"立即跑一次"的产品意图，upsert 时应把 `last_run` 置 now。

### 10. 其它小项
- `write_atomic` 的临时文件名固定 `{name}.q38tmp`：同文件并发写会互踩；崩溃会留垃圾文件。
  建议 uuid 后缀 + 启动时清理残留。
- 非测试代码 85 处 `Mutex::lock().expect(...)`：任务持锁 panic 后锁中毒，后续所有调用方
  连环 panic。`memory/index.rs` 已做 "poisoned" 降级，coordinator 等没做，风格不一致。
- clippy 8 个 "too many arguments (8/7)"、12 个 "field assignment outside initializer"，纯风格。
- `reports/github_trending_*.py` 脚本：GitHub Search API 拿不到"昨日新增星数"，脚本用
  推送活跃度近似（报告里已如实注明）；`/usr/bin/python3` 有 urllib3/LibreSSL 的
  NotOpenSSLWarning 噪音，不影响功能。

## 三、工具链活体验证（本 agent 自身）

| 工具 | 结果 |
|---|---|
| read | ✅ 行号/sha256 尾部/continue 提示正常；offset/limit 语义核对过 |
| write / edit | ✅ 原子写；media 扩展名拒读转 view 正常 |
| bash | ✅ 工作区 cwd、输出截断 1MB、kill_on_drop；超时由 coordinator 管 |
| view | ✅ 工作区内 PNG 解码显示正常；工作区外绝对路径被正确拒绝（confinement 生效实证） |
| memory_search | ✅ 正常返回（当前 digest/日记无匹配；MEMORY.md 按设计不入 FTS 索引） |
| mcp (tavily) | ❌ 超时 ×2 → 定位为 #5 冷 npx 缓存环境问题，非协议 bug（协议层有 fake server 单测覆盖） |
| 定时任务 | ✅ `.q38/cron.json` 存在（GitHub 日报 job）；web 侧 store/ingest/merge 逻辑审过 |
| q38 二进制 | ✅ `--help`、probe/web/dsh-install 子命令齐全；web/console/dist 已构建 |
| cargo build / clippy / test | ✅ / 仅 warning / 402 passed（含新增 2 个回归测试） |

## 四、修复 diff 摘要
- `crates/q38-loop/src/slash.rs`：`history_text` 改 char-safe clip + 回归测试
- `crates/q38-web/src/hub.rs`：`redact_key` 改 char-safe 尾取 + 测试模块
