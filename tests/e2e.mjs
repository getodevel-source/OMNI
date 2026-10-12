#!/usr/bin/env node
// LocalMind end-to-end audit (Node ESM, no dependencies, Node >= 18).
// Automates manual audit steps 4-13. Without flags it runs ONLY engine-off
// checks (auth, public UI, 502/409 branches, offline harnesses NOT included).
// With --with-engine it also performs ONE engine load at a time (never two
// concurrently): start -> gate -> chat/anthropic/responses streaming ->
// models -> usage -> agent isolation -> stop -> 409/502 -> second load (ETA)
// -> stop. Context is ALWAYS pinned explicitly (precedence-bug safe).
//
// Usage: node tests/e2e.mjs [baseUrl] [--with-engine] [--profile velocidad]
//   [--context 32768] [--timeout-ms 1800000]
// Key resolution (like tests/smoke.mjs): env LM_KEY, else
// %APPDATA%/LocalMind/gateway.key (trimmed). Non-zero exit on any FAIL.

import { existsSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";

const ARGS = process.argv.slice(2);
const BASE = (ARGS.find((a) => !a.startsWith("--")) || "http://127.0.0.1:17860").replace(/\/+$/, "");
const WITH_ENGINE = ARGS.includes("--with-engine");
const PROFILE = (ARGS.find((a) => a.startsWith("--profile=")) || "--profile=velocidad").split("=")[1];
const CONTEXT = parseInt((ARGS.find((a) => a.startsWith("--context=")) || "--context=32768").split("=")[1], 10);
const TIMEOUT_MS = parseInt((ARGS.find((a) => a.startsWith("--timeout-ms=")) || "--timeout-ms=1800000").split("=")[1], 10);

function resolveKey() {
  const env = (process.env.LM_KEY || "").trim();
  if (env) return { key: env, source: "LM_KEY" };
  try {
    const appdata = process.env.APPDATA;
    if (appdata) {
      const p = join(appdata, "LocalMind", "gateway.key");
      if (existsSync(p)) {
        const k = readFileSync(p, "utf8").trim();
        if (k) return { key: k, source: "gateway.key" };
      }
    }
  } catch {}
  return { key: "", source: "none" };
}
const { key: API_KEY, source: KEY_SOURCE } = resolveKey();
const H = () => ({ Authorization: `Bearer ${API_KEY}`, "Content-Type": "application/json" });
console.log(`key source: ${KEY_SOURCE} | base: ${BASE} | with-engine: ${WITH_ENGINE}`);

let pass = 0, fail = 0, skipped = 0;
const check = (ok, name, detail = "") => {
  if (ok) { pass++; console.log(`PASS ${name}${detail ? " :: " + detail : ""}`); }
  else { fail++; console.log(`FAIL ${name}${detail ? " :: " + detail : ""}`); }
};
// SKIP honesto: informa sin inflar el contador de PASS.
const skip = (name, detail = "") => { skipped++; console.log(`SKIP ${name}${detail ? " :: " + detail : ""}`); };

async function fetchT(path, { method = "GET", body = null, auth = true, timeout = 15000 } = {}) {
  const ctl = new AbortController();
  const t = setTimeout(() => ctl.abort(), timeout);
  try {
    const res = await fetch(BASE + path, {
      method,
      headers: { ...(auth && API_KEY ? { Authorization: `Bearer ${API_KEY}` } : {}), ...(body ? { "Content-Type": "application/json" } : {}) },
      body: body ? JSON.stringify(body) : null,
      signal: ctl.signal,
    });
    const text = await res.text();
    return { status: res.status, text, headers: res.headers };
  } finally { clearTimeout(t); }
}
const jparse = (t) => { try { return JSON.parse(t); } catch { return null; } };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Collect SSE events incrementally (proves streaming). Returns {events, deltas, firstDeltaMs, lastMs, raw}.
async function sseEvents(path, body, deltaName) {
  const t0 = Date.now();
  const res = await fetch(BASE + path, { method: "POST", headers: H(), body: JSON.stringify(body) });
  if (res.status !== 200) return { status: res.status, events: [], error: await res.text() };
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let buf = "", events = [], deltas = 0, firstDelta = null, last = null, firstByte = null, raw = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    if (firstByte === null) firstByte = Date.now();
    last = Date.now();
    buf += dec.decode(value, { stream: true });
    let i;
    while ((i = buf.indexOf("\n\n")) >= 0) {
      const blk = buf.slice(0, i); buf = buf.slice(i + 2); raw += blk + "\n---\n";
      const ev = (/^event:\s*(.+)$/m.exec(blk) || [])[1] || "?";
      events.push(ev);
      if (ev === deltaName) { deltas++; if (firstDelta === null) firstDelta = Date.now(); }
    }
  }
  return { status: 200, events, deltas, firstByteMs: firstByte - t0, firstDeltaMs: firstDelta === null ? null : firstDelta - t0, lastMs: last - t0, raw };
}

