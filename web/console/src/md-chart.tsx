import { useId, type ReactNode } from "react";

const SERIES = ["#615ced", "#1a3057", "#16a34a", "#b45309", "#7d74f2", "#475569", "#a89dff", "#23406e"];

export type ChartKind = "pie" | "bar" | "line";

export type ChartSpec = {
  type: ChartKind;
  title?: string;
  labels?: string[];
  values?: number[];
  items?: { label: string; value: number }[];
  slices?: { label: string; value: number }[];
  series?: { name: string; values: number[] }[];
};

type FlowNode = { id: string; label: string; shape: "rect" | "round" | "diamond" | "circle" | "stadium" };
type FlowEdge = { from: string; to: string; label?: string; dashed?: boolean };
type SeqMsg = { from: string; to: string; label: string; dashed?: boolean; note?: boolean };

export type Diagram =
  | { kind: "pie"; title: string; slices: { label: string; value: number }[] }
  | { kind: "xy"; title: string; labels: string[]; bars?: number[]; lines: { name: string; values: number[] }[] }
  | { kind: "flow"; dir: "TD" | "LR"; nodes: FlowNode[]; edges: FlowEdge[] }
  | { kind: "seq"; actors: { id: string; label: string }[]; messages: SeqMsg[] };

export function parseMermaid(src: string): Diagram | null {
  const text = src.replace(/^\uFEFF/, "").replace(/\r\n/g, "\n").trim();
  if (!text) return null;
  const head = text.split("\n", 1)[0].trim();
  if (/^pie\b/i.test(head) || /^pie\b/im.test(text)) return parsePie(text);
  if (/^xychart(?:-beta)?\b/i.test(head)) return parseXy(text);
  if (/^sequenceDiagram\b/i.test(head)) return parseSeq(text);
  if (/^(?:graph|flowchart)\b/i.test(head)) return parseFlow(text);
  return null;
}

export function parseChart(src: string): Diagram | null {
  const t = src.trim();
  if (!t) return null;
  const json = asChartJson(t);
  if (json) return chartToDiagram(json);
  return parseChartLines(t);
}

export function asChartJson(src: string): ChartSpec | null {
  try {
    const j = JSON.parse(src) as ChartSpec;
    if (!j || typeof j !== "object") return null;
    if (j.type !== "bar" && j.type !== "line" && j.type !== "pie") return null;
    if (!j.values && !j.items && !j.slices && !j.series) return null;
    return j;
  } catch {
    return null;
  }
}

function chartToDiagram(j: ChartSpec): Diagram {
  const title = j.title || "";
  if (j.type === "pie") {
    const slices =
      j.slices ||
      j.items ||
      (j.labels || []).map((label, i) => ({ label, value: Number(j.values?.[i] ?? 0) }));
    return { kind: "pie", title, slices: slices.filter((s) => Number.isFinite(s.value)) };
  }
  const labels = j.labels || j.items?.map((x) => x.label) || [];
  const bars = j.type === "bar" ? j.values || j.items?.map((x) => x.value) : undefined;
  const lines = j.series || (j.type === "line" && j.values ? [{ name: title || "series", values: j.values }] : []);
  return { kind: "xy", title, labels, bars, lines };
}

function parseChartLines(src: string): Diagram | null {
  const lines = src.split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"));
  if (!lines.length) return null;
  let type: ChartKind = "bar";
  let title = "";
  const items: { label: string; value: number }[] = [];
  for (const line of lines) {
    const kv = /^(\w+)\s*[:=]\s*(.+)$/.exec(line);
    if (kv) {
      const k = kv[1].toLowerCase();
      const v = stripQ(kv[2]);
      if (k === "type" && (v === "bar" || v === "line" || v === "pie")) {
        type = v;
        continue;
      }
      if (k === "title") {
        title = v;
        continue;
      }
    }
    if (/^(bar|line|pie)$/i.test(line) && !items.length) {
      type = line.toLowerCase() as ChartKind;
      continue;
    }
    const row = /^(?:"([^"]+)"|'([^']+)'|(.+?))\s*[:=\t]\s*(-?[\d.]+)%?$/.exec(line);
    if (row) {
      items.push({ label: (row[1] || row[2] || row[3]).trim(), value: Number(row[4]) });
    }
  }
  if (!items.length) return null;
  return chartToDiagram({ type, title, items });
}

