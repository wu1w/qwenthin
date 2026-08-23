# q-harness 源码审计 — 2026-08-21 轮（只读，未改任何源码）

审计范围：本轮改动文件（8/21 09:14–09:49）

- `q38-loop`: `lib.rs`、`config.rs`、`agent/mod.rs`、`mcp/mod.rs`、`tools/{mod,fs,fold,path,run_code}.rs`、`tools/q38_sdk.py`、`tool_calls/{coordinator,types,timeout,mod}.rs`、`paw_loop/{store,handler}.rs`、`paw_loop/gates/{mod,doom,lossy}.rs`、`channel/mailbox.rs`
- `q38-web`: `cron.rs`、`hub.rs`；`q38-cli`: `main.rs`

验证：`cargo check --workspace --all-targets` 干净；`cargo test --workspace` 441 passed / 0 failed（live 测试默认 ignored）。

## A. 正确性

### A1（中）`run_code` / `bash` 输出超 1 MiB 会挂满超时并丢输出
`tools/run_code.rs::read_capped`（从 `bash.rs` 继承）到达 `OUTPUT_MAX_BYTES` 后**停止读管道**。子进程继续写 → 管道缓冲（~64 KiB）满后阻塞 → `child.wait()` 永不返回 → 只能等 coordinator kill deadline（默认 `bash_timeout_secs=60s`）杀掉，结果变成 `timeout`，且已捕获的前 1 MiB 一并丢弃。
示例：`print("a" * 2_000_000)` → 60 秒后返回 "timeout"，而不是 capped 输出。
修法：到 cap 后继续 drain 丢弃直到 EOF，只保留前 1 MiB。`bash.rs` 是旧有同款问题，本轮 `run_code.rs` 把它复制到了新工具。

### A2（低-中）`config.rs::save_to` 失败路径可摧毁现有配置
```rust
if let Err(e) = fs::rename(&tmp, path) {
    let _ = fs::remove_file(path);          // 先删掉用户现有 config
    fs::rename(&tmp, path).map_err(|_| e)?; // 再失败 → 旧配置没了，只剩 .toml.tmp
}
```
第一次 rename 失败 + 第二次 rename 也失败 ⇒ 原 `config.toml` 被删、新内容未落盘。另外任何失败路径都不清理 `config.toml.tmp`。建议：重试前不删目标，失败时删 tmp。

### A3（低）`fs.rs::read_file` 空文件报错
0 行文件：`start(1) > total(0)` → 返回 `Error: start_line 1 exceeds file length (0 lines)`。空文件应返回空内容（或 "empty file" 提示）。

### A4（低）`read_file` 的 `end_line < offset` 静默回退默认 limit
显式传 `end_line` 但小于 `offset` 时，不是报错而是退回 `limit` 默认 2000 行，可能意外返回大段内容。

### A5（低）`edit_file` 空 `old_string` 边界
文件为空且 `old_string=""` 时 `matches("")==1`，整文件被替换为 `new_string`。属边角，但空串 old 直接拒绝更稳。

### A6（低）`run_code` 与 `bash` 退出码语义不一致
- `bash`：非零退出 → `ToolState::Success`（仅文本里写 "Command failed with exit code N"）。
- `run_code`：非零退出 → `ToolState::Error`。
下游按 `ToolState::Error` 触发的逻辑（如 "failed tool 后注入 skill testhook"）只对 `run_code` 失败生效，`bash` 失败不触发。两者应统一。

## B. 安全

### B1（中）MCP 子进程继承完整进程环境 + workspace 级 mcp.toml 是半不可信输入
`mcp/mod.rs::spawn` 只做 `cmd.env(k, v)` 追加，不清环境。MCP server 进程因此能读到 `Q38_API_KEY` 等全部宿主环境变量。而 `.q38/mcp.toml` 可在 workspace 层挂载——workspace 是外部仓库/模型可写区时，恶意 `mcp.toml` 即可起进程并外带 token。
MCP 本质允许跑任意命令，但建议：`env_clear()` + 白名单（PATH/HOME 等）或每 server 显式 `inherit_env=true` 开关，把"全环境透传"变成 opt-in。

