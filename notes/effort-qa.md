# q38 思考等级（think effort）自动升级 QA

核心：`crates/q38-loop/src/policy.rs` 的 `EffortController`，被 `crates/q38-loop/src/agent/mod.rs` 的 agent 主循环驱动。

## 1. 什么时候自动升级到 medium？
两条路径，都走私有 `upgrade_medium()`（policy.rs:351）：
- **解析失败**：每步若 `turn.parse_fail`（agent/mod.rs:602），主循环调 `note_parse_fail()`。默认 `parse_upgrade_after=2`，即**第 2 次**解析失败才升级（首次失败保持原级）；低精度/lossy 会话在 agent/mod.rs:426 用 `.with_parse_upgrade_after(1)`，**第 1 次**就升级。
- **harness 失败**：工具执行后若 `is_harness_fail()`（agent/mod.rs:1650，即 response 为 `ToolState::Interrupted` 且文本是 "tool task aborted"）为真，调 `note_harness_fail()`；**连续 2 次**即升级。工具级错误（bash 非零、unknown tool 等）不算。

`upgrade_medium()` 的目标：`effort=Medium, max_think_tokens=2048`，`max_tokens` 抬到 ≥4096（原 ≥8192 则保留），`preserve` 不变；永远**不会**升到 xhigh。

## 2. 升级后什么时候降回原水平？
`note_clean_step()`（policy.rs:337）：清零 parse/harness 连续计数；若处于 `auto_upgraded` 且未锁定，把 policy 恢复为 `baseline`（`EffortController::new()` 时保存的会话初值）并置 `auto_upgraded=false`。agent 侧通过 `mark_clean()`（agent/mod.rs:1200，调用点在 658/667/678/910/1245）在**任意一个干净步**（无 harness 失败的工具执行，或非失败路径）触发，并经 `sync_effort(PolicyReason::Downgrade)` 同步到 completer。即：升一级、一次 clean 步降一级（policy.rs 注释："one prefix miss up, one miss down"）。干净步也能打断 harness 连击（见测试 `harness_streak_resets_on_clean`）。

## 3. 用户 `--think low` 锁定后还会自动升级吗？
**不会**。CLI 在 `crates/q38-cli/src/main.rs:238` 计算 `effort_locked = cli.fast || cli.think.is_some() || mode∈{Think,Chat}`，即 `--think low` 置 `effort_locked=true` → 传给 `EffortController::new(policy, user_locked=true)`。`upgrade_medium()` 开头 `if self.user_locked { return; }`（policy.rs:352）直接跳过；且 `note_clean_step()` 对锁定会话不做降级（policy.rs:340），保证低精度 cap 等锁定状态不被覆盖。测试 `user_lock_blocks_upgrade_and_downgrade`（policy.rs:595）验证了这一点。副作用：lossy 时 `apply_lossy_think_cap(locked)` 也跳过（policy.rs:193），low 保持 512 而非 384。
