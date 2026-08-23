# q-harness × Qwen3.8-27B 适配度评估

## 架构总览

这套代码是为 **Qwen3.8-27B 本地部署**极致打磨的 Agent Harness，Rust 写，核心 `q38-loop` crate。设计哲学：**"少即是多"**——极短 prompt、frozen 4 工具、字节锁定模板、多层兜底而不是给模型讲道理。

## 适配度评分

| 维度 | 分数 | 说明 |
|------|------|------|
| 工具调用格式 | 10/10 | XML function / JSON / lenient 三级解析，完全匹配 |
| Prompt 策略 | 10/10 | 10 字 system prompt，27B 不需要长 essay |
| 上下文窗口 | 10/10 | 262144 tokens 精确匹配 |
| Think 调控 | 8/10 | low/medium/xhigh 精细调控，xhigh 4096 可能偏紧 |
| 安全防护 | 10/10 | 6 道 gate 分层合理 |
| 容错兜底 | 10/10 | echo strip / parse retry / watchdog oneshot |
| 前缀缓存 | 10/10 | SHA256 锁定 Jinja + tokenizer |
| 扩展性 | 5/10 | 硬绑 Qwen 家族 |

**总分：92%**

## 核心亮点

1. **极短 prompt 策略** — DEFAULT_AGENT_MD 只有 10 个中文字。注释明确说 "One line of voice markers beats a 4-8 line essay on this 27B"
2. **Frozen 4 tools** — read/write/edit/bash 字节锁定，不 splice，保护前缀缓存
3. **Echo strip** — 27B 收到"你好"会复读，harness 静默去掉
4. **Watchdog oneshot** — think cap 命中后关 thinking 重试，不 lecture 模型
5. **Doom loop halt** — 3 次重复直接停，不注入"请换方法"
6. **hidden_user 机制** — skill/MCP/MEMORY 卡片用
2. **Frozen 4 tools 设计** — `read, write, edit, bash` 字节锁定，注释说 *"frozen four, never splice"*。27B 的 attention 在 tools 多时容易混淆，这个限制是对的。`recall`, `memory_search`, `mcp`, `view` 是追加的，不破坏前缀缓存。

3. **Echo strip** — `echo.rs` 专门处理 27B 收到"你好"会**复读用户原话**的毛病。harness 层静默去掉，不 lecture 模型。我刚才回"在的！"就是这类行为，不过还没触发 strip 条件。

4. **Watchdog oneshot** — think token 撞到 cap 时，harness 关掉 thinking 用同一批 messages 重试一次，不注入任何"你思考太多了"的提示。这个设计非常对——27B 被 lecture 了反而会更 confused。

5. **Doom loop halt** — 连续 3 次相同 tool call 直接 halt，**不**往模型里注入"请换一种方法"。注释说得很清楚：*"halt after 3 repeats: harness stop, not a hidden user warning"*。

6. **hidden_user 机制** — skill/MCP/MEMORY 卡片用 `

<tool_call>
<function=write>
<parameter=path>
/Users/william/q-harness/notes/harness_analysis.md