# Serve checklist (q-harness)

Primary quality gate: **Qwen3.8-27B** on an OpenAI-compatible endpoint.
llama.cpp + MI210 + Unsloth UD-Q8 is the development reference box, not the only target.
3.6 / 3.5 are supported at the builder/template layer; their tokenizers are not vendored yet.

Run `q38 probe` against the endpoint. It writes `~/.q38-agent/probe.json`.

## A. Family (any engine / quant)

1. Model is Qwen3.5 / 3.6 / 3.8. `family` auto-detects from the model id, or set `family` in `~/.q38-agent/config.toml`.
2. Per-request `enable_thinking` works at **token** layer (`off` ⇒ think tokens ≤ 4). Red light otherwise — `/fast` is useless.
3. Declared `reasoning_effort` values change think length. **Qwen3.8** accepts `low` / `medium` / `xhigh`. 3.5 / 3.6 templates have no effort sentence — the builder omits the key.
4. `preserve_thinking`: 3.8/3.6 daily `false`, `/mode think` `true`. 3.5: do not send.
5. Client sends the full official sampling table (thinking vs instruct).
6. Context **262,144** (native). Short windows force compact/re-read and ruin coding. Do **not** turn on static YaRN to fake 262k; YaRN is for 1M and hurts short text.
7. Tool-call: OpenAI `tool_calls` and/or XML `<tool_call>`.
8. Keep-alive; do not idle-unload the model.
9. Cached-token field missing ⇒ hit-rate `n/a`.
10. Prefix cache on if the engine has it. MTP is optional.

## B. Engine appendix (fill the one you connected)

### llama.cpp (reference box)

- `--jinja` with the official (or community-fixed) **that generation** template.
- MTP: `--spec-type draft-mtp` if stable; otherwise off. Record `mtp=yes|no` in probe.
- MI210 backend HIP vs Vulkan: write into probe notes, do not guess in code.
- Some versions pin `--reasoning` at start and ignore per-request kwargs. Probe must prove the toggle.

### vLLM

- Prefix caching on.
- Qwen reasoning / tool parser.
- Do not rely on a start-time-only thinking lock. Kwargs live in `extra_body.chat_template_kwargs`.

### SGLang

- Radix cache on.
- Reasoning parser.
- Speculative decoding flags recorded as `mtp`.

### generic (Ollama, LM Studio, …)

- Only keys the probe proved. Dynamic depth may be unavailable (yellow, not a fake green).

Quant (UD-Q8 / Q4 / FP8 / AWQ / GPTQ / …) is a **label + quality gate**, not a builder branch.

## Config

See `config.example.toml`. First `q38 probe` creates `~/.q38-agent/config.toml` if missing.
