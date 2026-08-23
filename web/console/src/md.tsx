import { memo, useMemo, useState } from "react";
import { asChartJson, DiagramView, parseChart, parseMermaid, type Diagram } from "./md-chart";
import { highlight, normLang, type HlTok } from "./md-hl";
import { Icon } from "./ui";

const esc = (s: string) =>
  s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]!));

type Align = "l" | "c" | "r";
type ListItem = { html: string; task?: boolean; checked?: boolean };

type Block =
  | { k: "p"; html: string }
  | { k: "h"; lvl: number; html: string }
  | { k: "list"; ordered: boolean; items: ListItem[] }
  | { k: "quote"; html: string }
  | { k: "hr" }
  | { k: "code"; lang: string; code: string }
  | { k: "table"; align: Align[]; head: string[]; rows: string[][] };

/** 行内：转义后再套标记。链接 / 图片只放行 http(s)。 */
export function mdInline(escaped: string): string {
  let s = escaped.replace(/`([^`]+)`/g, "<code>$1</code>");
  s = s.replace(/!\[([^\]]*)\]\((https?:\/\/[^\s)]+)\)/g, '<img class="md-img" alt="$1" src="$2" loading="lazy" />');
  s = s.replace(
    /\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
    '<a href="$2" target="_blank" rel="noreferrer">$1</a>',
  );
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/~~([^~]+)~~/g, "<del>$1</del>");
  s = s.replace(/(^|[^*])\*([^*\n]+)\*(?!\*)/g, "$1<em>$2</em>");
  s = s.replace(/(^|[\s(（【])_([^_\n]+)_(?=[\s).,!?;:）】]|$)/g, "$1<em>$2</em>");
  s = s.replace(
    /(^|[^"'>])(https?:\/\/[^\s<]+[^\s<.,;:!?]) /g,
    '$1<a href="$2" target="_blank" rel="noreferrer">$2</a>',
  );
  // 行尾裸链（上面那条要求尾空格）
  s = s.replace(
    /(^|[^"'>])(https?:\/\/[^\s<]+[^\s<.,;:!?])$/g,
    '$1<a href="$2" target="_blank" rel="noreferrer">$2</a>',
  );
  return s;
}

function inline(raw: string): string {
  return mdInline(esc(raw));
}

export function parseMd(src: string): Block[] {
  const lines = src.replace(/\r\n/g, "\n").split("\n");
  const out: Block[] = [];
  let i = 0;
  let para: string[] = [];
  let list: { ordered: boolean; items: ListItem[] } | null = null;
  let quote: string[] = [];

  const flushPara = () => {
    if (para.length) {
      out.push({ k: "p", html: para.join("<br>") });
      para = [];
    }
  };
  const flushList = () => {
    if (list) {
      out.push({ k: "list", ordered: list.ordered, items: list.items });
      list = null;
    }
  };
  const flushQuote = () => {
    if (quote.length) {
      out.push({ k: "quote", html: quote.join("<br>") });
      quote = [];
    }
  };
  const flushText = () => {
    flushPara();
    flushList();
    flushQuote();
  };

  while (i < lines.length) {
    const raw = lines[i];
    const t = raw.trim();

    const fence = /^(`{3,})(.*)$/.exec(t);
    if (fence) {
      flushText();
      const ticks = fence[1].length;
      const lang = fence[2].trim().split(/\s+/)[0] || "";
      const body: string[] = [];
      i++;
      while (i < lines.length) {
        const close = lines[i].trim();
        const closer = /^(`{3,})\s*[\w+-]*\s*$/.exec(close);
        if (closer && closer[1].length >= ticks) break;
        body.push(lines[i]);
        i++;
      }
      if (i < lines.length) i++;
      out.push({ k: "code", lang, code: body.join("\n").replace(/\n$/, "") });
      continue;
    }

    if (!t) {
      flushText();
      i++;
      continue;
    }

    if (/^(-{3,}|\*{3,}|_{3,})$/.test(t)) {
      flushText();
      out.push({ k: "hr" });
      i++;
      continue;
    }

    const tbl = readTable(lines, i);
    if (tbl) {
      flushText();
      out.push(tbl.block);
      i = tbl.next;
      continue;
    }

    const h = /^(#{1,4})\s+(.*)$/.exec(t);
    if (h) {
      flushText();
      const lvl = Math.min(6, h[1].length + 2);
      out.push({ k: "h", lvl, html: inline(h[2]) });
      i++;
      continue;
    }

    const q = /^>\s?(.*)$/.exec(t);
    if (q) {
      flushPara();
      flushList();
      quote.push(inline(q[1]));
      i++;
      continue;
    }
    if (quote.length) flushQuote();

    const task = /^[-*•]\s+\[([ xX])\]\s+(.*)$/.exec(t);
    if (task) {
      flushPara();
      if (list?.ordered) flushList();
      if (!list) list = { ordered: false, items: [] };
      list.items.push({ html: inline(task[2]), task: true, checked: task[1] !== " " });
      i++;
      continue;
    }

    const ul = /^[-*•]\s+(.*)$/.exec(t);
    if (ul) {
      flushPara();
      if (list?.ordered) flushList();
      if (!list) list = { ordered: false, items: [] };
      list.items.push({ html: inline(ul[1]) });
      i++;
      continue;
    }

    const ol = /^\d{1,3}[.、)]\s+(.*)$/.exec(t);
    if (ol) {
      flushPara();
      if (list && !list.ordered) flushList();
      if (!list) list = { ordered: true, items: [] };
      list.items.push({ html: inline(ol[1]) });
      i++;
      continue;
    }

    flushList();
    para.push(inline(raw));
    i++;
  }
  flushText();
  return out;
}

function readTable(lines: string[], i: number): { block: Extract<Block, { k: "table" }>; next: number } | null {
  if (i + 1 >= lines.length) return null;
  if (lines[i].indexOf("|") < 0) return null;
  const head = splitRow(lines[i]);
  const sep = splitRow(lines[i + 1]);
  if (!head.length || sep.length < head.length) return null;
  if (!sep.every((c) => /^:?-{2,}:?$/.test(c))) return null;
  const align: Align[] = sep.map((c) => {
    const l = c.startsWith(":");
    const r = c.endsWith(":");
    return l && r ? "c" : r ? "r" : "l";
  });
  const rows: string[][] = [];
  let j = i + 2;
  while (j < lines.length && lines[j].indexOf("|") >= 0 && lines[j].trim()) {
    if (/^[-*•>\s#`]/.test(lines[j].trim()) && !lines[j].includes("|")) break;
    rows.push(padRow(splitRow(lines[j]), head.length));
    j++;
  }
  return { block: { k: "table", align, head: padRow(head, head.length), rows }, next: j };
}

