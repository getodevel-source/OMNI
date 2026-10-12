#!/usr/bin/env node
// Deterministic, read-only quality checks against an already-running OMNI model.
// Does not start or stop the engine. Exact prompts are padded and tokenized by
// the loaded llama.cpp instance so each model sees the requested input depth.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

const args = process.argv.slice(2);
function option(name, fallback) {
  const i = args.indexOf(name);
  return i >= 0 && i + 1 < args.length ? args[i + 1] : fallback;
}
const BASE = (option("--base-url", "http://127.0.0.1:17860") || "").replace(/\/+$/, "");
const TARGET = Number(option("--prompt-tokens", "2048"));
const MAX_TOKENS = Number(option("--max-tokens", "128"));
const REASONING_EFFORT = option("--reasoning-effort", "medium").toLowerCase();
const OUT = option("--out", "tests/quality-results");
const DRY_RUN = args.includes("--dry-run");
const SELF_TEST = args.includes("--self-test");
const compareAt = args.indexOf("--compare");

const NEEDLE = "LIMA-4821-KAPPA";
const RECORDS = Array.from({ length: 80 }, (_, i) => {
  const n = String(i + 1).padStart(2, "0");
  return `record-${n}: zone=${["amber", "blue", "green", "violet"][i % 4]}, code=REF-${7000 + i}`;
});
RECORDS[62] = `record-63: zone=violet, recovery_key=${NEEDLE}`;

const CASES = [
  {
    id: "arithmetic_exact",
    task: "Calculate 17 * 24 - 9. Respond with only the integer, no explanation.",
    score: (s) => /^399[.!]?$/.test(normalize(s)),
    expected: "399",
  },
  {
    id: "identifier_extraction",
    task: "From the record below, return only its ticket ID.\nRecord: service restarted after timeout; ticket=OPS-7319; owner=R.\nTicket ID:",
    score: (s) => /^OPS-7319[.!]?$/.test(normalize(s)),
    expected: "OPS-7319",
  },
  {
    id: "json_exact",
    task: 'Return exactly this JSON object and nothing else: {"status":"ok","count":3}',
    score: (s) => {
      try {
        const value = JSON.parse(stripFence(s));
        return value?.status === "ok" && value?.count === 3 && Object.keys(value).length === 2;
      } catch { return false; }
    },
    expected: '{"status":"ok","count":3}',
  },
  {
    id: "ordered_lines",
    task: "Sort these names alphabetically. Return exactly three plain lines, without bullets or extra text: Zoe, Ana, Luis.",
    score: (s) => normalize(s).split(/\r?\n/).map((x) => x.trim()).filter(Boolean).join("\n") === "Ana\nLuis\nZoe",
    expected: "Ana\\nLuis\\nZoe",
  },
  {
    id: "long_context_retrieval",
    task: `Find record-63 and return only its recovery_key.\nRecords:\n${RECORDS.join("\n")}\nRecovery key:`,
    score: (s) => normalize(s) === NEEDLE,
    expected: NEEDLE,
  },
];

