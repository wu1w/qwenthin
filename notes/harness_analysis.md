# q-harness × Qwen3.8-27B 适配度分析

## 架构总览

为 Qwen3.8-27B 本地部署极致打磨的 Agent Harness，Rust 写，核心 q38-loop crate。

设计哲学：**"少即是多"**——极短 prompt、frozen 4 工具、字节锁定模板、多层兜底。

## 模块一览

- Agent Loop (agent/mod.rs) — ReAct 主循环，gate 决策，parallel tool dispatch
- Template (template.rs) — minijinja 本地渲染官方 Jinja chat template
- Adapter (adapter/mod.rs) — OpenAI-compat 请求构建，按引擎分支
- XML Tools (agent/xml_tools.rs) — Qwen 原生 tool_call XML + JSON fallback
- Policy (policy.rs) — effort × max_think_tokens 精细调控
- Family (family.rs) — 模型代数识别 + capability 探测
- Gates (paw_loop/gates/) — 6 道安全门
- Sticky (sticky.rs) — hidden-user 注入
- Echo (echo.rs) — 剥离 27B 复读
- Stutter (stutter.rs) — 重复输出检测
- Vendor (vendor.rs) — SHA256 锁定
- Probe (probe.rs) — 端点探测

## 适配度评分

### 1. 工具调用格式 — 10/10
xml_tools.rs 支持三级解析：XML function、JSON、lenient XML。当前模型确实主要用 XML 格式，偶发 JSON。strict→lenient fallback 覆盖了漏写闭合标签的情况。think 通道里泄漏的 tool_call 也会被恢复。

### 2. Prompt 策略 — 10/10
DEFAULT_AGENT_MD 只有 10 个中文字。注释说 "One line of voice markers beats a 4-8 line essay on this 27B"。对 27B 来说大 system prompt 会稀释 attention，这个策略正确。

### 3. 上下文窗口 — 10/10
CODING_CTX_TOKENS = 262144，精确匹配 Qwen3.8 原生上下文。

### 4. Think 调控 — 8/10
effort (low/medium/xhigh) × max_think_tokens 精细调控。默认 low/512。xhigh 4096 对复杂多步推理可能偏紧。EffortController 在 parse-fail 时自动 upgrade，但连续 3 次就停。

### 5. 安全防护 — 10/10
6 道 gate：doom-loop / iteration / timeout / token-budget / tool-budget / lossy。分层合理，doom halt 不 lecture 模型。

### 6. 容错/兜底 — 10/10
echo strip、parse retry (3次)、watchdog oneshot、stutter detect。

### 7. 前缀缓存 — 10/10
SHA256 锁定 Jinja + tokenizer。id_slot pin llama.cpp KV slot。

### 8. 扩展性 — 5/10
Family enum 硬绑 Qwen 家族。换模型需要改多处。

## 总评：92%

## 建议
- max_think_xhigh 4096 对多文件重构可能偏紧
- 如果实际使用中频繁撞 think cap 可提到 8192
