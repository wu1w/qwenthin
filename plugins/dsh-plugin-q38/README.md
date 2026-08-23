# dsh-plugin-q38

Optional stdio client. **The product shell is `q38 web`**, not dsh.

dsh 只当可选壳。官方 `agent-loop` 关掉，同一套 `q38 --sidecar` 跑工具、ThinkPolicy、JSONL。不要把这个插件当交付面。

不要在这个包里写第二套工具循环、Cordis 重写、或 MCP。

## 可选安装

已经有 `q38` 在 PATH 上：

```
q38 dsh-install
```

仓库脚本同样可用，但不是推荐入口：

```
./scripts/install-dsh-q38.sh
```

会把插件拷到 `~/.q38-agent/dsh-plugin-q38` 并注册 dsh profile。日常请用：

```
q38 web
```

模型、key、workspace 仍走 `~/.q38-agent/config.toml`。dsh 自己的模型页不会驱动 loop。

## 架构

```
dsh Web / Host
  ctx.agents.followup / cancel
        │
        ▼
dsh-plugin-q38   (Cordis: setFactory, 翻译 session 事件, 皮肤)
        │  stdio JSON-RPC
        ▼
q38 --sidecar    (Rust: tools / ThinkPolicy / JSONL / compact)
```

`cordis.patch.yml` 把 `agent-loop` 设为 `disabled: true`，插入 `q38-loop`。dsh 的 fs/bash 工具动物园还在进程里，但 factory 不会调用它们。

## Spawn

```
q38 --sidecar --workspace <dir> --session <id>
```

stdio: `['pipe', 'pipe', 'inherit']`。二进制默认 `q38`，可用 `Q38_BIN` 或 patch `config.command` 指到绝对路径。`q38 dsh-install` 会把当前可执行文件写进 profile overlay。

## Methods

| method | params | result |
|---|---|---|
| `session.open` | `{ session, workspace, mode? }` | `{ ok: true, session, channel }` |
| `session.list` | `{ search? }` | `{ ok: true, sessions: [...] }` |
| `session.resume` | `{ session? \| search? }` | `{ ok: true, session, title }` |
| `session.new` | `{ title? }` | `{ ok: true, session, from }` |
| `session.title` | `{ title }` | `{ ok: true, text }` |
| `session.delete` | `{ session }` | `{ ok: true, deleted }` |
| `session.status` / `history` / `context` | _(none)_ | `{ ok: true, text }` |
| `session.compress` | _(none)_ | `{ ok: true, text }` |
| `slash` | `{ text }` | `{ ok: true, text? }` or starts a turn |
| `turn.start` | `{ prompt \| text, content_parts? }` | `{ ok: true }` (held until the turn finishes) |
| `turn.abort` | _(none)_ | `{ ok: true }` |
| `turn.queue` | `{ prompt \| text }` | `{ ok: true, queued }` or starts a turn if idle |
| `turn.steer` | `{ prompt \| text }` | `{ ok: true, steered }` |
| `channel.list` | _(none)_ | `{ ok: true, channels }` |
| `channel.inbound` | QwenPaw native payload | `{ ok: true }` |

`/mode …` forks the JSONL。`/think` 和 `/fast` 只 append `policy`。斜杠从 dsh 输入框进来时，插件看到前导 `/` 走 `slash`，否则 `turn.start`。

## 皮肤

| id | 方案 |
|---|---|
| `q38-ink` | 深色暖炭底 + 青绿强调（默认） |
| `q38-paper` | 浅色纸色 + 同一青绿 |

Host 在 HTML 里注入 `--dsw-alias-*`，client bundle 往 `ctx.theme` 注册同一套，设置里可切。不搬像素鲸、壁纸、桌宠。

## Notifications

`event.append` — JSONL session events plus ephemeral **`delta`** (not persisted).

```json
{ "type": "delta", "channel": "reasoning" | "content", "text": "…", "delta": true, "reset": true }
```

插件把这些投影成 dsh 的 `user/message` / `assistant/chunk` / `assistant/message` / `tool/call` / `tool/result`，Web 对话区才能画。q38 JSONL 不改。
