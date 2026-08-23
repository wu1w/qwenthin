# q38 console

Vite + React source for the local Web workbench (`q38 web`). Visual language follows `web/demo` (paper content, ink sidebar, Electron titlebar). Page information architecture: 聊天 / 控制 / 工作区 / 设置.

The Rust host (`crates/q38-web`) serves `dist/`. `npm run build` writes the bundle into `dist/`.

```
npm install
npm run dev     # proxy /api → 127.0.0.1:3848 (override with Q38_WEB)
npm run build   # writes a clean dist/ (emptyOutDir)
```

Electron can wrap the same `dist/` later; the titlebar already uses `-webkit-app-region: drag`. Do not depend on `@agentscope-ai/chat`. Uploads become `content_parts` or workspace paths; the loop stays `q38-loop`.
