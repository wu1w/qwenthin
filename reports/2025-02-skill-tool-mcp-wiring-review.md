# skill / tool / mcp 与核心 loop 接线审查

日期：2025-02（q38-loop @ HEAD，374 个 lib 测试全绿后做的审查）

## 结论先行

整体做得相当好。设计纪律（frozen tools[] + 单 mcp 包装 + skill 走 hidden card）是自洽的，
四条入口（sidecar / CLI / web / agent 直连）的接线都通，`cargo test -p q38-loop --lib`
374 passed / 0 failed。**没有发现会导致运行时错误的断线**；发现 1 个中等可维护性问题
（surface 双实现已漂移）、1 个中等隐患（双 dispatch 工具表靠单行谓词保平安）、
2 个低级别启发式/注释问题。

## 做得好的部分

1. **frozen tools[] 纪律守住了**。`tools_schema.rs` 里 read/write/edit/bash 是字节级冻结的
   JSON 常量，`agent_tools()` 测试断言序列化字节不变。`view` / `mcp` / `memory_search`
   按条件追加在冻结前缀之后，`skill` 永远不进 `tools[]`。web 控制台 `tools_get` 的 note
   也明确写了这条契约。
2. **mcp 收口成单个工具是对的**。`mcp(server, method, args)` 一个入口，绝不把
   `tools/list` 展开进 OpenAI `tools[]` —— 注释直接点明这是防止 "QwenPaw zoo" 毁掉
   27B 的 tool choice 和 Jinja tools hash。stdio JSON-RPC 实现扎实：
   - Content-Length 大小写不敏感（`content_length_of` 有测试）
   - 跳过无 `id` 的 notification（上限 64，防死循环）
   - 单帧 8MB 上限、整调用 `tokio::time::timeout`、`kill_on_drop`
   - 每次调用 spawn 新进程，无跨调用状态泄漏
3. **skill 渐进披露链路完整**：catalog（name + 40 字 trigger）只在 `skills_auto_catalog`
   开时进 system prompt（默认关）；body 走 hidden user card，400-token fail-closed
   （超了不注入并留 note）；模型幻觉出的 `skill` 调用在 `dispatch_one` 有兜底
   （测试 `emitted_skill_call_runs_without_tools_entry` 锁住了）。
4. **overlay 语义 skill 和 mcp 一致**：home → workspace，后者赢，都有测试
   （`workspace_skill_overlays_home` / `workspace_mcp_toml_overlays_home`）。
   mcp 的 console 回写不会清掉 `config.toml` 里的 env token
   （`merge_mcp_servers` / `upsert_mcp_server` 有 round-trip 测试）。
5. **接线闭环**（逐一核对过）：
   - `run_message` → `split_skill_prefix` / `split_mcp_prefix` → `inject_notes`
     （memory → skill → mcp → plan → cron，每类都有 `live_has_*` 去重 +
     `stub_expired_notes` 过期回收）→ `drive`
   - `dispatch_one` 覆盖 recall / memory_search / mcp / skill / view / 兜底 `run_tool`
   - 工具失败后 `inject_skill_from_tools`（test FAILED → testhook skill）接在
     `execute_tools` 尾部，路径存在
   - slash 侧 `InvokeSkill` / `InvokeMcp` 经 `skill_turn_prompt` / `mcp_turn_prompt`
     回到同一个前缀解析，channel inbound 也走同一入口（`channels.rs`）

## 发现的问题

### P1（中）surface 双实现，且已经漂移

同一份"Agent/Think 模式的 system + tools"在两处独立构建：

| | `sidecar/helpers.rs::sidecar_agent_surface`（session start，记 tools_hash） | `agent/mod.rs::bind_periphery`（Agent 实际用的 tools） |
|---|---|---|
| view 工具 | **无条件** push | 仅 `opts.media`（`cfg.media.enabled`）且 Agent |
| memory_search | `home.is_dir()` | `MemoryStore::open(home).ok()`（open 还会 `ensure_layout` 建目录） |
| config 来源 | 现场 `Config::load_or_init()`（**有落盘副作用**） | `execute_turn` 传入的 cfg 快照 |