function parsePie(src: string): Diagram | null {
  const slices: { label: string; value: number }[] = [];
  let title = "";
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("%%")) continue;
    const tm = /^(?:pie\s+)?(?:showData\s+)?title\s+(.+)$/i.exec(line);
    if (tm) {
      title = stripQ(tm[1]);
      continue;
    }
    if (/^pie\b/i.test(line)) continue;
    const m = /^(?:"([^"]+)"|'([^']+)'|([^:]+))\s*:\s*(-?[\d.]+)\s*%?$/.exec(line);
    if (m) slices.push({ label: (m[1] || m[2] || m[3]).trim(), value: Number(m[4]) });
  }
  return slices.length ? { kind: "pie", title, slices } : null;
}

function parseXy(src: string): Diagram | null {
  let title = "";
  let labels: string[] = [];
  let bars: number[] | undefined;
  const lines: { name: string; values: number[] }[] = [];
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("%%") || /^xychart/i.test(line)) continue;
    const tm = /^title\s+(.+)$/i.exec(line);
    if (tm) {
      title = stripQ(tm[1]);
      continue;
    }
    const xa = /^x-axis\s+(?:[^\[]*?)?\[(.+)\]/i.exec(line);
    if (xa) {
      labels = splitList(xa[1]);
      continue;
    }
    if (/^y-axis\b/i.test(line)) continue;
    const bar = /^bar(?:\s+"([^"]+)")?\s+\[(.+)\]/i.exec(line);
    if (bar) {
      bars = splitList(bar[2]).map(Number);
      continue;
    }
    const ln = /^line(?:\s+"([^"]+)")?\s+\[(.+)\]/i.exec(line);
    if (ln) {
      lines.push({ name: ln[1] || "line", values: splitList(ln[2]).map(Number) });
    }
  }
  if (!labels.length && !bars && !lines.length) return null;
  return { kind: "xy", title, labels, bars, lines };
}

function parseSeq(src: string): Diagram | null {
  const actors: { id: string; label: string }[] = [];
  const seen = new Map<string, string>();
  const messages: SeqMsg[] = [];
  const add = (id: string, label?: string) => {
    const key = id.trim();
    if (!key) return key;
    if (!seen.has(key)) {
      seen.set(key, label || key);
      actors.push({ id: key, label: label || key });
    } else if (label) {
      seen.set(key, label);
      const a = actors.find((x) => x.id === key);
      if (a) a.label = label;
    }
    return key;
  };
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("%%") || /^sequenceDiagram/i.test(line)) continue;
    const p = /^participant\s+(\S+)(?:\s+as\s+(.+))?$/i.exec(line);
    if (p) {
      add(p[1], p[2] ? stripQ(p[2]) : undefined);
      continue;
    }
    const n = /^Note\s+(?:over|left of|right of)\s+([^:]+):\s*(.+)$/i.exec(line);
    if (n) {
      const who = n[1].split(",")[0].trim();
      add(who);
      messages.push({ from: who, to: who, label: n[2].trim(), note: true });
      continue;
    }
    const m = /^(\S+?)\s*(-->>|->>|-->|->)\s*(\S+)\s*:\s*(.*)$/.exec(line);
    if (m) {
      add(m[1]);
      add(m[3]);
      messages.push({ from: m[1], to: m[3], label: m[4].trim(), dashed: m[2].includes("--") });
    }
  }
  return actors.length && messages.length ? { kind: "seq", actors, messages } : null;
}