function splitRow(line: string): string[] {
  let s = line.trim();
  if (s.startsWith("|")) s = s.slice(1);
  if (s.endsWith("|")) s = s.slice(0, -1);
  return s.split("|").map((c) => c.trim());
}
function padRow(row: string[], n: number): string[] {
  const out = row.slice(0, n);
  while (out.length < n) out.push("");
  return out;
}

function looksMermaid(lang: string, code: string): boolean {
  const l = normLang(lang);
  if (l === "mermaid" || l === "mmd") return true;
  if (l) return false;
  return /^(?:graph|flowchart|sequenceDiagram|pie|xychart(?:-beta)?)\b/.test(code.trim());
}

function diagramOf(lang: string, code: string, live?: boolean): Diagram | null {
  if (live) return null;
  const l = normLang(lang);
  if (l === "chart") return parseChart(code);
  if (l === "json") {
    const j = asChartJson(code);
    return j ? parseChart(code) : null;
  }
  if (looksMermaid(lang, code)) return parseMermaid(code);
  return null;
}

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.left = "-9999px";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      return ok;
    } catch {
      return false;
    }
  }
}

function sanitizeSvg(src: string): string | null {
  const t = src.trim();
  if (!/^<svg[\s>]/i.test(t)) return null;
  try {
    const doc = new DOMParser().parseFromString(t, "image/svg+xml");
    if (doc.querySelector("parsererror")) return null;
    const svg = doc.documentElement;
    if (svg.tagName.toLowerCase() !== "svg") return null;
    const bad = new Set(["script", "foreignobject", "iframe", "object", "embed", "animate", "set", "animatetransform", "animatecolor", "animatemotion"]);
    const walk = (el: Element) => {
      const kids = [...el.children];
      for (const kid of kids) {
        if (bad.has(kid.tagName.toLowerCase())) {
          kid.remove();
          continue;
        }
        for (const attr of [...kid.attributes]) {
          const n = attr.name.toLowerCase();
          const v = attr.value.trim();
          if (n.startsWith("on") || n === "href" || n.endsWith(":href")) {
            if (n.startsWith("on") || /^(javascript|data):/i.test(v)) kid.removeAttribute(attr.name);
          }
        }
        walk(kid);
      }
    };
    walk(svg);
    for (const attr of [...svg.attributes]) {
      if (attr.name.toLowerCase().startsWith("on")) svg.removeAttribute(attr.name);
    }
    return new XMLSerializer().serializeToString(svg);
  } catch {
    return null;
  }
}

