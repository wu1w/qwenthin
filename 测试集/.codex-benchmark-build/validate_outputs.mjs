import fs from "node:fs/promises";
import path from "node:path";

const dir = "E:/项目/测试集/outputs/qwen38_agent_benchmark_v1";
async function readJsonl(name) {
  const text = await fs.readFile(path.join(dir, name), "utf8");
  return text.trim().split(/\r?\n/).filter(Boolean).map((line, i) => {
    try { return JSON.parse(line); }
    catch (e) { throw new Error(`${name}:${i + 1}: ${e.message}`); }
  });
}

const pub = await readJsonl("tasks_public.jsonl");
const priv = await readJsonl("evaluator_private.jsonl");
const fixtures = await readJsonl("fixtures_manifest.jsonl");
if (pub.length !== 152 || priv.length !== 152 || fixtures.length !== 100) throw new Error("line count mismatch");
const ids = pub.map((x) => x.id);
if (new Set(ids).size !== 152) throw new Error("duplicate public IDs");
if (new Set(priv.map((x) => x.id)).size !== 152) throw new Error("duplicate private IDs");
if (ids.some((id) => !priv.some((x) => x.id === id))) throw new Error("public/private ID mismatch");

const counts = Object.fromEntries([...new Set(pub.map((x) => x.suite))].map((s) => [s, pub.filter((x) => x.suite === s).length]));
const expected = { simple_code: 30, complex_code: 50, long_code: 20, math: 20, philosophy: 20, context_tools: 12 };
if (JSON.stringify(counts) !== JSON.stringify(expected)) throw new Error(`suite counts mismatch ${JSON.stringify(counts)}`);

const serializedPublic = JSON.stringify(pub);
for (const forbidden of ["root_cause", "hidden_tests", "required_files", "forbidden_changes", "target_files", "seed_fault"]) {
  if (serializedPublic.includes(`\"${forbidden}\"`)) throw new Error(`private field leaked: ${forbidden}`);
}
if (pub.some((x) => !x.task_prompt || x.task_prompt.length < 40)) throw new Error("short/missing prompt");
if (pub.filter((x) => x.suite.endsWith("code")).some((x) => !x.metrics.includes("retrieval_precision_useful_lines/returned_lines"))) throw new Error("code retrieval metric missing");
if (pub.filter((x) => x.suite === "context_tools").some((x) => !Array.isArray(x.turn_script))) throw new Error("context script missing");
if (priv.find((x) => x.id === "M002")?.answer !== "049") throw new Error("M002 leading zero oracle damaged");
if (priv.find((x) => x.id === "M007")?.answer !== "32/3") throw new Error("M007 oracle wrong");
if (priv.find((x) => x.id === "M018")?.answer !== "A真，B假") throw new Error("M018 oracle wrong");

const stat = await fs.stat(path.join(dir, "qwen38_agent_benchmark_v1.xlsx"));
if (stat.size < 50000) throw new Error("xlsx unexpectedly small");
const readme = await fs.readFile(path.join(dir, "README.md"), "utf8");
if (!readme.includes("总计 **152**") || !readme.includes("代码任务合计 **100**")) throw new Error("README counts missing");

console.log(JSON.stringify({ ok: true, public_tasks: pub.length, private_oracles: priv.length, fixtures: fixtures.length, counts, xlsx_bytes: stat.size }));
