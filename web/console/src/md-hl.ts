/** Tiny syntax highlighter. Tokens become React class names, never HTML. */

export type HlTok = { t: string; k?: string };

const ALIAS: Record<string, string> = {
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  jsx: "javascript",
  ts: "typescript",
  tsx: "typescript",
  py: "python",
  rs: "rust",
  sh: "bash",
  zsh: "bash",
  shell: "bash",
  yml: "yaml",
  kt: "kotlin",
  cs: "csharp",
  "c++": "cpp",
  cc: "cpp",
  hpp: "cpp",
  h: "c",
  md: "markdown",
};

const KW: Record<string, Set<string>> = {
  rust: s(
    "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while type Self",
  ),
  python: s(
    "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield",
  ),
  javascript: s(
    "async await break case catch class const continue debugger default delete do else export extends false finally for from function get if import in instanceof let new null of return set static super switch this throw true try typeof var void while yield",
  ),
  typescript: s(
    "abstract as async await break case catch class const continue debugger default delete do else enum export extends false finally for from function get if implements import in infer instanceof interface keyof let new null of private protected public readonly return set static super switch this throw true try type typeof var void while yield",
  ),
  go: s(
    "break case chan const continue default defer else fallthrough false for func go goto if import interface map nil package range return select struct switch true type var",
  ),
  bash: s(
    "alias break case continue do done elif else esac export fi for function if in return then until while true false",
  ),
  sql: s(
    "add all alter and as asc between by case cast create delete desc distinct drop else end exists false from full group having in inner insert into is join left like limit not null on or order outer right select set table then true union update values when where",
  ),
  toml: s("true false"),
  yaml: s("true false null yes no on off"),
  kotlin: s(
    "abstract as break by catch class companion const constructor continue crossinline data else enum false finally for fun if in inner interface internal is lateinit null object open operator override private protected public return super this throw true try val var when where while",
  ),
  csharp: s(
    "abstract as async await base bool break byte case catch char checked class const continue decimal default delegate do double else enum event explicit extern false finally fixed float for foreach goto if implicit in int interface internal is lock long namespace new null object operator out override params private protected public readonly ref return sbyte sealed short sizeof stackalloc static string struct switch this throw true try typeof uint ulong unchecked unsafe ushort using var virtual void volatile while",
  ),
  cpp: s(
    "alignas alignof and and_eq asm auto bitand bitor bool break case catch char class compl concept const consteval constexpr constinit continue co_await co_return co_yield decltype default delete do double dynamic_cast else enum explicit export extern false float for friend goto if inline int long mutable namespace new noexcept not not_eq nullptr operator or or_eq private protected public register reinterpret_cast requires return short signed sizeof static static_assert static_cast struct switch template this thread_local throw true try typedef typeid typename union unsigned using virtual void volatile wchar_t while xor xor_eq",
  ),
  c: s(
    "auto break case char const continue default do double else enum extern float for goto if inline int long register restrict return short signed sizeof static struct switch typedef union unsigned void volatile while _Bool _Complex _Imaginary true false NULL",
  ),
};

KW.typescript = new Set([...KW.javascript, ...KW.typescript]);

function s(words: string): Set<string> {
  return new Set(words.split(/\s+/).filter(Boolean));
}