function CodeBlock({ lang, code, live }: { lang: string; code: string; live?: boolean }) {
  const [copied, setCopied] = useState(false);
  const [src, setSrc] = useState(false);
  const diagram = useMemo(() => diagramOf(lang, code, live), [lang, code, live]);
  const svg = useMemo(() => (normLang(lang) === "svg" && !live ? sanitizeSvg(code) : null), [lang, code, live]);
  const showFig = (!!diagram || !!svg) && !src;
  const label = diagram ? (diagram.kind === "pie" ? "pie" : diagram.kind === "xy" ? "chart" : diagram.kind === "seq" ? "sequence" : "mermaid") : lang || "code";
  const toks = useMemo(() => (showFig ? [] : highlight(code, lang)), [showFig, code, lang]);

  const onCopy = () => {
    void copyText(code).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    });
  };

  return (
    <div className={`md-code${showFig ? " fig" : ""}`}>
      <div className="md-code-bar">
        <span className="md-code-lang">{label}</span>
        <span className="md-code-actions">
          {diagram || svg ? (
            <button type="button" className="md-code-btn" onClick={() => setSrc((v) => !v)}>
              {src ? "图" : "源码"}
            </button>
          ) : null}
          <button type="button" className="md-code-btn" onClick={onCopy} aria-label="复制代码">
            <Icon name={copied ? "check" : "copy"} />
            {copied ? "已复制" : "复制"}
          </button>
        </span>
      </div>
      {showFig && diagram ? <DiagramView d={diagram} /> : null}
      {showFig && svg ? (
        <div className="md-svg" dangerouslySetInnerHTML={{ __html: svg }} />
      ) : null}
      {!showFig ? (
        <pre>
          <code>
            {toks.length ? <Hl toks={toks} /> : code}
          </code>
        </pre>
      ) : null}
    </div>
  );
}

function Hl({ toks }: { toks: HlTok[] }) {
  return (
    <>
      {toks.map((t, i) =>
        t.k ? (
          <span key={i} className={`hl-${t.k}`}>
            {t.t}
          </span>
        ) : (
          <span key={i}>{t.t}</span>
        ),
      )}
    </>
  );
}

function isNumeric(s: string) {
  const t = s.replace(/,/g, "").trim();
  if (!t) return false;
  return /^-?\d+(\.\d+)?%?$/.test(t) || /^-?\d+(\.\d+)?[eE][+-]?\d+$/.test(t);
}
function numVal(s: string) {
  return Number(s.replace(/,/g, "").replace(/%$/, "").trim());
}

function TableBlock({ align, head, rows }: { align: Align[]; head: string[]; rows: string[][] }) {
  const numeric = head.map((_, c) => rows.length > 0 && rows.every((r) => !r[c] || isNumeric(r[c])));
  const maxes = numeric.map((on, c) => (on ? Math.max(0, ...rows.map((r) => Math.abs(numVal(r[c] || "0")))) : 0));
  return (
    <div className="md-table-wrap">
      <table className="md-table">
        <thead>
          <tr>
            {head.map((h, i) => (
              <th key={i} className={align[i] === "c" ? "c" : align[i] === "r" || numeric[i] ? "r" : undefined}>
                <span dangerouslySetInnerHTML={{ __html: inline(h) }} />
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, ri) => (
            <tr key={ri}>
              {row.map((cell, ci) => {
                const n = numeric[ci];
                const v = n ? numVal(cell) : 0;
                const pct = n && maxes[ci] > 0 ? Math.min(100, (Math.abs(v) / maxes[ci]) * 100) : 0;
                return (
                  <td
                    key={ci}
                    className={`${align[ci] === "c" ? "c" : align[ci] === "r" || n ? "r" : ""}${n ? " num" : ""}`}
                  >
                    {n && pct > 0 ? <span className="md-spark" style={{ width: `${pct}%` }} /> : null}
                    <span dangerouslySetInnerHTML={{ __html: n ? esc(cell) : inline(cell) }} />
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function BlockView({ b, live }: { b: Block; live?: boolean }) {
  switch (b.k) {
    case "p":
      return <p dangerouslySetInnerHTML={{ __html: b.html }} />;
    case "h": {
      const Tag = `h${b.lvl}` as "h3" | "h4" | "h5" | "h6";
      return <Tag dangerouslySetInnerHTML={{ __html: b.html }} />;
    }
    case "quote":
      return <blockquote dangerouslySetInnerHTML={{ __html: b.html }} />;
    case "hr":
      return <hr className="md-hr" />;
    case "code":
      return <CodeBlock lang={b.lang} code={b.code} live={live} />;
    case "table":
      return <TableBlock align={b.align} head={b.head} rows={b.rows} />;
    case "list": {
      const Tag = b.ordered ? "ol" : "ul";
      return (
        <Tag className={b.items.some((it) => it.task) ? "md-tasks" : undefined}>
          {b.items.map((it, i) => (
            <li key={i} className={it.task ? "md-task" : undefined}>
              {it.task ? (
                <input type="checkbox" disabled checked={!!it.checked} aria-hidden />
              ) : null}
              <span dangerouslySetInnerHTML={{ __html: it.html }} />
            </li>
          ))}
        </Tag>
      );
    }
    default:
      return null;
  }
}

/** 正文块：parse 只在文本变化时重算。流式未闭合的围栏当代码块，图等收束后再画。 */
export const MdText = memo(function MdText({ text, live }: { text: string; live?: boolean }) {
  const blocks = useMemo(() => parseMd(text), [text]);
  return (
    <div className={`msg-a${live ? " caret" : ""}`}>
      {blocks.map((b, i) => (
        <BlockView key={i} b={b} live={live && i === blocks.length - 1} />
      ))}
    </div>
  );
});