function normalize(value) {
  return String(value ?? "").trim().replace(/^```(?:\w+)?\s*|\s*```$/g, "").trim();
}
function stripFence(value) {
  return normalize(value).replace(/^```(?:json)?\s*/i, "").replace(/\s*```$/, "").trim();
}
function filler(chars) {
  const line = "Background reference: this neutral benchmark line contains no answer-bearing information.\n";
  return line.repeat(Math.max(0, Math.ceil(chars / line.length))).slice(0, Math.max(0, chars));
}
function buildContent(task, padChars) {
  const preamble = "Follow the final task exactly. The background below is unrelated unless the task explicitly refers to records.\n";
  return `${preamble}${filler(padChars)}\nTask:\n${task}`;
}
function resolveKey() {
  const env = (process.env.LM_KEY || "").trim();
  if (env) return env;
  try {
    const path = join(process.env.APPDATA || "", "LocalMind", "gateway.key");
    if (existsSync(path)) return readFileSync(path, "utf8").trim() || null;
  } catch {}
  return null;
}
async function request(url, { method = "GET", key, body, timeoutMs = 120000 } = {}) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(new Error(`timeout after ${timeoutMs}ms`)), timeoutMs);
  try {
    const response = await fetch(url, {
      method,
      signal: ctrl.signal,
      headers: {
        ...(key ? { Authorization: `Bearer ${key}` } : {}),
        ...(body ? { "Content-Type": "application/json" } : {}),
        Accept: "application/json",
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    const text = await response.text();
    let json = null;
    try { json = JSON.parse(text); } catch {}
    if (!response.ok) throw new Error(`${url} -> ${response.status}: ${text.slice(0, 300)}`);
    return { json, text };
  } finally { clearTimeout(timer); }
}
async function calibrate(engineUrl, key, task, context, reasoningEffort) {
  if (!Number.isInteger(TARGET) || TARGET < 1 || !Number.isInteger(MAX_TOKENS) || MAX_TOKENS < 1) {
    throw new Error("--prompt-tokens and --max-tokens must be positive integers");
  }
  if (TARGET + MAX_TOKENS + 16 > context) {
    throw new Error(`target ${TARGET} + max_tokens ${MAX_TOKENS} exceeds context ${context} with 16-token margin`);
  }
  let chars = Math.max(0, TARGET * 4 - task.length);
  let tokens = null;
  let content = "";
  const tolerance = Math.max(8, Math.ceil(TARGET * 0.002));
  let withinTolerance = false;
  for (let iteration = 1; iteration <= 10; iteration += 1) {
    content = buildContent(task, chars);
    const rendered = await request(`${engineUrl}/apply-template`, {
      method: "POST", key,
      body: { messages: [{ role: "user", content }], reasoning_effort: reasoningEffort },
    });
    if (typeof rendered.json?.prompt !== "string") throw new Error("/apply-template did not return prompt string");
    const tokenized = await request(`${engineUrl}/tokenize`, {
      method: "POST", key, body: { content: rendered.json.prompt, add_special: true, parse_special: true },
    });
    if (!Array.isArray(tokenized.json?.tokens)) throw new Error("/tokenize did not return tokens array");
    tokens = tokenized.json.tokens.length;
    if (Math.abs(tokens - TARGET) <= tolerance) { withinTolerance = true; break; }
    if (chars === 0 && tokens > TARGET + tolerance) {
      throw new Error(`case prompt needs ${tokens} tokens before padding; raise --prompt-tokens above that depth`);
    }
    const next = Math.max(0, Math.round(chars * TARGET / Math.max(tokens, 1)));
    chars = next === chars ? Math.max(0, chars + Math.sign(TARGET - tokens)) : next;
  }
  if (!withinTolerance) throw new Error(`could not calibrate prompt to ${TARGET} tokens (last count ${tokens})`);
  if (tokens + MAX_TOKENS + 16 > context) throw new Error(`calibrated prompt ${tokens} + max_tokens ${MAX_TOKENS} exceeds context ${context}`);
  return { content, tokenized_prompt_tokens: tokens, within_tolerance: withinTolerance };
}
async function evaluateCase(base, key, engineUrl, status, testCase, reasoningEffort) {
  const prompt = await calibrate(engineUrl, key, testCase.task, Number(status.context), reasoningEffort);
  const start = Date.now();
  const { json } = await request(`${base}/v1/chat/completions`, {
    method: "POST", key,
    body: {
      messages: [{ role: "user", content: prompt.content }],
      temperature: 0,
      seed: 42,
      max_tokens: MAX_TOKENS,
      stream: false,
      reasoning_effort: reasoningEffort,
    },
    timeoutMs: 600000,
  });
  const elapsedMs = Date.now() - start;
  const choice = json?.choices?.[0];
  const answer = typeof choice?.message?.content === "string" ? choice.message.content : "";
  const usage = json?.usage || {};
  const actualPrompt = Number.isFinite(Number(usage.prompt_tokens)) ? Number(usage.prompt_tokens) : null;
  const completion = Number.isFinite(Number(usage.completion_tokens)) ? Number(usage.completion_tokens) : null;
  const depthOk = actualPrompt !== null && Math.abs(actualPrompt - TARGET) <= Math.max(8, Math.ceil(TARGET * 0.002));
  return {
    id: testCase.id,
    passed: Boolean(testCase.score(answer)),
    prompt_tokens_target: TARGET,
    prompt_tokens_calibrated: prompt.tokenized_prompt_tokens,
    prompt_tokens: actualPrompt,
    prompt_depth_match: depthOk,
    completion_tokens: completion,
    elapsed_ms: elapsedMs,
    answer,
    expected: testCase.expected,
  };
}
function selfTest() {
  const checks = [
    ["arithmetic exact", CASES[0].score("399"), true],
    ["arithmetic rejects explanation", CASES[0].score("The answer is 399"), false],
    ["identifier extraction", CASES[1].score("OPS-7319\n"), true],
    ["JSON exact fields", CASES[2].score('{"status":"ok","count":3}'), true],
    ["JSON rejects wrong count", CASES[2].score('{"status":"ok","count":4}'), false],
    ["ordered lines", CASES[3].score("Ana\nLuis\nZoe"), true],
    ["long-context needle", CASES[4].score(NEEDLE), true],
    ["filler exact size", filler(127).length === 127, true],
  ];
  let failures = 0;
  for (const [name, got, expected] of checks) {
    const pass = got === expected;
    if (!pass) failures += 1;
    console.log(`${pass ? "PASS" : "FAIL"} ${name}`);
  }
  console.log(`${checks.length - failures}/${checks.length} quality harness assertions passed`);
  if (failures) process.exitCode = 1;
}
function compareFiles(aPath, bPath) {
  const a = JSON.parse(readFileSync(aPath, "utf8"));
  const b = JSON.parse(readFileSync(bPath, "utf8"));
  const aCases = new Map((a.cases || []).map((c) => [c.id, c]));
  const bCases = new Map((b.cases || []).map((c) => [c.id, c]));
  const ids = [...new Set([...aCases.keys(), ...bCases.keys()])];
  console.log(`A: ${a.model || "?"} (${a.engine_build || "build unknown"}) | B: ${b.model || "?"} (${b.engine_build || "build unknown"})`);
  let aScore = 0, bScore = 0, paired = 0;
  for (const id of ids) {
    const ca = aCases.get(id), cb = bCases.get(id);
    if (!ca || !cb) { console.log(`${id}: only in ${ca ? "A" : "B"}`); continue; }
    const sameTarget = a.prompt_tokens_target === b.prompt_tokens_target;
    const pa = Number(ca.prompt_tokens), pb = Number(cb.prompt_tokens);
    const target = Number(a.prompt_tokens_target);
    const depthMatches = sameTarget && Number.isFinite(pa) && Number.isFinite(pb) &&
      Math.abs(pa - pb) <= Math.max(8, Math.ceil(target * 0.002));
    if (depthMatches) { paired += 1; aScore += Number(ca.passed); bScore += Number(cb.passed); }
    const latency = Number.isFinite(ca.elapsed_ms) && Number.isFinite(cb.elapsed_ms)
      ? ` latency A=${ca.elapsed_ms}ms B=${cb.elapsed_ms}ms` : "";
    console.log(`${id}: A=${ca.passed ? "PASS" : "FAIL"} B=${cb.passed ? "PASS" : "FAIL"}${depthMatches ? latency : " incomparable: prompt depth mismatch"}`);
  }
  console.log(`score paired: A ${aScore}/${paired}, B ${bScore}/${paired}; prompt-depth equality is required for comparable latency.`);
}
function stamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "Z");
}