### B2（低，固有）confinement 存在 check-then-open TOCTOU
`path.rs::check_confined` canonicalize 判定与实际 open 之间有窗口，期间 symlink 可被替换。属该类设计固有取舍；如需强保证需 O_NOFOLLOW/openat 级别手段。本轮实现的 lexical+canonicalize+tail 重拼（含目录 symlink 穿透）本身是正确的，测试覆盖了文件/目录 symlink 逃逸。

### B3（低）`q38_sdk.py::bash()` 无超时且孙进程可孤儿化
`subprocess.run(command, shell=True)` 无 `timeout`；外层 kill python 不会连带 kill shell 孙进程，挂死的命令会留下孤儿继续跑。建议加 `timeout=`（可用 `_Q38_ROOT` 旁的配置或常量）并在超时 kill 进程组。

## C. 一致性 / 死代码 / 性能

### C1（低）coordinator offload 分支语义名不副实（当前死代码）
`drive()` 的 offload 分支返回 `offloaded=true, "running in background", state=Success`，但实际 `join.abort()`——`kill_on_drop` 工具会被直接杀掉。"后台运行"实为"杀掉并谎称在跑"。目前 `set_offload_on_deadline(true)` 无任何调用方（仅测试），是死代码；将来若启用会向模型注入错误状态。启用前需要真正的后台注册表，或改文案为"已放弃等待/已终止"。

### C2（低）`fs_tool_path` 不认 QwenPaw 别名 `file_path`
`lossy.rs::fs_tool_path` 只读 `args["path"]`，而 `fs.rs::arg_path` 同时接受 `file_path`。模型若用 `file_path` 别名反复读写同一文件，`PathLoopGate` 对其完全失明。建议 `fs_tool_path` 复用同一 alias 逻辑。

### C3（信息）`cleanup_stale_tmp` 每次写/put 全量扫目录
`fs.rs` 与 `fold.rs` 都在每次成功写后 `read_dir` 整个目录（workspace 目录 / blob 目录），按 pid 前缀删外部残留：
- blob 目录随使用增长，O(n) 扫描每次 put 都发生；
- 两个进程并发写同一目标文件时，可能删掉对方 in-flight 的 tmp，使对方 rename 失败（返回 error，不损坏数据，但是 race）。
建议加 mtime 阈值（只清 N 分钟前的）或降低频率。

### C4（信息）`run_code` 的 `.q38-sdk/` 目录可能残留
脚本文件本身会删；但若模型代码往 `.q38-sdk/` 里写了东西，`remove_dir` 失败、目录留着。无害，注意即可。

## D. 验证通过项（本轮改动质量好的部分）

- **路径 confinement**：lexical normalize + 存在段 canonicalize + tail 重拼；`is_within` 按组件比较（防 `/workspace-evil` 前缀混淆）；symlink 文件/目录逃逸、`hole/new.txt` 缺失路径场景测试齐全且通过。
- **BlobStore**：sha256-hex 校验堵 path traversal（`get("../passwd")` 测试在）；tmp 名含 pid+nanos+uuid 防并发碰撞；unix 下 0600/0700；rename 原子落盘。
- **write_atomic**：fsync+rename、TmpGuard 兜底删 tmp、foreign-pid 残留清理有测试。
- **fold_text**：char 边界 head/tail、blob 落盘回链、超 cap 提示 paging，测试覆盖。
- **coordinator**：poison lock 恢复（live/hooks/per_agent 全走 `lock_unpoison`，有测试）；`CancelFlag` 用 `send_replace` 修复了"无订阅者时 cancel 丢失"（有专门测试）；offload/kill 双 deadline 拉回逻辑自洽。
- **gates**：doom gate 修复了并行 `read a,read b,read c` 只记 last_tool 的误报（现在记录整 hop 全部 fingerprint，回归测试在）；`reset_turn`（每 user 轮）与 `reset_repeat`（compact 后只清 stutter 类）语义区分正确；NameStreak/PathLoop 的 same-iter 二次 check 保护正确。
- **mcp JSON-RPC**：Content-Length 大小写不敏感、notification skip 有上限 64、单帧 8 MiB cap、stderr 独立 drain 防死锁（有 flood 回归测试）、超时错误带 stderr 尾部。
- **web cron**：失败 turn 回滚 `last_run`（`restore_last_run`/`restore_heartbeat`）避免 job 跳过整个 interval，测试齐全；`overlay_cron_jobs` 不丢表单未列出的 job；`save_with_workspace` 与 `.q38/cron.json` ingest 往返一致（last_run 取 max 不回拨）。
- **hub**：permit FIFO（只在队空时广播新 ask）、`redact_key` 修掉非 ASCII 字节切片 panic（有测试）、cron tick 在 live/turn_in_flight 时跳过不 mark。
- **config**：env overlay 不序列化、`mutate_disk` 从盘重读（回归测试防旧 bug）、working_window 边界校验。

