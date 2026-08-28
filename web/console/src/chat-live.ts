import type { SessionEvent } from "./api.ts";

export type LiveBuf = { think: string; content: string };

export function lastAssistantContent(events: SessionEvent[]): string {
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    if (e.type === "assistant" && (e.content || "").trim()) return e.content || "";
  }
  return "";
}

/**
 * Assistant text after the latest user turn. A previous turn's reply must not
 * count as covering the live buffer of the turn still on screen.
 */
export function lastAssistantInCurrentTurn(events: SessionEvent[]): string {
  let lastUser = -1;
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i].type === "user") {
      lastUser = i;
      break;
    }
  }
  const start = lastUser + 1;
  for (let i = events.length - 1; i >= start; i--) {
    const e = events[i];
    if (e.type === "assistant" && (e.content || "").trim()) return e.content || "";
  }
  return "";
}

function controlSig(events: SessionEvent[]): { n: number; compactUntil: number; undoUntil: number } {
  let n = 0;
  let compactUntil = -1;
  let undoUntil = -1;
  for (const e of events) {
    if (e.type === "session/compact") {
      n++;
      compactUntil = Math.max(compactUntil, Number(e.until_seq ?? 0));
    } else if (e.type === "session/undo") {
      n++;
      undoUntil = Math.max(undoUntil, Number(e.until_seq ?? 0));
    }
  }
  return { n, compactUntil, undoUntil };
}

function incomingHasNewerControl(current: SessionEvent[], incoming: SessionEvent[]): boolean {
  const a = controlSig(current);
  const b = controlSig(incoming);
  if (b.n > a.n) return true;
  if (b.n < a.n) return false;
  return b.compactUntil > a.compactUntil || b.undoUntil > a.undoUntil;
}

/**
 * Keep the on-screen stream if the refetch is missing the just-finished reply.
 * Compact/undo rewrites are the source of truth even when the live transcript
 * still has the evicted assistant. Never use this across a session switch —
 * empty incoming history would keep the previous transcript on screen.
 */
export function preferFresherHistory(current: SessionEvent[], incoming: SessionEvent[]): SessionEvent[] {
  if (!incoming.length && current.length) return current;
  if (incomingHasNewerControl(current, incoming)) return incoming;
  const a = lastAssistantInCurrentTurn(current);
  const b = lastAssistantInCurrentTurn(incoming);
  if (a && !b) return current;
  return incoming;
}

/** Session swap: take incoming even when it is empty. */
export function applyHistoryIncoming(
  current: SessionEvent[],
  incoming: SessionEvent[],
  reset: boolean,
): SessionEvent[] {
  return reset ? incoming : preferFresherHistory(current, incoming);
}

/**
 * True when committed events already contain the streamed answer, so the live
 * overlay can be dropped without a blank transcript.
 */
export function coversLive(events: SessionEvent[], live: LiveBuf): boolean {
  if (!live.content) return true;
  const a = lastAssistantInCurrentTurn(events);
  if (!a) return false;
  if (a.includes(live.content)) return true;
  // Stream ran a few tokens ahead of the commit — keep the overlay.
  if (live.content.startsWith(a)) return false;
  return a.length > 0;
}

export function nextLive(events: SessionEvent[], live: LiveBuf): LiveBuf {
  return coversLive(events, live) ? { think: "", content: "" } : live;
}

function firstSentence(s: string): { head: string; rest: string } {
  const t = s.trim();
  const m = t.match(/^(.+?(?:。|！|？|\. |!\s|\?\s|\n))([\s\S]*)$/);
  if (!m) return { head: t, rest: "" };
  return { head: m[1].trim(), rest: m[2].trim() };
}

function fold(s: string): string {
  return s.replace(/\s+/g, " ").trim().toLowerCase();
}

function isRestatementSentence(user: string, sentence: string): boolean {
  const u = user.trim();
  const s = sentence.trim();
  if (!u || !s) return false;
  if (s === u) return true;
  const sl = fold(s);
  const ul = fold(u);
  if (ul.length >= 4 && sl.includes(ul) && [...s].length < [...u].length + 48) return true;
  const prefixed =
    sl.startsWith("the user ") ||
    sl.startsWith("user wants") ||
    sl.startsWith("user asked") ||
    sl.startsWith("the task ") ||
    sl.startsWith("the user's ") ||
    s.startsWith("用户") ||
    s.startsWith("好的，用户");
  if (!prefixed) return false;
  if ([...s].length <= 96) return true;
  const chunk = ul.slice(0, 24);
  return chunk.length >= 4 && sl.includes(chunk);
}

/**
 * Qwen 27B often opens thinking with "The user wants…". Drop that preamble
 * from what we show; leave the rest of the chain of thought.
 */
export function stripThinkRestatement(user: string, think: string): string {
  let rest = think.trim();
  if (!user.trim() || !rest) return rest;
  for (let i = 0; i < 4 && rest; i++) {
    const { head, rest: tail } = firstSentence(rest);
    if (!isRestatementSentence(user, head)) break;
    rest = tail;
  }
  return rest;
}

const TOOL_MARKUP = ["<tool_calls>", "<tool_call>", "<tool_results>", "<tool_result>"] as const;

/** Same scan as Rust `in_markdown_code`: tags inside `…` or ```…``` are citations. */
function inMarkdownCode(text: string, at: number): boolean {
  let fence = false;
  let inline = false;
  for (let i = 0; i < at; i++) {
    if (!inline && text.startsWith("```", i)) {
      fence = !fence;
      i += 2;
      continue;
    }
    if (!fence && text[i] === "`") inline = !inline;
  }
  return fence || inline;
}

function markupAt(text: string, open: string, from: number): number {
  let i = from;
  while (i < text.length) {
    const at = text.indexOf(open, i);
    if (at < 0) return -1;
    if (open === "<tool_call>" && text.startsWith("<tool_calls>", at)) {
      i = at + 1;
      continue;
    }
    if (open === "<tool_result>" && text.startsWith("<tool_results>", at)) {
      i = at + 1;
      continue;
    }
    if (inMarkdownCode(text, at)) {
      i = at + 1;
      continue;
    }
    return at;
  }
  return -1;
}

/**
 * Flash-Next sometimes dumps empty `<tool_calls>` / `<tool_result>` into the
 * visible reply. Cut at the first real markup so it never paints as chat text.
 */
export function stripLeakedToolMarkup(text: string): string {
  if (!text) return text;
  let cut = -1;
  for (const open of TOOL_MARKUP) {
    const at = markupAt(text, open, 0);
    if (at < 0) continue;
    if (cut < 0 || at < cut) cut = at;
  }
  if (cut < 0) return text;
  return text.slice(0, cut).trimEnd();
}