function parseFlow(src: string): Diagram | null {
  const lines = src.split("\n");
  const first = lines[0].trim();
  const dirM = /^(?:graph|flowchart)\s+(TD|TB|BT|LR|RL)\b/i.exec(first);
  const dir: "TD" | "LR" = dirM && /LR|RL/i.test(dirM[1]) ? "LR" : "TD";
  const nodes = new Map<string, FlowNode>();
  const edges: FlowEdge[] = [];

  const ensure = (id: string, label?: string, shape?: FlowNode["shape"]) => {
    const cur = nodes.get(id);
    if (!cur) nodes.set(id, { id, label: label || id, shape: shape || "rect" });
    else {
      if (label && label !== id) cur.label = label;
      if (shape) cur.shape = shape;
    }
  };

  for (let li = 0; li < lines.length; li++) {
    let line = lines[li].trim();
    if (!line || line.startsWith("%%")) continue;
    if (li === 0 && /^(?:graph|flowchart)\b/i.test(line)) continue;
    if (/^(?:subgraph|end|classDef|class|style|linkStyle|click)\b/.test(line)) continue;
    line = line.replace(/^subgraph\s+\S+\s*/, "");
    const bits = splitFlow(line);
    if (!bits) continue;
    const [pts, ops] = bits;
    for (const p of pts) ensure(p.id, p.label, p.shape);
    for (let i = 0; i < ops.length; i++) {
      const a = pts[i];
      const b = pts[i + 1];
      if (!a || !b) continue;
      edges.push({ from: a.id, to: b.id, label: ops[i].label, dashed: ops[i].dashed });
    }
  }
  if (!nodes.size) return null;
  return { kind: "flow", dir, nodes: [...nodes.values()], edges };
}

type FlowTok = { id: string; label?: string; shape?: FlowNode["shape"] };
type FlowOp = { label?: string; dashed?: boolean };

function splitFlow(line: string): [FlowTok[], FlowOp[]] | null {
  const pts: FlowTok[] = [];
  const ops: FlowOp[] = [];
  let rest = line;
  const first = readFlowNode(rest);
  if (!first) return null;
  pts.push(first.tok);
  rest = first.rest.trim();
  while (rest) {
    const op = readFlowEdge(rest);
    if (!op) break;
    const next = readFlowNode(op.rest.trim());
    if (!next) break;
    ops.push(op.op);
    pts.push(next.tok);
    rest = next.rest.trim();
  }
  return pts.length ? [pts, ops] : null;
}

function readFlowNode(s: string): { tok: FlowTok; rest: string } | null {
  const idm = /^([A-Za-z_][\w.-]*|[\u4e00-\u9fff][\w.\u4e00-\u9fff-]*)/.exec(s);
  if (!idm) return null;
  const id = idm[1];
  let rest = s.slice(idm[0].length);
  const sh = readShape(rest);
  if (sh) return { tok: { id, label: sh.label, shape: sh.shape }, rest: sh.rest };
  return { tok: { id }, rest };
}

function readShape(s: string): { label: string; shape: FlowNode["shape"]; rest: string } | null {
  const rules: Array<[RegExp, FlowNode["shape"]]> = [
    [/^\(\[([^\]]+)\]\)/, "stadium"],
    [/^\(\(([^)]+)\)\)/, "circle"],
    [/^\[\[([^\]]+)\]\]/, "rect"],
    [/^\["([^"]+)"\]/, "rect"],
    [/^\[([^\]]+)\]/, "rect"],
    [/^\(\s*"([^"]+)"\s*\)/, "round"],
    [/^\(([^)]+)\)/, "round"],
    [/^\{"([^"]+)"\}/, "diamond"],
    [/^\{([^}]+)\}/, "diamond"],
  ];
  for (const [re, shape] of rules) {
    const m = re.exec(s);
    if (m) return { label: m[1], shape, rest: s.slice(m[0].length) };
  }
  return null;
}

function readFlowEdge(s: string): { op: FlowOp; rest: string } | null {
  const labeled = /^(-->|-\.->|==>|---)\s*\|([^|]*)\|\s*/.exec(s);
  if (labeled) {
    return {
      op: { label: labeled[2].trim() || undefined, dashed: labeled[1].includes(".") },
      rest: s.slice(labeled[0].length),
    };
  }
  const text = /^--\s+(.+?)\s+-->/.exec(s);
  if (text) return { op: { label: text[1].trim() }, rest: s.slice(text[0].length) };
  const plain = /^(-\.->|-->|==>|---)\s*/.exec(s);
  if (plain) return { op: { dashed: plain[1].includes(".") }, rest: s.slice(plain[0].length) };
  return null;
}