export function normLang(raw: string): string {
  const l = raw.trim().toLowerCase().split(/[\s{]/)[0] || "";
  return ALIAS[l] || l;
}

export function highlight(code: string, langRaw: string): HlTok[] {
  if (code.length > 80_000) return [{ t: code }];
  const lang = normLang(langRaw);
  if (lang === "diff") return highlightDiff(code);
  if (lang === "json") return highlightJson(code);
  if (lang === "html" || lang === "xml" || lang === "svg") return highlightMarkup(code);
  if (lang === "markdown") return highlightMd(code);
  return highlightCLike(code, lang);
}

function push(out: HlTok[], t: string, k?: string) {
  if (!t) return;
  const last = out[out.length - 1];
  if (last && last.k === k) last.t += t;
  else out.push(k ? { t, k } : { t });
}

function highlightDiff(code: string): HlTok[] {
  const out: HlTok[] = [];
  const lines = code.split(/(\n)/);
  for (const line of lines) {
    if (line === "\n") {
      push(out, line);
      continue;
    }
    if (line.startsWith("+++") || line.startsWith("---") || line.startsWith("diff") || line.startsWith("index") || line.startsWith("@@")) {
      push(out, line, "meta");
    } else if (line.startsWith("+")) push(out, line, "add");
    else if (line.startsWith("-")) push(out, line, "del");
    else push(out, line);
  }
  return out;
}

function highlightJson(code: string): HlTok[] {
  const out: HlTok[] = [];
  let i = 0;
  const n = code.length;
  while (i < n) {
    const c = code[i];
    if (c === '"' ) {
      const { s, end } = readString(code, i);
      const rest = skipWs(code, end);
      push(out, s, rest < code.length && code[rest] === ":" ? "key" : "str");
      i = end;
      continue;
    }
    if (c === "/" && code[i + 1] === "/") {
      const j = code.indexOf("\n", i);
      const end = j < 0 ? n : j;
      push(out, code.slice(i, end), "cmt");
      i = end;
      continue;
    }
    if (isDigit(c) || (c === "-" && isDigit(code[i + 1]))) {
      const j = readNum(code, i);
      push(out, code.slice(i, j), "num");
      i = j;
      continue;
    }
    if (c === "t" || c === "f" || c === "n") {
      const lit = ["true", "false", "null"].find((w) => code.startsWith(w, i) && !isIdent(code[i + w.length]));
      if (lit) {
        push(out, lit, "kw");
        i += lit.length;
        continue;
      }
    }
    push(out, c, "{}[],:".includes(c) ? "punc" : undefined);
    i++;
  }
  return out;
}

function highlightMarkup(code: string): HlTok[] {
  const out: HlTok[] = [];
  let i = 0;
  const n = code.length;
  while (i < n) {
    if (code.startsWith("<!--", i)) {
      const j = code.indexOf("-->", i + 4);
      const end = j < 0 ? n : j + 3;
      push(out, code.slice(i, end), "cmt");
      i = end;
      continue;
    }
    if (code[i] === "<") {
      const j = code.indexOf(">", i);
      const end = j < 0 ? n : j + 1;
      const tag = code.slice(i, end);
      highlightTag(out, tag);
      i = end;
      continue;
    }
    const j = code.indexOf("<", i);
    const end = j < 0 ? n : j;
    push(out, code.slice(i, end));
    i = end;
  }
  return out;
}

function highlightTag(out: HlTok[], tag: string) {
  const m = /^<\/?([A-Za-z][\w:-]*)/.exec(tag);
  if (!m) {
    push(out, tag, "punc");
    return;
  }
  const head = m[0];
  push(out, head.slice(0, head.length - m[1].length), "punc");
  push(out, m[1], "kw");
  let i = head.length;
  while (i < tag.length) {
    const sp = /^[\s/]*/.exec(tag.slice(i));
    if (sp && sp[0]) {
      push(out, sp[0], /[/>]/.test(sp[0]) ? "punc" : undefined);
      i += sp[0].length;
    }
    const attr = /^([\w:-]+)(\s*=\s*)?/.exec(tag.slice(i));
    if (!attr) {
      push(out, tag.slice(i), "punc");
      return;
    }
    push(out, attr[1], "key");
    i += attr[1].length;
    if (attr[2]) {
      push(out, attr[2], "punc");
      i += attr[2].length;
      if (tag[i] === '"' || tag[i] === "'") {
        const { s, end } = readString(tag, i);
        push(out, s, "str");
        i = end;
      }
    }
  }
}

function highlightMd(code: string): HlTok[] {
  const out: HlTok[] = [];
  for (const line of code.split(/(\n)/)) {
    if (line === "\n") {
      push(out, line);
      continue;
    }
    if (/^#{1,6}\s/.test(line)) push(out, line, "kw");
    else if (/^>\s?/.test(line)) push(out, line, "cmt");
    else if (/^```/.test(line)) push(out, line, "meta");
    else if (/^\s*[-*+]\s/.test(line) || /^\s*\d+[.)]\s/.test(line)) push(out, line, "fn");
    else push(out, line);
  }
  return out;
}

function highlightCLike(code: string, lang: string): HlTok[] {
  const keys = KW[lang];
  const hash = lang === "python" || lang === "bash" || lang === "yaml" || lang === "toml" || lang === "ruby";
  const sql = lang === "sql";
  const out: HlTok[] = [];
  let i = 0;
  const n = code.length;
  while (i < n) {
    const c = code[i];
    const two = code.slice(i, i + 2);
    if (two === "//" || (sql && two.toLowerCase() === "--") || (hash && c === "#")) {
      const start = i;
      if (two === "//") i += 2;
      else if (sql && two === "--") i += 2;
      else i += 1;
      while (i < n && code[i] !== "\n") i++;
      push(out, code.slice(start, i), "cmt");
      continue;
    }
    if (two === "/*") {
      const j = code.indexOf("*/", i + 2);
      const end = j < 0 ? n : j + 2;
      push(out, code.slice(i, end), "cmt");
      i = end;
      continue;
    }
    if (lang === "python" && (two === '"""' || two === "'''")) {
      const q = two;
      const j = code.indexOf(q, i + 3);
      const end = j < 0 ? n : j + 3;
      push(out, code.slice(i, end), "str");
      i = end;
      continue;
    }
    if (c === "`" && (lang === "javascript" || lang === "typescript" || lang === "bash")) {
      const j = endTemplate(code, i);
      push(out, code.slice(i, j), "str");
      i = j;
      continue;
    }
    if (c === '"' || c === "'") {
      const { s: str, end } = readString(code, i);
      push(out, str, "str");
      i = end;
      continue;
    }
    if (isDigit(c) || (c === "." && isDigit(code[i + 1]))) {
      const j = readNum(code, i);
      push(out, code.slice(i, j), "num");
      i = j;
      continue;
    }
    if (isIdentStart(c)) {
      let j = i + 1;
      while (j < n && isIdent(code[j])) j++;
      const word = code.slice(i, j);
      const nextNonWs = skipWs(code, j);
      if (keys && keys.has(sql ? word.toLowerCase() : word)) push(out, word, "kw");
      else if (code[nextNonWs] === "(") push(out, word, "fn");
      else if (/^[A-Z][\w]*$/.test(word) && word.length > 1) push(out, word, "type");
      else push(out, word);
      i = j;
      continue;
    }
    push(out, c, "(){}[],.;:?<>+-*/%=!&|^~".includes(c) ? "punc" : undefined);
    i++;
  }
  return out;
}

function isDigit(c: string | undefined) {
  return !!c && c >= "0" && c <= "9";
}
function isIdentStart(c: string) {
  return (c >= "A" && c <= "Z") || (c >= "a" && c <= "z") || c === "_" || c === "$" || c.charCodeAt(0) > 127;
}
function isIdent(c: string | undefined) {
  return !!c && (isIdentStart(c) || isDigit(c));
}
function skipWs(s: string, i: number) {
  while (i < s.length && (s[i] === " " || s[i] === "\t" || s[i] === "\n" || s[i] === "\r")) i++;
  return i;
}
function readNum(s: string, i: number) {
  let j = i;
  if (s[j] === "-") j++;
  while (j < s.length && (isDigit(s[j]) || s[j] === "_" || s[j] === "." || s[j] === "x" || s[j] === "b" || s[j] === "o" || (s[j] >= "a" && s[j] <= "f") || (s[j] >= "A" && s[j] <= "F"))) j++;
  if (s[j] === "e" || s[j] === "E") {
    j++;
    if (s[j] === "+" || s[j] === "-") j++;
    while (j < s.length && isDigit(s[j])) j++;
  }
  return j;
}
function readString(s: string, i: number): { s: string; end: number } {
  const q = s[i];
  let j = i + 1;
  while (j < s.length) {
    if (s[j] === "\\") {
      j += 2;
      continue;
    }
    if (s[j] === q) {
      j++;
      break;
    }
    if (s[j] === "\n" && q !== "`") break;
    j++;
  }
  return { s: s.slice(i, j), end: j };
}
function endTemplate(s: string, i: number) {
  let j = i + 1;
  while (j < s.length) {
    if (s[j] === "\\") {
      j += 2;
      continue;
    }
    if (s[j] === "`") return j + 1;
    j++;
  }
  return s.length;
}
