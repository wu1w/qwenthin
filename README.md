# Qwenthin（q38）

本地编码助手控制台。主界面是 `q38 web`，对话循环在进程内跑，连任意 OpenAI 兼容接口。主要对着 **Qwen3.8-27B**（同架构的 3.5 / 3.6 也能用）。

更细的内部结构见 [技术说明](docs/architecture.md)。

## 需要什么

- Rust（`rustup`）
- 一个 OpenAI 兼容端点：llama.cpp `llama-server`、vLLM、SGLang、Ollama `/v1`，或远程网关
- 打开控制台用浏览器

## 三步跑起来

```bash
# 1. 编译安装
cargo install --path crates/q38-cli
# 或: cargo build --release && 把 target/release/q38 放到 PATH

# 2. 探测端点（写入 ~/.q38-agent/config.toml）
q38 probe --base-url http://127.0.0.1:8080/v1

# 3. 打开控制台
q38 web
```

浏览器会打开 `http://127.0.0.1:3848/`。第 2 步可以跳过：在控制台 **模型** 页填 `base_url` / `model` / `api_key` 并点检测。本地服务一般 `api_key` 留空或随便写。

`q38 web --bind 127.0.0.1:3848 --no-open` 只起服务、不弹浏览器。

## 控制台怎么用

侧栏三组：

| 分组 | 页 | 做什么 |
|---|---|---|
| 聊天 | 聊天 | 主对话。工具轨迹、思考、Markdown / 表格 / 图、复制、解码速度 |
| 聊天 | 收件箱 | 工具审批（ask 模式下写文件、跑命令会停在这里） |
| 控制 | 频道 | QQ / 飞书 / Telegram 等外部入口 |
| 控制 | 会话 | 历史 JSONL、切换、删除 |
| 控制 | 定时任务 | 到点向当前工作区发一轮（主机定时器，不是模型工具） |
| 控制 | 心跳 | 周期性巡检提示 |
| 工作区 | 文件 | 选文件夹、看树、预览。换目录后 read/write/edit/bash 都跟过去 |
| 工作区 | 技能 | `SKILL.md` 目录，会话里 `/技能名` 触发 |
| 工作区 | MCP | 外部 MCP 进程，一个 `mcp()` 工具 |
| 工作区 | 工具 | 当前冻结的 tools[] |
| 底栏 | 模型 | 端点、模型名、窗口、步数 |
| 底栏 | 安全 | 审批档位 |
| 底栏 | 用量 | token / 缓存命中 |

### 选工作区

**文件** 页可以：

- 粘贴绝对路径或 `~/…`，回车或点 **打开**
- **系统选择**：系统文件夹对话框
- **浏览**：在控制台里点进子目录，再 **使用此文件夹**
- 快捷：主目录 / 桌面 / 文稿 / 下载 / 启动目录，以及最近用过的路径

换目录会写入 `~/.q38-agent/config.toml` 的 `[console] workspace`。下次只跑 `q38 web`（不带 `--workspace`）仍打开这个文件夹。正在跑一轮时不能换。

命令行 `q38 web --workspace /path/to/project` 优先于配置文件。

### 审批

默认 ask：写文件、bash 会先问。可在聊天或安全页改成自动 / yolo。这是本机控制台，不是沙箱产品。

## 命令行

```bash
q38 web                         # 控制台（主入口）
q38 --print "总结这个仓库"       # 一次性跑完打到 stdout
q38                             # 在 TTY 且没有 prompt 时开 TUI
q38 probe                       # 探测端点能力，写 probe.json
q38 --sidecar                   # stdio JSON-RPC（给可选 dsh 插件用）
```

全局 `--workspace` 指定根目录。`--print` 适合脚本；日常请用 `q38 web`。

环境变量 `Q38_BASE_URL` / `Q38_API_KEY` / `Q38_MODEL` **只覆盖 CLI/TUI**。`q38 web` 故意不读它们，避免设置页显示的和真正用的不一致。Web 模式请改控制台或 `config.toml`。

## 配置

路径：`~/.q38-agent/config.toml`。字段说明和默认值见仓库里的 [`config.example.toml`](config.example.toml)。

常见项：

- `[server]` 端点、key、模型、引擎 profile、家族
- `[context] working_window` 默认 262144，不要压成 16k
- `[console] workspace` 控制台上次选的文件夹
- `[web]` 搜索工具（无 key 也能用；有 Tavily key 自动升级）
- `[mcp]` / 技能目录 overlay

工作区还可以放 `AGENT.md`（人设）、`.q38/skills`、`.q38/mcp.toml`。

## 改控制台前端

`q38 web` 托管的是已经编好的 `web/console/dist`。改 React 之后：

```bash
cd web/console && npm install && npm run build
```

再重启 `q38 web`。开发时 `npm run dev` 会把 `/api` 代理到本机 3848。

## 可选：dsh 插件

已经在用 dsh 的人可以 `q38 dsh-install`，把同一套 loop 挂到 dsh 里。**产品壳仍是 `q38 web`**，不是 dsh。说明见 [`plugins/dsh-plugin-q38/README.md`](plugins/dsh-plugin-q38/README.md)。