现状为什么没炸：`tools_hash` 只写不读（全仓 grep 确认只有 SessionStart 存储/展示/
fork 透传，没有拿它和实际 tools 比对），`media.enabled` 默认 true，所以默认配置下
两边恰好一致。但：
- 用户关掉 media 后，session 日志里记的 hash 含 view、实际请求不含；
- 只读 home 上 `ensure_layout` 失败时 memory_search 两边不一致；
- `load_or_init` 在每次 session start / fork / mode 切换时可能改写磁盘 config，
  与本轮 cfg 快照分叉。
- 另外 system prompt 也是两处构建：开已有 session 时 `open(&session_id)` 用 log 里
  （sidecar 版）的 messages，agent 自己构建的 system 被丢弃。会话中途改 config
  （开 mcp_auto_catalog、加 mcp server）就会 log system 与实际 tools 分叉，
  要到下个 session 才自愈。

建议：把 surface 构建收敛成单一函数（`bind_periphery` 逻辑上移），`make_start`
复用它；`sidecar_agent_surface` 里别再现场 `load_or_init`，用传入的 cfg。

### P2（中）双 dispatch 各持一张工具表，靠单行谓词保平安

- `dispatch_one`（agent/mod.rs:906）知道 recall / memory_search / mcp / skill / view；
- `dispatch_parallel`（:949）只特判 view，其余一律 `run_tool`（它只认识冻结 5 件套 + view）。

今天不出事是因为 `parallel_safe_batch` 只放行 `read | view`（:1402）。这是隐性契约：
将来有人把谓词放宽（比如"memory_search 也并行"或"只读 bash 并行"），相关调用会
静默落到 `run_tool` 的 `unknown tool` 错误，且没有测试能提前拦住。

建议：`dispatch_parallel` 的每个任务直接复用 `dispatch_one`（或抽出
name→handler 共享表），让工具知识只存在一处。

### P3（低）mcp / skill / recall / memory_search 绕过 coordinator

`dispatch_one` 里这四类不走 `self.coordinator.execute`（无统一 timeout/cancel 包装）。
mcp 自带 `tokio::time::timeout`（OK），skill/recall/memory_search 都是本地 IO（OK）。
记录在案即可，别哪天给 coordinator 加全局预算时忘了这四条旁路。

### P4（低）注释与启发式的两处不精确

1. `mcp::card_for` 注释说 "Bare server names are not enough (`docs/foo.md` must not
   mount `docs`)"，但实际闸门是 **`mcp` 这个 token**（`mentions_mcp`）。
   "use mcp to write docs/foo.md" 会经 `named_in_text` 命中名为 docs 的 server
   （`docs/` 拆词出 `docs`）。注释把保护说强了。
2. skill 的 `match_user` 没有关键词闸门：skill 名是常见词（`test`、`docs`、`pdf`）
   时，任何消息含该词就注入 card。400-token 预算限了伤害，但噪音是真实的。
   建议要么给 skill 也加一个类似 `mentions_mcp` 的软闸门（`[skill:` 前缀或 "skill"
   词），要么在文档里明说这是已知噪音源。

### P5（info）hydration 路径无 hidden card，无测试锁定

`run.rs` 里 `load_messages + drive()` 分支（`!persist` 且带历史 messages）不经过
`inject_notes`，该路径零注入。从设计看是有意的（只 live query 注入），但没有任何
测试把这个行为锁住，重构时容易悄悄改变。建议加一个 drive-only 路径的断言。

## 建议的改动优先级

1. 收敛 surface 双实现（P1）—— 一次改动同时消掉 hash 漂移和 config 分叉；
2. `dispatch_parallel` 复用 `dispatch_one`（P2）—— 几行改动，消掉未来地雷；
3. 修 `card_for` 注释 / 给 skill 匹配加软闸门（P4）；
4. 补 hydration 路径行为测试（P5）。
