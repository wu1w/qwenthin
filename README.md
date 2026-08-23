# q38

Reasoning-economy coding agent for the Qwen3.8 / Qwen3.5 family. Loop is `q38-loop`. Frozen OpenAI `tools[]` stay frozen (`read` / `write` / `edit` / `bash`).

## Quick start (3 steps)

You need a Rust toolchain (`rustup`) and any OpenAI-compatible endpoint (llama.cpp `llama-server`, vLLM, SGLang, Ollama's `/v1`, or a remote URL).

```
# 1. Build & install the binary
cargo install --path crates/q38-cli        # or: cargo build --release && use target/release/q38

# 2. Point it at your model endpoint (writes ~/.q38-agent/config.toml)
q38 probe --base-url http://127.0.0.1:8080/v1

# 3. Open the console
q38 web
```

The browser opens `http://127.0.0.1:3848/`. If step 2 was skipped, the console's 模型 (Settings) page can fill in `base_url` / `model` / `api_key` and test the connection — no file editing needed. Local endpoints usually take an empty `api_key`.

Environment variables `Q38_BASE_URL` / `Q38_API_KEY` / `Q38_MODEL` affect **CLI/TUI runs only**; `q38 web` deliberately ignores them so the settings page always tells the truth.

## Shell

**Primary UI is the local Web console:**

```
q38 web
```

Opens `http://127.0.0.1:3848/` (override with `--bind`). Same in-process `SidecarSession` as the TUI: chat, sessions, uploads, workspace files, channels, skills/MCP catalog, approvals inbox. Cron and heartbeat are host timers that call `turn.start` — they are not model tools.

`--print` is oneshot. A TTY with no prompt still opens the TUI.

```
q38 --print "summarize this repo"
q38                    # TUI on a TTY
q38 web --no-open      # serve only
```

Config lives in `~/.q38-agent/config.toml` (see `config.example.toml`).

## Optional dsh plugin

`plugins/dsh-plugin-q38` is an optional stdio client for people who already run dsh. It is **not** the product shell. `q38 dsh-install` remains as a helper; do not treat it as the install path.

## Console frontend

`web/console` is a Vite + React source tree. `q38 web` serves `web/console/dist` (a self-contained SPA so the binary does not need Node). To rebuild the React bundle:

```
cd web/console && npm install && npm run build
```
