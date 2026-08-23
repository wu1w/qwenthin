# 技术说明

本文描述 Qwenthin（仓库 / CLI 名 `q38`）**当前实现**。

## 它是什么

一个本机编码优化的 harness：短系统提示、冻结的 OpenAI `tools[]`、可观测的一轮轨迹、全栈rust。

主交付面是 **进程内 Web 控制台**（`q38 web`）。结构是一个后端+前端electron壳。

优化对象是 **q35架构模型的 agent 契约**（思考开关、effort、工具 XML + `tool_calls`），主要深耕qwen3.8 27B模型适配，专治各种雷霆大思考。

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

`q38-loop` 的工具轨迹形状参考了下QwenPaw 的行为；家族请求构造和 probe 是我自己写的。

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

会话开始时冻结一份 `tools[]`，中途不改 schema 字节（这块主要参考了dsh的玩法，提高缓存命中率，降低本地模型的负担）。

**日常 agent 四件套（冻结，顺序固定）：**

`read` · `write` · `edit` · `bash` （所以windows用户建议买台mac或者装一个git bash）

**按配置追加：**

| 工具 | 何时出现 |
|---|---|
| `web` | `[web] enabled`（默认开；无 key 走 Bing/DuckDuckGo HTML + 抓取） |
| `mcp` | 配置里列出了 MCP server |
| `skill` | 技能目录存在时 |
| `view` | 打开图片 / 音视频 |
| `search` | 代码搜索 |
| `recall` / `memory_search` | compact 之后再挂上，避免改冻结四工具的 JSON |

`code` 模式是另一组：`run_code`、`read`、`bash`。

`search` 使用函数级 SQLite FTS 索引，只把命中的有限代码片段交给模型。Git 工作区的索引缓存在 `~/.q38-agent/code-index/`，再次打开时按文件大小和纳秒 mtime 增量刷新；项目目录里不生成索引文件。工具写文件后会即时刷新对应条目。

`bash` 对模型保持一个名字和一种常用语法：macOS/Linux 走无 profile Bash，Windows 优先自动发现 Git Bash，没有时才退到无 profile PowerShell。可用 `Q38_SHELL` 显式覆盖，但正常安装无需给模型增加操作系统判断提示。

模型若发 Qwen 风格的 XML `<tool_call>`，和 OpenAI `tool_calls` 走同一套解析合并。

工作区路径用 `Workspace` 做相对解析。控制台换根目录等于换这个 `Workspace`，并 `refresh_surface` 重载该目录下的技能 / MCP overlay。

## agent轨迹

1. 用户消息（可带图片等 `content_parts`）进入 mailbox（忙时排队或打断，看 `[channels]` 的 busy 策略）。
2. `SidecarSession` 组 messages：角色边界 + 可选 `AGENT.md` + 冻结 tools + 历史。
3. HTTP 补全；思考 / 正文分通道流式推到 WS。
4. 工具调用按审批模式停或放行；结果写回 messages，直到模型停或打到 `max_steps`。
5. 事件追加到会话 JSONL；`stop` 结束本轮。控制台会重放本轮前半段，避免 WS 丢包后画面残缺。

q35 架构模型，尤其是 Qwen3.8，偶尔会出现“雷霆大思考”甚至把思考预算耗尽。默认 `auto` 映射到官方中性的 `medium`，模板不注入深浅指令，由模型按任务自行分配思考强度；最大思考 token 只作失控上限。历史思考默认保留。`/think`、`--think`、`/fast` 仍可人工覆盖。thinking 采样固定对齐官方 `temperature=1.0, top_p=0.95, top_k=20`；`low_precision` 只收紧本机围栏，模型看不见。

轨迹控制遵循“模型主导、harness 软干预”：测试转红、修改测试期望和编辑摇摆只作为隐藏事实反馈，不替模型决定停止或回退；同参工具连续 3 次才提醒，连续 6 次完全不换路才停止（有状态 shell 为 7 次）。思考触及上限时保留模型选择的思考模式，追加一次简短收敛提示并给更宽的一次重试；只有再次触顶、时间、步数或上下文硬上限才终止。重复答案伴随暂存/清理动作时先延后该批工具，把选择权交还模型，避免在提醒送达前执行低信息量写入或删除。

上下文窗口默认 **262144**。超过 `working_window * compact_ratio` 才 compact；建议用的话还是尽量128K以上，低了体验不太好。

## 会话与状态

- 会话文件：`~/.q38-agent/sessions/<id>.jsonl`
- 配置：`~/.q38-agent/config.toml`（`q38 web` 只认这个文件，不认 `Q38_*`）
- probe：`~/.q38-agent/probe.json`
- 控制台上次工作区：`[console] workspace`

Web 启动时：若 CLI 没传 `--workspace`，用配置里的路径；路径不存在则退回进程 cwd。

## HTTP API（本机）

控制台前缀 `/api`。和前端相关的是这些：

- `POST /api/rpc` sidecar 方法（`turn.start` / `session.*` / `slash` …）
- `GET /api/events` WebSocket（`state`、`event.append`、`history.replace`、`permit.ask`）
- `GET/POST /api/config` 模型与行为
- `GET /api/tree`、`GET /api/files` 工作区树和预览
- `GET/POST /api/workspace` 切换根目录；`POST /api/workspace/pick` 系统选文件夹；`GET /api/workspace/ls` 列子目录
- skills / mcp / channels / jobs / heartbeat / permit / usage

绑定默认 `127.0.0.1`，跨域响应只放行 loopback 开发页，WebSocket 校验同源。本机控制台优先好用，不做远程多租户安全模型。非 loopback 绑定必须显式传 `--allow-lan`，含义是“我信任这个局域网”；换工作区仍可指向本机任意文件夹。

## 前端

`web/console` 是 SPA。`q38-web` 只托管 `dist/`，运行时不需要 Node。

对话页自己做 Markdown（表格、代码高亮、一部分图），不额外拉 npm 渲染库。流式时未闭合的围栏先当代码，闭合后再当图。

前端让grok+kimi k3+fable5调了三轮美化过，成本巨大，各位老爷麻烦看到的点个星星，感激不尽。

## 频道与插件

`q38-loop::channel` 把 QQ / 飞书 / Telegram 等收成同一套 mailbox。凭证在配置里，控制台 **频道** 页编辑。

`q38 --sidecar` 是 newline JSON-RPC（stdio）。dsh 插件只翻译 UI 事件，**禁止**再开一套工具循环。见 `plugins/dsh-plugin-q38/README.md`。

## 为什么做这个玩意儿

- 我平时习惯用Hermes接本地模型，这次qwen3.8 27B的体验很不好， 超长 system + 全量 tool 动物园扣在 27B 上，加上模型爱思考，难用的一匹。
- dsh和pi agent的思路我很喜欢，但pi太简陋了，dsh折腾，原版agent给DS优化的。
- 阿里的几个agent难用，尤其是qoder，是史。我还试了qwenpaw接qwen3.8 27B，超过80轮工具的大会话harness会挂掉，再起不能，明显没对他家自己的本地模型做过优化适配。