function splitList(s: string): string[] {
  return s.split(",").map((x) => stripQ(x.trim())).filter(Boolean);
}
function stripQ(s: string) {
  const t = s.trim();
  if ((t.startsWith('"') && t.endsWith('"')) || (t.startsWith("'") && t.endsWith("'"))) return t.slice(1, -1);
  return t;
}

function textW(s: string) {
  let w = 0;
  for (const ch of s) w += ch.charCodeAt(0) > 255 ? 12 : 7.2;
  return w;
}

export function DiagramView({ d }: { d: Diagram }) {
  if (d.kind === "pie") return <PieView d={d} />;
  if (d.kind === "xy") return <XyView d={d} />;
  if (d.kind === "seq") return <SeqView d={d} />;
  return <FlowView d={d} />;
}

function PieView({ d }: { d: Extract<Diagram, { kind: "pie" }> }) {
  const uid = useId().replace(/:/g, "");
  const total = d.slices.reduce((a, s) => a + Math.max(0, s.value), 0) || 1;
  const cx = 72;
  const cy = 72;
  const r = 58;
  const ir = 34;
  let a = -Math.PI / 2;
  const arcs: ReactNode[] = [];
  d.slices.forEach((s, i) => {
    const sweep = (Math.max(0, s.value) / total) * Math.PI * 2;
    const a1 = a + sweep;
    arcs.push(
      <path key={i} d={donut(cx, cy, r, ir, a, a1)} fill={SERIES[i % SERIES.length]} />,
    );
    a = a1;
  });
  return (
    <div className="md-chart md-pie">
      {d.title ? <div className="md-chart-title">{d.title}</div> : null}
      <div className="md-pie-row">
        <svg viewBox="0 0 144 144" width="144" height="144" role="img" aria-label={d.title || "饼图"}>
          {arcs}
          <circle cx={cx} cy={cy} r={ir - 1} fill="var(--paper)" />
        </svg>
        <ul className="md-legend">
          {d.slices.map((s, i) => (
            <li key={`${uid}${i}`}>
              <i style={{ background: SERIES[i % SERIES.length] }} />
              <span>{s.label}</span>
              <b>{fmtNum(s.value)}</b>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}

function donut(cx: number, cy: number, r: number, ir: number, a0: number, a1: number) {
  if (a1 - a0 < 0.001) return "";
  const large = a1 - a0 > Math.PI ? 1 : 0;
  const p = (r: number, a: number) => [cx + r * Math.cos(a), cy + r * Math.sin(a)] as const;
  const [x0, y0] = p(r, a0);
  const [x1, y1] = p(r, a1);
  const [ix0, iy0] = p(ir, a0);
  const [ix1, iy1] = p(ir, a1);
  return `M${x0} ${y0} A${r} ${r} 0 ${large} 1 ${x1} ${y1} L${ix1} ${iy1} A${ir} ${ir} 0 ${large} 0 ${ix0} ${iy0} Z`;
}

function XyView({ d }: { d: Extract<Diagram, { kind: "xy" }> }) {
  const n = Math.max(d.labels.length, d.bars?.length || 0, ...d.lines.map((l) => l.values.length), 1);
  const labels = Array.from({ length: n }, (_, i) => d.labels[i] || String(i + 1));
  const nums = [...(d.bars || []), ...d.lines.flatMap((l) => l.values)].filter((x) => Number.isFinite(x));
  const max = Math.max(0, ...nums, 1);
  const W = Math.max(280, n * 48 + 48);
  const H = 180;
  const pad = { l: 36, r: 12, t: 12, b: 36 };
  const iw = W - pad.l - pad.r;
  const ih = H - pad.t - pad.b;
  const bw = (iw / n) * 0.62;
  const gap = iw / n;
  const y = (v: number) => pad.t + ih - (Math.max(0, v) / max) * ih;
  const ticks = [0, 0.5, 1].map((t) => max * t);
  return (
    <div className="md-chart">
      {d.title ? <div className="md-chart-title">{d.title}</div> : null}
      <svg viewBox={`0 0 ${W} ${H}`} className="md-xy" role="img" aria-label={d.title || "图表"} preserveAspectRatio="xMidYMid meet">
        {ticks.map((t, i) => (
          <g key={i}>
            <line x1={pad.l} x2={W - pad.r} y1={y(t)} y2={y(t)} className="md-grid" />
            <text x={pad.l - 6} y={y(t) + 3} className="md-axis" textAnchor="end">
              {fmtNum(t)}
            </text>
          </g>
        ))}
        {d.bars
          ? labels.map((lb, i) => {
              const v = d.bars![i] || 0;
              const x = pad.l + gap * i + (gap - bw) / 2;
              const top = y(v);
              return (
                <g key={lb + i}>
                  <rect x={x} y={top} width={bw} height={Math.max(0, pad.t + ih - top)} rx="3" fill={SERIES[0]} />
                  <text x={x + bw / 2} y={H - 10} className="md-axis" textAnchor="middle">
                    {clip(lb, 8)}
                  </text>
                </g>
              );
            })
          : labels.map((lb, i) => (
              <text key={lb + i} x={pad.l + gap * i + gap / 2} y={H - 10} className="md-axis" textAnchor="middle">
                {clip(lb, 8)}
              </text>
            ))}
        {d.lines.map((ln, si) => {
          const pts = ln.values
            .map((v, i) => `${pad.l + gap * i + gap / 2},${y(v)}`)
            .join(" ");
          return (
            <g key={ln.name + si}>
              <polyline points={pts} fill="none" stroke={SERIES[(si + 1) % SERIES.length]} strokeWidth="2" />
              {ln.values.map((v, i) => (
                <circle
                  key={i}
                  cx={pad.l + gap * i + gap / 2}
                  cy={y(v)}
                  r="3"
                  fill={SERIES[(si + 1) % SERIES.length]}
                />
              ))}
            </g>
          );
        })}
      </svg>
    </div>
  );
}

function FlowView({ d }: { d: Extract<Diagram, { kind: "flow" }> }) {
  const uid = useId().replace(/:/g, "");
  const laid = layoutFlow(d);
  return (
    <div className="md-chart md-flow">
      <svg
        viewBox={`0 0 ${laid.w} ${laid.h}`}
        width={laid.w}
        height={laid.h}
        role="img"
        aria-label="流程图"
        className="md-flow-svg"
      >
        <defs>
          <marker id={`${uid}arr`} viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto">
            <path d="M0 0L10 5L0 10z" fill="var(--label-3)" />
          </marker>
        </defs>
        {laid.edges.map((e, i) => (
          <g key={i}>
            <path
              d={e.d}
              fill="none"
              stroke="var(--label-3)"
              strokeWidth="1.4"
              strokeDasharray={e.dashed ? "5 4" : undefined}
              markerEnd={`url(#${uid}arr)`}
            />
            {e.label ? (
              <text x={e.lx} y={e.ly} className="md-edge" textAnchor="middle">
                {e.label}
              </text>
            ) : null}
          </g>
        ))}
        {laid.nodes.map((n) => (
          <g key={n.id} transform={`translate(${n.x},${n.y})`}>
            {flowShape(n)}
            <text x={n.w / 2} y={n.h / 2 + 4} textAnchor="middle" className="md-node">
              {clip(n.label, 18)}
            </text>
          </g>
        ))}
      </svg>
    </div>
  );
}

function flowShape(n: { w: number; h: number; shape: FlowNode["shape"] }) {
  const cls = "md-node-shape";
  if (n.shape === "diamond") {
    const p = `${n.w / 2},0 ${n.w},${n.h / 2} ${n.w / 2},${n.h} 0,${n.h / 2}`;
    return <polygon points={p} className={cls} />;
  }
  if (n.shape === "circle") return <ellipse cx={n.w / 2} cy={n.h / 2} rx={n.w / 2} ry={n.h / 2} className={cls} />;
  const r = n.shape === "stadium" ? n.h / 2 : n.shape === "round" ? 10 : 6;
  return <rect x={0} y={0} width={n.w} height={n.h} rx={r} className={cls} />;
}

function layoutFlow(d: Extract<Diagram, { kind: "flow" }>) {
  const ids = d.nodes.map((n) => n.id);
  const idx = new Map(ids.map((id, i) => [id, i]));
  const out: number[][] = ids.map(() => []);
  const indeg = ids.map(() => 0);
  for (const e of d.edges) {
    const a = idx.get(e.from);
    const b = idx.get(e.to);
    if (a == null || b == null || a === b) continue;
    out[a].push(b);
    indeg[b]++;
  }
  const rank = ids.map(() => 0);
  const q = indeg.map((n, i) => (n === 0 ? i : -1)).filter((i) => i >= 0);
  const seen = new Set(q);
  while (q.length) {
    const i = q.shift()!;
    for (const j of out[i]) {
      rank[j] = Math.max(rank[j], rank[i] + 1);
      if (!seen.has(j)) {
        seen.add(j);
        q.push(j);
      }
    }
  }
  const maxR = Math.max(0, ...rank);
  const layers: number[][] = Array.from({ length: maxR + 1 }, () => []);
  rank.forEach((r, i) => layers[r].push(i));
  for (let pass = 0; pass < 2; pass++) {
    for (let r = 1; r < layers.length; r++) {
      layers[r].sort((a, b) => bary(a, layers[r - 1], d, true) - bary(b, layers[r - 1], d, true));
    }
  }
  const sized = d.nodes.map((n) => {
    const tw = textW(n.label) + 28;
    const w = n.shape === "diamond" ? Math.max(84, tw + 12) : Math.min(200, Math.max(72, tw));
    const h = n.shape === "diamond" ? 52 : n.shape === "circle" ? Math.max(40, Math.min(64, w * 0.55)) : 32;
    return { ...n, w, h, x: 0, y: 0 };
  });
  const pad = 16;
  const rankGap = 44;
  const nodeGap = 20;
  let maxW = 0;
  let maxH = 0;
  if (d.dir === "TD") {
    let y = pad;
    for (const layer of layers) {
      const lh = Math.max(...layer.map((i) => sized[i].h), 32);
      const lw = layer.reduce((a, i) => a + sized[i].w, 0) + nodeGap * Math.max(0, layer.length - 1);
      let x = pad;
      for (const i of layer) {
        sized[i].x = x;
        sized[i].y = y + (lh - sized[i].h) / 2;
        x += sized[i].w + nodeGap;
      }
      maxW = Math.max(maxW, lw);
      y += lh + rankGap;
      maxH = y;
    }
  } else {
    let x = pad;
    for (const layer of layers) {
      const lw = Math.max(...layer.map((i) => sized[i].w), 72);
      const lh = layer.reduce((a, i) => a + sized[i].h, 0) + nodeGap * Math.max(0, layer.length - 1);
      let y = pad;
      for (const i of layer) {
        sized[i].x = x + (lw - sized[i].w) / 2;
        sized[i].y = y;
        y += sized[i].h + nodeGap;
      }
      maxH = Math.max(maxH, lh);
      x += lw + rankGap;
      maxW = x;
    }
  }
  const w = maxW + pad * 2;
  const h = maxH + pad;
  const edges = d.edges.map((e) => {
    const a = sized.find((n) => n.id === e.from);
    const b = sized.find((n) => n.id === e.to);
    if (!a || !b) return { d: "", dashed: e.dashed, label: e.label, lx: 0, ly: 0 };
    const { x1, y1, x2, y2 } = ports(a, b, d.dir);
    const dy = d.dir === "TD" ? Math.max(24, (y2 - y1) / 2) : 0;
    const dx = d.dir === "LR" ? Math.max(24, (x2 - x1) / 2) : 0;
    const path =
      d.dir === "TD"
        ? `M${x1} ${y1} C${x1} ${y1 + dy}, ${x2} ${y2 - dy}, ${x2} ${y2}`
        : `M${x1} ${y1} C${x1 + dx} ${y1}, ${x2 - dx} ${y2}, ${x2} ${y2}`;
    return { d: path, dashed: e.dashed, label: e.label, lx: (x1 + x2) / 2, ly: (y1 + y2) / 2 - 4 };
  });
  return { w, h, nodes: sized, edges };
}

function bary(i: number, prev: number[], d: Extract<Diagram, { kind: "flow" }>, fromPrev: boolean) {
  const id = d.nodes[i].id;
  const hits: number[] = [];
  prev.forEach((j, pos) => {
    const oid = d.nodes[j].id;
    const linked = fromPrev
      ? d.edges.some((e) => e.from === oid && e.to === id)
      : d.edges.some((e) => e.from === id && e.to === oid);
    if (linked) hits.push(pos);
  });
  if (!hits.length) return prev.length / 2;
  return hits.reduce((a, b) => a + b, 0) / hits.length;
}

function ports(
  a: { x: number; y: number; w: number; h: number },
  b: { x: number; y: number; w: number; h: number },
  dir: "TD" | "LR",
) {
  if (dir === "TD") {
    return { x1: a.x + a.w / 2, y1: a.y + a.h, x2: b.x + b.w / 2, y2: b.y };
  }
  return { x1: a.x + a.w, y1: a.y + a.h / 2, x2: b.x, y2: b.y + b.h / 2 };
}

function SeqView({ d }: { d: Extract<Diagram, { kind: "seq" }> }) {
  const col = 132;
  const top = 36;
  const row = 36;
  const w = Math.max(280, d.actors.length * col + 24);
  const h = top + d.messages.length * row + 28;
  return (
    <div className="md-chart md-seq">
      <svg viewBox={`0 0 ${w} ${h}`} width={w} height={h} role="img" aria-label="时序图" className="md-flow-svg">
        {d.actors.map((a, i) => {
          const x = 24 + i * col + col / 2;
          return (
            <g key={a.id}>
              <rect x={x - 46} y={8} width="92" height="22" rx="6" className="md-node-shape" />
              <text x={x} y={24} textAnchor="middle" className="md-node">
                {clip(a.label, 12)}
              </text>
              <line x1={x} x2={x} y1={30} y2={h - 10} className="md-life" />
            </g>
          );
        })}
        {d.messages.map((m, i) => {
          const fi = Math.max(0, d.actors.findIndex((a) => a.id === m.from));
          const ti = Math.max(0, d.actors.findIndex((a) => a.id === m.to));
          const x1 = 24 + fi * col + col / 2;
          const x2 = 24 + ti * col + col / 2;
          const y = top + i * row + 16;
          if (m.note || fi === ti) {
            return (
              <g key={i}>
                <rect x={Math.min(x1, x2) - 50} y={y - 12} width="100" height="22" rx="4" className="md-note" />
                <text x={(x1 + x2) / 2} y={y + 4} textAnchor="middle" className="md-edge">
                  {clip(m.label, 16)}
                </text>
              </g>
            );
          }
          const left = x1 < x2;
          return (
            <g key={i}>
              <line
                x1={x1}
                y1={y}
                x2={x2}
                y2={y}
                stroke="var(--label-2)"
                strokeWidth="1.3"
                strokeDasharray={m.dashed ? "5 4" : undefined}
              />
              <polygon
                points={left ? `${x2},${y} ${x2 - 8},${y - 4} ${x2 - 8},${y + 4}` : `${x2},${y} ${x2 + 8},${y - 4} ${x2 + 8},${y + 4}`}
                fill="var(--label-2)"
              />
              <text x={(x1 + x2) / 2} y={y - 6} textAnchor="middle" className="md-edge">
                {clip(m.label, 22)}
              </text>
            </g>
          );
        })}
      </svg>
    </div>
  );
}

function fmtNum(n: number) {
  if (!Number.isFinite(n)) return "0";
  if (Math.abs(n) >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (Math.abs(n) >= 10_000) return `${(n / 1000).toFixed(1)}k`;
  if (Number.isInteger(n)) return String(n);
  return n.toFixed(1).replace(/\.0$/, "");
}
function clip(s: string, n: number) {
  const cs = [...s];
  return cs.length <= n ? s : `${cs.slice(0, n - 1).join("")}…`;
}
