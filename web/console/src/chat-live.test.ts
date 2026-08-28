import assert from "node:assert/strict";
import {
  applyHistoryIncoming,
  coversLive,
  lastAssistantContent,
  lastAssistantInCurrentTurn,
  preferFresherHistory,
  stripLeakedToolMarkup,
  stripThinkRestatement,
} from "./chat-live.ts";
import type { SessionEvent } from "./api.ts";

const user: SessionEvent = { type: "user", text: "把登录页标题改成 ixiaotao" };
const asst: SessionEvent = { type: "assistant", content: "已改好标题。" };

assert.equal(lastAssistantContent([user, asst]), "已改好标题。");
assert.equal(lastAssistantContent([user]), "");
assert.equal(lastAssistantInCurrentTurn([user, asst]), "已改好标题。");
assert.equal(lastAssistantInCurrentTurn([user]), "");

{
  const live = { think: "", content: "已改好标题。" };
  assert.equal(coversLive([user, asst], live), true);
  assert.equal(coversLive([user], live), false);
  assert.equal(coversLive([user, asst], { think: "x", content: "" }), true);
  assert.equal(
    coversLive([user, { type: "assistant", content: "已改" }], { think: "", content: "已改好标题。" }),
    false,
  );
}

{
  const cur = [user, asst];
  assert.deepEqual(preferFresherHistory(cur, [user]), cur);
  assert.deepEqual(preferFresherHistory(cur, []), cur);
  const later = [user, { type: "assistant", content: "已改好标题。并保存。" }];
  assert.deepEqual(preferFresherHistory(cur, later), later);
}

{
  const qwen = [user, { type: "assistant", content: "先确认上传文件是否存在。" }];
  assert.deepEqual(preferFresherHistory(qwen, []), qwen);
  assert.deepEqual(applyHistoryIncoming(qwen, [], true), []);
  assert.deepEqual(applyHistoryIncoming(qwen, [], false), qwen);
}

{
  const t1u: SessionEvent = { type: "user", text: "先看 Cargo.toml" };
  const t1a: SessionEvent = { type: "assistant", content: "依赖已经列出来了。" };
  const t2u: SessionEvent = { type: "user", text: "把登录页标题改成 ixiaotao" };
  const prior = [t1u, t1a, t2u];
  const live = { think: "", content: "已改好标题。" };
  assert.equal(lastAssistantInCurrentTurn(prior), "");
  assert.equal(lastAssistantContent(prior), "依赖已经列出来了。");
  assert.equal(coversLive(prior, live), false);
  assert.deepEqual(preferFresherHistory(prior, prior), prior);
}

assert.equal(
  stripLeakedToolMarkup(
    "先确认上传文件是否存在。\n\n<tool_calls>\n</tool_calls>\n\n<tool_result>\n</tool_result>\n\n文件存在，读取内容。",
  ),
  "先确认上传文件是否存在。",
);
assert.equal(stripLeakedToolMarkup("正常回复，没有工具标记。"), "正常回复，没有工具标记。");

assert.equal(
  stripThinkRestatement("把登录页标题改成 ixiaotao", "用户想把登录页标题改成 ixiaotao。\n先改 auth.tsx。"),
  "先改 auth.tsx。",
);
assert.equal(
  stripThinkRestatement("fix the login title", "The user wants me to fix the login title. I'll open auth.tsx."),
  "I'll open auth.tsx.",
);
assert.equal(
  stripThinkRestatement("修 paging", "auth.rs 里 page_bounds 的起算是 1-based。"),
  "auth.rs 里 page_bounds 的起算是 1-based。",
);