## E. 实测复核（8/21 晚，agent 在自己运行的同一份代码上亲测）

用 `bash`/`read`/`edit` 实测了 A 组与 B3。结论有修正：

### A1 — 确认存在，但形态修正（中，真实阻碍）
- `bash` 输出 2MB（`python3 -c "print('a'*2_000_000)"`）：**没有挂满 60s**。
  实际链：`read_capped` 读到 1MiB 上限 → 读任务结束、读端关闭 → 子进程下一次 write 收到
  `BrokenPipeError` → **以 exit 1 返回**。我拿到的是 "Command failed with exit code 1" +
  半截输出 + 误导性 traceback——**成功命令被误报为失败**（会触发 harness-fail 的 effort
  升级 / skill testhook 注入等下游逻辑）。前 1MiB 其实完整进了 blob，可用 `recall` 取回。
- `run_code` 同款（本会话未暴露 run_code，用等价 python 验证了 bash 侧两次）。
- 真正会挂死 60s 并全量丢数据的分支也确认存在：子进程写出 >1MiB+管道容量后**停止写**
  （如写文件再 sleep）→ 读端不会关 → `child.wait()` 挂到 kill deadline。
- 绕行（我在实测中用的）：`| head -c N`（丢真实退出码）、输出重定向到文件再分页 read。
- 修法不变：到 cap 后继续 drain 到 EOF；并让 EPIPE 导致的非零退出与真实退出码可区分。

### A3 — **不成立，撤销**
空文件 read 正常：返回空首行 + 空内容 sha（`e3b0c44298fc`），无报错。

### A4 — 降级（低-信息）
`end_line < offset` 静默回退默认 limit 在代码中可达，但**冻结 read schema 只暴露
path/offset/limit**，模型无法发出 `end_line`，实际不会踩。仅 QwenPaw 别名路径可达。

### A5 — 确认可达（低）
对空文件 `edit(old_string="")` 成功把整文件替换为 new_string。

### B3 — **完全确认，且有真实代价**（低-中）
`subprocess.run('sleep 300', shell=True)`（sdk `bash()` 的等价形态）：
- 挂满 60s 后返回 `timeout`（浪费整次工具预算）；
- harness 只 kill python：`pgrep` 证实 **`sleep 300` 孤儿进程（PID 61146）继续存活**，
  需手动清理。建议 `start_new_session=True` + 超时 kill 进程组。

### A6 — 模型侧不可观测（维持记录）
ToolState（Success/Error）不进我看到的文本，两工具退出码语义不一致我无法直接感知；
影响仅限 harness 内部按 state 触发的逻辑，维持低优。

### 其他实测正常项
- read 的 limit 封顶提示（`[limit capped … pass offset to page]`）正常触发；
- fold blob 回链正常：超 10k 字符的 live 文本带 `blob=sha`，可 `recall`；
- `.q38-sdk/` 无残留（C4 未触发）。

## 建议优先级

~~1. A1（read_capped drain 到 EOF）~~ ✅ 已修复（F 节实测）；
~~2. B1（MCP 环境透传改 opt-in）~~ ✅ 已修复；
~~3. B3（sdk bash 进程组 kill + 超时）~~ ✅ 已修复（F 节实测）；
~~4. A2（save_to 失败不删旧文件）~~ ✅ 已修复；
~~5. A6（bash/run_code 退出码语义统一）~~ ✅ 已修复；
6. 其余为低优/记录在案（A3 已撤销，A4 降级，C1/C3 未动）。

## F. 修复复核（8/21 二轮，agent 复测后确认）

本轮修复后复核。`cargo check --workspace --all-targets` 干净；`cargo test --workspace` **447 passed / 0 failed**（新增 6 个回归：read_capped drain×2、save_to 失败不删旧、sdk 超时 kill、edit 空 old_string、`file_path` 别名 gate、MCP env 白名单）。