async function main() {
  if (args.includes("--help") || args.includes("-h")) {
    console.log("node tests/engine-quality.mjs [--prompt-tokens N] [--max-tokens N] [--reasoning-effort none|low|medium|xhigh] [--base-url URL] [--out DIR] [--dry-run] [--self-test] [--compare A.json B.json]");
    return;
  }
  if (SELF_TEST) return selfTest();
  if (!["none", "low", "medium", "xhigh"].includes(REASONING_EFFORT)) {
    throw new Error("--reasoning-effort must be one of: none, low, medium, xhigh");
  }
  if (compareAt >= 0) {
    const files = args.slice(compareAt + 1).filter((x) => !x.startsWith("--"));
    if (files.length !== 2) throw new Error("--compare needs two result files");
    compareFiles(files[0], files[1]);
    return;
  }
  if (DRY_RUN) {
    console.log(`dry-run: read-only evaluation via ${BASE}; target=${TARGET} prompt tokens; max=${MAX_TOKENS} completion tokens; reasoning=${REASONING_EFFORT}; ${CASES.length} fixed cases.`);
    console.log("requires GET /api/status = running + acceptance_ok=true and the engine /apply-template + /tokenize routes; no requests issued.");
    for (const c of CASES) console.log(`- ${c.id}: expected ${c.expected}`);
    return;
  }
  const key = resolveKey();
  if (!key) throw new Error("No gateway key; set LM_KEY or ensure %APPDATA%/LocalMind/gateway.key exists");
  const status = (await request(`${BASE}/api/status`, { key })).json;
  if (status?.status !== "running" || status?.acceptance_ok !== true) {
    throw new Error(`refusing to run: engine must already be running and accepted (status=${status?.status}, acceptance_ok=${status?.acceptance_ok})`);
  }
  const port = Number(status.port);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error(`invalid engine port in status: ${status.port}`);
  const engineUrl = `http://127.0.0.1:${port}`;
  const version = await request(`${BASE}/api/version`, { key }).catch(() => ({ json: null }));
  const result = {
    schema_version: 1,
    started_at: new Date().toISOString(),
    model: status.model || null,
    profile: status.profile || null,
    context: status.context || null,
    engine_build: version.json?.engine_build || null,
    engine_commit: version.json?.engine_commit || null,
    prompt_tokens_target: TARGET,
    max_tokens: MAX_TOKENS,
    temperature: 0,
    seed: 42,
    reasoning_effort: REASONING_EFFORT,
    cases: [],
  };
  let failures = 0;
  for (const testCase of CASES) {
    try {
      const row = await evaluateCase(BASE, key, engineUrl, status, testCase, REASONING_EFFORT);
      result.cases.push(row);
      if (!row.passed || !row.prompt_depth_match) failures += 1;
      console.log(`${row.passed ? "PASS" : "FAIL"} ${row.id} prompt=${row.prompt_tokens ?? "n/a"}/${TARGET} elapsed=${row.elapsed_ms}ms${row.prompt_depth_match ? "" : " depth-mismatch"}`);
    } catch (error) {
      failures += 1;
      result.cases.push({ id: testCase.id, passed: false, error: String(error?.message || error) });
      console.log(`FAIL ${testCase.id}: ${error?.message || error}`);
    }
  }
  result.finished_at = new Date().toISOString();
  const outDir = resolve(OUT);
  mkdirSync(outDir, { recursive: true });
  const path = join(outDir, `quality-${stamp()}.json`);
  writeFileSync(path, `${JSON.stringify(result, null, 2)}\n`, { flag: "wx" });
  const passed = result.cases.filter((c) => c.passed && c.prompt_depth_match).length;
  console.log(`quality: ${passed}/${CASES.length}; saved ${path}`);
  if (failures) process.exitCode = 1;
}

main().catch((error) => {
  console.error(`error: ${error?.message || error}`);
  process.exitCode = 1;
});
