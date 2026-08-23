# 技术说明

本文描述 Qwenthin（仓库 / CLI 名 `q38`）**当前实现**，不是设计草案。怎么安装、怎么点界面见 [README](../README.md)。

## 它是什么

一个本机编码 harness：短系统提示、冻结的 OpenAI `tools[]`、可观测的一轮轨迹。模型通过 HTTP 说话，工具在 Rust 里执行，工作区就是用户选的那个文件夹。

主交付面是 **进程内 Web 控制台**（`q38 web`）。TUI、`--print`、stdio sidecar 共用同一套 `SidecarSession`，不是第二套 loop。

优化对象是 **Qwen3.5 家族的 agent 契约**（思考开关、effort、工具 XML + `tool_calls`），不是某一个 GGUF 或某一种量化。开发时常用 Qwen3.8-27B。

## 仓库结构

```
crates/q38-loop   循环、工具、会话 JSONL、sidecar RPC、频道、probe
crates/q38-web    axum：静态控制台 + /api + WebSocket
crates/q38-cli    q38 可执行文件
crates/q38-tui    终端 UI
crates/q38-bench  小任务基准
web/console       Vite + React；产物在 dist/，由 q38-web 托管
plugins/dsh-plugin-q38   可选 stdio 客户端（不是产品壳）
config.example.toml      ~/.q38-agent/config.toml 的字段说明
```

`q38-loop` 的工具轨迹形状来自 QwenPaw 行为的 Rust 重写；家族请求构造和 probe 是本仓库自己的。

## 进程模型

```
浏览器  ──HTTP/WS──►  q38 web (q38-web)
                         │
                         ▼
                   SidecarSession (q38-loop)
                         │
                         ├── HttpCompleter  →  OpenAI 兼容 /v1/chat/completions
                         ├── 工具            →  当前工作区磁盘 / bash
                         └── 会话 JSONL      →  ~/.q38-agent/sessions/
```

没有单独的 agent 守护进程。控制台、TUI、sidecar 都是同一个 session 对象上的 `turn.start`。

Cron / 心跳 / 频道入站也是 **主机定时器或适配器** 去调 `turn.start`，不会给模型多暴露一个 `cron` 工具。

## 工具面

会话开始时冻结一份 `tools[]`，中途不改 schema 字节（前缀缓存才稳）。

**日常 agent 四件套（冻结，顺序固定）：**

`read` · `write` · `edit` · `bash`

**按配置追加（不拆进那四个 JSON 里）：**

| 工具 | 何时出现 |
|---|---|
| `web` | `[web] enabled`（默认开；无 key 走 Bing/DuckDuckGo HTML + 抓取） |
| `mcp` | 配置里列出了 MCP server |
| `skill` | 技能目录存在时 |
| `view` | 打开图片 / 音视频 |
| `search` | 代码搜索 |
| `recall` / `memory_search` | compact 之后再挂上，避免改冻结四工具的 JSON |

`code` 模式是另一组：`run_code`、`read`、`bash`。

模型若发 Qwen 风格的 XML `<tool_call>`，和 OpenAI `tool_calls` 走同一套解析合并。

工作区路径用 `Workspace` 做相对解析。控制台换根目录等于换这个 `Workspace`，并 `refresh_surface` 重载该目录下的技能 / MCP overlay。

## 一轮怎么跑

1. 用户消息（可带图片等 `content_parts`）进入 mailbox（忙时排队或打断，看 `[channels]` 的 busy 策略）。
2. `SidecarSession` 组 messages：角色边界 + 可选 `AGENT.md` + 冻结 tools + 历史。
3. HTTP 补全；思考 / 正文分通道流式推到 WS。
4. 工具调用按审批模式停或放行；结果写回 messages，直到模型停或打到 `max_steps`。
5. 事件追加到会话 JSONL；`stop` 结束本轮。控制台会重放本轮前半段，避免 WS 丢包后画面残缺。

思考策略：日常不靠「默认 xhigh」。`/think`、`--think`、`/fast` 改 `ThinkPolicy`。`low_precision` 只收紧本机围栏，模型看不见。

上下文窗口默认 **262144**。超过 `working_window * compact_ratio` 才 compact；不要把 coding 基线压成 16k。

## 会话与状态

- 会话文件：`~/.q38-agent/sessions/<id>.jsonl`
- 配置：`~/.q38-agent/config.toml`（`q38 web` 只认这个文件，不认 `Q38_*`）
- probe：`~/.q38-agent/probe.json`
- 控制台上次工作区：`[console] workspace`

Web 启动时：若 CLI 没传 `--workspace`，用配置里的路径；路径不存在则退回进程 cwd。

## HTTP API（本机）

控制台前缀 `/api`。和前端相关的大致是：

- `POST /api/rpc` sidecar 方法（`turn.start` / `session.*` / `slash` …）
- `GET /api/events` WebSocket（`state`、`event.append`、`history.replace`、`permit.ask`）
- `GET/POST /api/config` 模型与行为
- `GET /api/tree`、`GET /api/files` 工作区树和预览
- `GET/POST /api/workspace` 切换根目录；`POST /api/workspace/pick` 系统选文件夹；`GET /api/workspace/ls` 列子目录
- skills / mcp / channels / jobs / heartbeat / permit / usage

绑定默认 `127.0.0.1`。本机控制台优先好用，不做远程多租户安全模型。换工作区可以指向本机任意文件夹。

## 前端

`web/console` 是 SPA。`q38-web` 只托管 `dist/`，运行时不需要 Node。

对话页自己做 Markdown（表格、代码高亮、一部分图），不额外拉 npm 渲染库。流式时未闭合的围栏先当代码，闭合后再当图。

## 频道与插件

`q38-loop::channel` 把 QQ / 飞书 / Telegram 等收成同一套 mailbox。凭证在配置里，控制台 **频道** 页编辑。

`q38 --sidecar` 是 newline JSON-RPC（stdio）。dsh 插件只翻译 UI 事件，**禁止**再开一套工具循环。见 `plugins/dsh-plugin-q38/README.md`。

## 明确不做的

- 把 Hermes 式超长 system + 全量 tool 动物园扣在 27B 上
- 把 dsh / Cursor 当产品壳
- 云端多用户、远程暴露 API 当默认
- 视觉 / 1M YaRN 当产品功能
- 按某一种量化调解码器