| 项 | 结论 | 依据 |
|---|---|---|
| A1 | **已修复（实测）** | `read_capped`（bash.rs + run_code.rs 两份）到 cap 后继续 drain 到 EOF。实测：2MB 输出**立即**返回、capped 1MiB + blob 回链、无 EPIPE→exit 1 误报；"写 2.5MB 后 sleep 10s" 场景 **10s 返回**而非 60s kill deadline（`time` 实测 10.04s，exit 0）。 |
| A2 | **已修复（代码）** | `save_to` 改为 `replace_tmp`：任何失败路径不再先 unlink 目标，只清 tmp；Windows `AlreadyExists` 走专门替换分支。 |
| A5 | **已修复** | `edit_file` 空 `old_string` 直接拒绝（"must be non-empty"），有回归测试。 |
| A6 | **已修复（代码）** | `bash` 非零退出现在也返回 `ToolState::Error`，与 `run_code` 统一。 |
| B1 | **已修复** | MCP `inherit_env` 默认 false：`env_clear()` + PATH/HOME 白名单；测试断言默认 false 且 host API key 不透传。 |
| B3 | **已修复（实测）** | sdk `bash()` 改 `Popen` + `start_new_session=True` + `communicate(timeout=Q38_BASH_TIMEOUT)`，超时 `os.killpg(SIGKILL)`。实测 `Q38_BASH_TIMEOUT=2` + `sleep 300`：**2.0s 抛 RuntimeError**，`pgrep` 确认 **无孤儿进程**。 |
| C2 | **已修复** | `lossy.rs::fs_tool_path` 现在同时接受 `file_path` 别名，有回归测试（含 path/file_path 并存时 path 优先）。 |
| C1 | **已修复（F2 节）** | offload 分支重写为真后台：watcher 任务持有 `JoinHandle`（不再 drop-kill），kill deadline 后台续跑，结果进 `finished` 注册表由 agent 循环 `drain_background()` 以 hidden note 投递；offload 条目留在 `live` 中故 `cancel()` 仍可达；文案改为"result will be posted as a follow-up note"，与实现一致。4 个回归测试。 |
| C3 | **已修复（F2 节）** | 两份 `cleanup_stale_tmp` 合并到 `tools/mod.rs`，加 `STALE_TMP_MAX_AGE=300s` mtime 阈值：只删 >5 分钟的外部 pid tmp，并发写者的 in-flight tmp 不再被误删。回归测试含"新鲜 foreign tmp 必须存活"。 |

**结论：A/B/C 组全部关闭。**

### F2. C1/C3 复核（8/21 三轮）

- `cargo check` 干净；`cargo test --workspace` **450 passed / 0 failed**（再增 3 个：recent-foreign-tmp 保留、offload 注册表相关）。
- **C1 代码走查**：`drive()` offload 分支 → `spawn_watch` 独立任务持有 `join`（子进程 `kill_on_drop` 不再被触发）；watcher 内 select 仍尊重 cancel 与 kill_at，完成后 `finished.push`，并从 `live` 移除。消费端：agent 主循环每步开头 + `finish()` 都 `drain_background()`，aborted 时 `cancel_background()` 兜底。offload 需 `has_kill_budget`（剩余 kill 预算 ≥ `MIN_BACKGROUND_WINDOW_SECS=30s`），否则正常走 kill，不会"假后台"。测试 `offload_keeps_task_running_and_delivers` / `offload_then_cancel_posts_interrupted` / `offload_then_kill_posts_timeout` 全过。
- **C3 实测注意**：我（审计 agent）正运行在 **11:15 的旧二进制**上（C1/C3 改动落盘于 15:26–15:27），所以用 `write` 工具实测清理会打到旧逻辑（旧版无阈值，新鲜 foreign tmp 也被删——这是旧行为，非新代码 bug）。新语义由单测钉死：`write_atomic_cleans_old_foreign_pid_leftover`（400s 旧 external → 删）、`write_atomic_keeps_recent_foreign_tmp`（新鲜 external → **保留**，即修复的并发 race）、`cleanup_stale_keeps_same_process_tmp`（同进程 tmp 永不删）。
- 建议：重启 harness（重新 build）后，新二进制的 C1/C3 行为才对我生效。