async function waitStatus(want, timeoutMs) {
  const t0 = Date.now();
  for (;;) {
    const r = await fetchT("/api/status");
    const j = jparse(r.text) || {};
    if (want(j)) return j;
    if (Date.now() - t0 > timeoutMs) return { __timeout: true, ...j };
    await sleep(3000);
  }
}
const homeFile = (rel) => join(process.env.USERPROFILE, ...rel.split("/"));
const sha256 = (p) => createHash("sha256").update(readFileSync(p)).digest("hex");

async function main() {
  // ---- engine-off checks (always run) ----
  {
    const r = await fetchT("/api/status", { auth: false });
    check(r.status === 401, "auth required on /api/status", `status=${r.status}`);
  }
  {
    const r = await fetchT("/", { auth: false });
    check(r.status === 200 && r.text.includes("<html"), "GET / public", `status=${r.status}`);
  }
  if (!API_KEY) { console.log("SKIP authenticated checks: no key"); }
  else {
    const m = await fetchT("/v1/models");
    const j = jparse(m.text);
    const ids = j && Array.isArray(j.data) ? j.data.map((d) => d.id) : [];
    check(m.status === 200 && ids.includes("localmind") && ids.includes("qwen3.8-27b"), "GET /v1/models ids", JSON.stringify(ids));
    const st = await fetchT("/api/status");
    const sj = jparse(st.text) || {};
    if (sj.status !== "running") {
      const met = await fetchT("/api/metrics");
      check(met.status === 502 && met.text.includes("engine_down"), "GET /api/metrics engine-off 502", `status=${met.status}`);
      const before = {};
      for (const a of ["omp", "pi"]) {
        const d = join(process.env.APPDATA, "LocalMind", "agents", a);
        for (const f of ["models.yml", "models.json"]) {
          const p = join(d, f);
          if (existsSync(p)) before[p] = statSync(p).mtimeMs;
        }
      }
      const lo = await fetchT("/api/launch_omp", { method: "POST", body: { target: "sistema" } });
      check(lo.status === 409, "POST /api/launch_omp engine-off 409", `status=${lo.status} body=${lo.text.slice(0, 120)}`);
      let unchanged = true;
      for (const [p, mt] of Object.entries(before)) if (statSync(p).mtimeMs !== mt) unchanged = false;
      if (Object.keys(before).length === 0) skip("launch-off writes nothing", "0 tracked files (nada que validar)");
      else check(unchanged, "launch-off writes nothing", `${Object.keys(before).length} tracked files`);
    } else {
      skip("engine-off 502/409 branches", "engine running (use engine-on path)");
    }
  }
  if (!WITH_ENGINE) { console.log(`engine-off only (--with-engine not given): ${pass} pass ${fail} fail ${skipped} skipped`); return fail ? 1 : 0; }
  if (!API_KEY) { console.log("FAIL: --with-engine needs a key"); return 1; }

  // ---- engine-on path: exactly two sequential loads, never concurrent ----
  const homeHashes = {};
  for (const rel of [".pi/agent/models.json", ".omp/agent/models.yml", ".omp/agent/config.yml"]) {
    const p = homeFile(rel);
    if (existsSync(p)) homeHashes[rel] = sha256(p);
  }
  const loadTimesPath = join(process.env.APPDATA, "LocalMind", "usage.jsonl");

  for (let load = 1; load <= 2; load++) {
    const sr = await fetchT("/api/start", { method: "POST", body: { profile: PROFILE, context: CONTEXT }, timeout: 30000 });
    check(sr.status === 200, `load${load}: POST /api/start pinned ctx`, `status=${sr.status} body=${sr.text.slice(0, 120)}`);
    let sawStarting = false, sawEta = false, sawVerify = false;
    const t0 = Date.now();
    let st = null;
    for (;;) {
      const r = await fetchT("/api/status");
      st = jparse(r.text) || {};
      if (st.status === "starting") sawStarting = true;
      if ((st.eta_secs || 0) > 0) sawEta = true;
      if (st.verifying) sawVerify = true;
      if (st.status === "running" || st.status === "error" || Date.now() - t0 > TIMEOUT_MS) break;
      await sleep(3000);
    }
    if (load === 1) {
      check(sawStarting && sawEta, "load1: starting_for_secs grows + eta_secs>0", `eta observed=${sawEta}`);
      check(sawVerify, "load1: verifying flips true during gate", "");
    } else {
      try {
        const lt = JSON.parse(readFileSync(join(process.env.APPDATA, "LocalMind", "load-times.json"), "utf8"));
        check(sawEta, "load2: eta_secs from stored duration", `stored=${JSON.stringify(lt).slice(0, 160)}`);
      } catch (e) { check(false, "load2: eta_secs from stored duration", String(e).slice(0, 120)); }
    }
    check(st.status === "running" && st.acceptance_ok === true
      && Number.isFinite(st.request_tps) && st.request_tps >= 3,
      `load${load}: gate passes`, `status=${st.status} ok=${st.acceptance_ok} request_tps=${st.request_tps} decode_tps=${st.decode_tps} ctx=${st.context}`);
    check(st.decode_tps == null || (Number.isFinite(st.decode_tps) && st.decode_tps > 0),
      `load${load}: native decode timing is valid when present`, `decode_tps=${st.decode_tps}`);
    check(st.context === CONTEXT, `load${load}: pinned context honored`, `context=${st.context} want=${CONTEXT}`);
    if (st.status !== "running") { console.log(`ABORT: engine ${st.status}: ${(st.last_error || "").slice(0, 300)}`); return 1; }
    if (load === 1) {
      // chat streaming: first byte well before last
      const cr = await sseEvents("/v1/chat/completions",
        { model: "whatever-alias", messages: [{ role: "user", content: "Cuenta del 1 al 60, solo numeros separados por comas." }], max_tokens: 200, temperature: 0, stream: true, stream_options: { include_usage: true } }, "choice-delta");
      check(cr.status === 200 && cr.firstByteMs !== null && cr.lastMs - cr.firstByteMs > 1000,
        "chat streaming incremental", `first=${cr.firstByteMs}ms last=${cr.lastMs}ms`);
      const lines = readFileSync(loadTimesPath, "utf8").trim().split("\n");
      const last = jparse(lines[lines.length - 1]) || {};
      check(last.endpoint === "chat.completions" && last.completion_tokens >= 20, "usage.jsonl last line matches", JSON.stringify(last).slice(0, 200));
      // anthropic
      const ar = await sseEvents("/v1/messages", { model: "localmind", max_tokens: 200, messages: [{ role: "user", content: "Di hola y cuenta hasta 20." }], stream: true }, "content_block_delta");
      check(ar.status === 200 && ar.events[0] === "message_start" && ar.events.includes("content_block_start") && ar.deltas >= 2 && ar.events.includes("content_block_stop") && ar.events.includes("message_delta") && ar.events[ar.events.length - 1] === "message_stop",
        "anthropic stream event order", `deltas=${ar.deltas} n=${ar.events.length}`);
      check(ar.firstDeltaMs !== null && ar.firstDeltaMs <= ar.lastMs, "anthropic first delta before last", `first=${ar.firstDeltaMs}ms last=${ar.lastMs}ms`);
      const an = await fetchT("/v1/messages", { method: "POST", body: { model: "localmind", max_tokens: 50, messages: [{ role: "user", content: "ok" }] }, timeout: 300000 });
      const aj = jparse(an.text) || {};
      check(an.status === 200 && aj.type === "message" && Array.isArray(aj.content) && aj.usage, "anthropic non-stream envelope", an.text.slice(0, 160));
      // responses
      const rr = await sseEvents("/v1/responses", { model: "localmind", input: "Responde ok y cuenta hasta 10.", stream: true, max_output_tokens: 200 }, "response.output_text.delta");
      check(rr.status === 200 && rr.events[0] === "response.created" && rr.events.includes("response.output_item.added") && rr.deltas >= 2 && rr.events.includes("response.output_item.done") && rr.events[rr.events.length - 1] === "response.completed",
        "responses stream event order", `deltas=${rr.deltas} n=${rr.events.length}`);
      const rn = await fetchT("/v1/responses", { method: "POST", body: { model: "localmind", input: "ok", max_output_tokens: 50 }, timeout: 300000 });
      check(rn.status === 200 && rn.text.includes("output"), "responses non-stream output", rn.text.slice(0, 160));
      // agent isolation: launch writes private dir with key, home untouched
      const lo = await fetchT("/api/launch_omp", { method: "POST", body: { target: "sistema" }, timeout: 60000 });
      check(lo.status === 200, "launch_omp 200 (opens terminal; expected)", lo.text.slice(0, 160));
      const ymlP = join(process.env.APPDATA, "LocalMind", "agents", "omp", "models.yml");
      const yml = existsSync(ymlP) ? readFileSync(ymlP, "utf8") : "";
      check(yml.includes(API_KEY) && yml.includes(String(CONTEXT)), "omp private models.yml carries key+live ctx", `size=${yml.length}`);
      let homeOk = true;
      for (const [rel, h] of Object.entries(homeHashes)) if (sha256(homeFile(rel)) !== h) homeOk = false;
      check(homeOk, "home agent files byte-identical", `${Object.keys(homeHashes).length} files`);
      const hasOmp = spawnSync("where", ["omp"], { timeout: 15000 }).status === 0;
      if (hasOmp) {
        const t = Date.now();
        const r = spawnSync("omp", ["-p", "Responde exactamente: ok", "--no-session", "--model", "localmind/localmind"], {
          timeout: 240000, encoding: "utf8",
          env: { ...process.env, PI_CODING_AGENT_DIR: join(process.env.APPDATA, "LocalMind", "agents", "omp"), OPENAI_BASE_URL: `${BASE}/v1` },
        });
        check(r.status === 0 && (r.stdout || "").includes("ok"), "omp one-shot via local engine", `exit=${r.status} ms=${Date.now() - t} out=${(r.stdout || "").trim().slice(0, 80)}`);
      } else skip("omp one-shot (not installed)", "sin binario omp en PATH: no suma PASS");
    }

    const stop = await fetchT("/api/stop", { method: "POST", timeout: 60000 });
    check(stop.status === 200, `load${load}: POST /api/stop`, stop.text.slice(0, 80));
    const fin = await waitStatus((j) => j.status === "stopped", 120000);
    check(fin.status === "stopped", `load${load}: status stopped`, JSON.stringify(fin).slice(0, 160));
  }
  // engine-off tail
  const met = await fetchT("/api/metrics");
  check(met.status === 502, "final metrics 502 engine_down", `status=${met.status}`);
  console.log(`== SUMMARY: ${pass} passed, ${fail} failed, ${skipped} skipped ==`);
  return fail ? 1 : 0;
}

process.exitCode = await main().catch((e) => { console.error("FATAL", e); return 2; });
