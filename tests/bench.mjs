#!/usr/bin/env node
// LocalMind performance benchmark harness (Node ESM, no dependencies, Node >= 18).
// Measures engine throughput per context profile so the "30 t/s at 262K" target
// (SRS LM-NF-1 / LM-NF-2) can be pursued with evidence instead of impressions.
//
// Contract: docs/SRS.md §3.2/§4 + src-rust/src/server.rs + src-rust/src/process.rs.
// Key resolution and timeout style copied from tests/smoke.mjs:
//   env LM_KEY, else %APPDATA%/LocalMind/gateway.key (trimmed); global fetch,
//   AbortSignal timeouts, no dependencies.
//
// Usage:
//   node tests/bench.mjs --profiles 32k,64k,128k,262k [--prompt-tokens 512]
//     [--runs 1] [--max-tokens 256] [--workload repeat|ram-cache-churn]
//     [--base-url http://127.0.0.1:17860]
//     [--out tests/bench-results] [--dry-run] [--self-test] [--stop-after]

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";

// ---------------------------------------------------------------------------
// Pure helpers (no I/O; unit-asserted by --self-test)
// ---------------------------------------------------------------------------

// CLI shorthand -> real engine profile id (src-rust/src/config.rs
// built_in_profiles) + expected context size. `32k@cache512` keeps the stable
// comparison label/context while selecting a custom engine profile. The live
// catalog is checked before any launch so unknown IDs can never fall through
// to the engine's default profile.
const PROFILE_MAP = {
  "32k": { id: "velocidad", context: 32768 },
  "64k": { id: "multi_doc", context: 65536 },
  "128k": { id: "libros", context: 131072 },
  "262k": { id: "max_contexto", context: 262144 },
};

function normalizeProfile(label) {
  const raw = String(label || "").trim();
  const parts = raw.split("@");
  if (parts.length > 2) throw new Error(`invalid profile selector "${label}"`);
  const k = parts[0].trim().toLowerCase().replace(/\s+/g, "");
  const short = k.endsWith("k") ? k : `${k}k`;
  const hit = PROFILE_MAP[short];
  if (!hit) throw new Error(`unknown profile "${label}" (expected one of: 32k, 64k, 128k, 262k)`);
  if (parts.length === 2) {
    const id = parts[1].trim();
    if (!/^[a-z0-9_-]{1,32}$/.test(id)) throw new Error(`invalid engine profile id in "${label}"`);
    return { label: short, id, context: hit.context, custom_id: true };
  }
  return { label: short, ...hit, custom_id: false };
}

function duplicateBenchmarkCondition(profiles) {
  const seen = new Set();
  for (const profile of profiles) {
    const key = `${profile.label}\u0000${profile.context}`;
    if (seen.has(key)) return profile;
    seen.add(key);
  }
  return null;
}

// Rough token estimate: ~4 chars per token (llama.cpp / OpenAI heuristic).
function approxTokens(s) {
  return Math.round(String(s).length / 4);
}

// Deterministic prompt whose approximate token count tracks the request.
// Built from a fixed sentence repeated to ~4 chars per requested token, so
// approxTokens(out) === target within rounding (well inside ±20%).
const PROMPT_SENTENCE =
  "Benchmark datum: the quick brown fox jumps over the lazy dog 0123456789. ";
const CHURN_PROMPT_SENTENCE =
  "Cache churn: twenty red bicycles cross quiet bridges before dawn 9876543210. ";
const MAX_CHURN_PROMPT_TOKENS = 256;
// Give the model a bounded, repeatable task after the calibrated filler. A
// filler-only prompt can spend the full token budget on hidden reasoning or
// continuation, leaving first visible text unmeasured.
const BENCHMARK_TASK =
  "Task: Explain the benchmark sentence in exactly 40 words. Begin the visible answer with READY. Do not add a preamble.";
const BENCHMARK_VISIBLE_MARKER = "READY";
const BENCHMARK_SCHEMA_VERSION = 4;

function appendVisiblePrefix(prefix, chunk, limit = 64) {
  return `${prefix}${typeof chunk === "string" ? chunk : ""}`.slice(0, limit);
}

function benchmarkVisibleMarkerFound(prefix) {
  return new RegExp(`^${BENCHMARK_VISIBLE_MARKER}\\b`, "i").test(String(prefix || "").trimStart());
}

function buildBenchmarkPrompt(filler) {
  return `${filler}\n\n${BENCHMARK_TASK}`;
}
function buildPrompt(targetTokens) {
  const target = Math.max(1, Math.floor(Number(targetTokens) || 0));
  const targetChars = target * 4;
  return buildPromptChars(targetChars);
}

function buildPromptChars(targetChars, sentence = PROMPT_SENTENCE) {
  const chars = Math.max(1, Math.floor(Number(targetChars) || 0));
  const seed = String(sentence || PROMPT_SENTENCE);
  const reps = Math.max(1, Math.ceil(chars / seed.length));
  return seed.repeat(reps).slice(0, chars);
}

function scaledPromptChars(currentChars, targetTokens, currentTokens) {
  if (!(currentChars > 0) || !(targetTokens > 0) || !(currentTokens > 0)) return Math.max(1, currentChars);
  const scaled = Math.round(currentChars * (targetTokens / currentTokens));
  return Math.max(1, scaled === currentChars ? currentChars + Math.sign(targetTokens - currentTokens) : scaled);
}

async function postEngineJson(engineBaseUrl, apiKey, route, body, timeoutMs = 120000) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(new Error(`${route} timeout after ${timeoutMs}ms`)), timeoutMs);
  try {
    const res = await fetch(`${engineBaseUrl}${route}`, {
      method: "POST",
      signal: ctrl.signal,
      headers: {
        ...(apiKey ? { Authorization: `Bearer ${apiKey}` } : {}),
        "Content-Type": "application/json",
        Accept: "application/json",
      },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    if (!res.ok) throw new Error(`${route} -> ${res.status} ${text.slice(0, 300)}`);
    try {
      return JSON.parse(text);
    } catch (e) {
      throw new Error(`${route} returned invalid JSON: ${e.message}`);
    }
  } finally {
    clearTimeout(timer);
  }
}

// Tune the deterministic prompt to a real token target using the loaded
// model's chat template and tokenizer. The final usage count in the chat
// response remains authoritative and is recorded separately.
async function preparePromptForDepth(engineBaseUrl, apiKey, targetTokens, contextTokens, maxTokens, sentence = PROMPT_SENTENCE, reasoningEffort = "medium") {
  const contextMargin = 16;
  if (targetTokens + maxTokens + contextMargin > contextTokens) {
    throw new Error(
      `prompt target ${targetTokens} + max_tokens ${maxTokens} exceeds context ${contextTokens} with ${contextMargin} token safety margin`
    );
  }
  let chars = Math.max(1, targetTokens * 4);
  let prompt = buildBenchmarkPrompt(buildPromptChars(chars, sentence));
  let tokenizedPromptTokens = null;
  let iterations = 0;
  const tolerance = Math.max(8, Math.ceil(targetTokens * 0.002));
  for (iterations = 1; iterations <= 8; iterations += 1) {
    const rendered = await postEngineJson(engineBaseUrl, apiKey, "/apply-template", {
      messages: [{ role: "user", content: prompt }],
      reasoning_effort: reasoningEffort,
    });
    if (typeof rendered.prompt !== "string") throw new Error("/apply-template response has no prompt string");
    const tokens = await postEngineJson(engineBaseUrl, apiKey, "/tokenize", {
      content: rendered.prompt,
      add_special: true,
      parse_special: true,
    });
    if (!Array.isArray(tokens.tokens)) throw new Error("/tokenize response has no tokens array");
    tokenizedPromptTokens = tokens.tokens.length;
    if (Math.abs(tokenizedPromptTokens - targetTokens) <= tolerance) break;
    chars = scaledPromptChars(chars, targetTokens, tokenizedPromptTokens);
    prompt = buildBenchmarkPrompt(buildPromptChars(chars, sentence));
  }
  if (tokenizedPromptTokens + maxTokens + contextMargin > contextTokens) {
    throw new Error(
      `tokenized prompt ${tokenizedPromptTokens} + max_tokens ${maxTokens} exceeds context ${contextTokens}`
    );
  }
  if (Math.abs(tokenizedPromptTokens - targetTokens) > tolerance) {
    throw new Error(
      `could not calibrate prompt to ${targetTokens} tokens (last count ${tokenizedPromptTokens})`
    );
  }
  return {
    prompt,
    target_tokens: targetTokens,
    tokenized_prompt_tokens: tokenizedPromptTokens,
    tokenization_iterations: Math.min(iterations, 8),
    within_tolerance: Math.abs(tokenizedPromptTokens - targetTokens) <= tolerance,
    tokenization_mode: "llama.cpp apply-template + tokenize",
  };
}

function mean(xs) {
  if (!xs.length) return null;
  let s = 0;
  for (const x of xs) s += x;
  return s / xs.length;
}

function median(xs) {
  const sorted = xs.filter(Number.isFinite).sort((a, b) => a - b);
  if (!sorted.length) return null;
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 1 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

const PROBE_NUMERIC_SUMMARY_FIELDS = [
  "prompt_tokens",
  "completion_tokens",
  "cached_prompt_tokens",
  "first_output_ms",
  "last_output_ms",
  "first_reasoning_ms",
  "last_reasoning_ms",
  "reasoning_duration_ms",
  "reasoning_lead_ms",
  "first_text_ms",
  "last_text_ms",
  "ttft_ms",
  "last_token_ms",
  "decode_secs",
  "stream_decode_tps",
  "request_to_first_output_tps",
  "decode_tps",
  "prefill_tps",
];

function completeMedian(values) {
  if (!values.length || values.some((value) => !Number.isFinite(value))) return null;
  return median(values);
}

function categoricalSummary(values) {
  if (!values.length || values.every((value) => value === null || value === undefined)) return null;
  if (values.some((value) => value === null || value === undefined)) return "mixed";
  return values.every((value) => value === values[0]) ? values[0] : "mixed";
}

function aggregateProbeSamples(samples) {
  const result = Object.fromEntries(
    PROBE_NUMERIC_SUMMARY_FIELDS.map((field) => [field, completeMedian(samples.map((sample) => sample[field] ?? null))])
  );
  const timingKeys = [...new Set(samples.flatMap((sample) => Object.keys(sample.engine_timings || {})))].sort();
  result.engine_timings = Object.fromEntries(
    timingKeys.map((key) => [
      key,
      completeMedian(samples.map((sample) => sample.engine_timings?.[key] ?? null)),
    ])
  );
  result.decode_tps_source = categoricalSummary(samples.map((sample) => sample.decode_tps_source ?? null));
  if (result.decode_tps_source === "mixed") result.decode_tps = null;
  result.buffered = categoricalSummary(samples.map((sample) => sample.buffered ?? null));
  result.probe_runs_completed = samples.length;
  return result;
}

// Nearest-rank percentile over a pre-sorted ascending array. null when empty.
function percentile(sorted, p) {
  if (!sorted.length) return null;
  const rank = Math.ceil((p / 100) * sorted.length) - 1;
  return sorted[Math.min(Math.max(rank, 0), sorted.length - 1)];
}

// Extract {prompt_tokens, completion_tokens} from one SSE `data:` payload or a
// plain JSON body. Returns nulls when no usable usage object is present.
function usageFromJsonObject(obj) {
  const u = obj && typeof obj === "object" ? obj.usage : null;
  if (!u || typeof u !== "object") return { prompt_tokens: null, completion_tokens: null };
  const p = Number(u.prompt_tokens);
  const c = Number(u.completion_tokens);
  return {
    prompt_tokens: Number.isFinite(p) ? p : null,
    completion_tokens: Number.isFinite(c) ? c : null,
  };
}

function parseSseUsage(text) {
  const lines = String(text).split(/\r?\n/);
  let prompt_tokens = null;
  let completion_tokens = null;
  for (const line of lines) {
    const t = line.trim();
    let payload = null;
    if (t.startsWith("data:")) {
      const data = t.slice(5).trim();
      if (!data || data === "[DONE]") continue;
      try {
        payload = JSON.parse(data);
      } catch {
        continue;
      }
    } else if (t.startsWith("{")) {
      try {
        payload = JSON.parse(t);
      } catch {
        continue;
      }
    }
    if (payload) {
      const u = usageFromJsonObject(payload);
      if (u.prompt_tokens !== null) prompt_tokens = u.prompt_tokens;
      if (u.completion_tokens !== null) completion_tokens = u.completion_tokens;
    }
  }
  return { prompt_tokens, completion_tokens };
}

// Classify generated output without storing its content. llama.cpp may emit
// hidden reasoning before the user-visible `content` field.
function chunkSignals(obj) {
  try {
    const ch = obj && obj.choices && obj.choices[0];
    if (!ch) return { has_output: false, has_text: false, has_reasoning: false, text: "" };
    const delta = ch.delta && typeof ch.delta === "object" ? ch.delta : {};
    const text =
      (typeof delta.content === "string" && delta.content) ||
      (typeof ch.text === "string" && ch.text) ||
      (ch.message && typeof ch.message.content === "string" && ch.message.content) ||
      "";
    const reasoningFields = ["reasoning_content", "reasoning", "analysis", "thinking"];
    const reasoning = reasoningFields.some((key) => typeof delta[key] === "string" && delta[key].length > 0);
    const toolCall = Array.isArray(delta.tool_calls) && delta.tool_calls.length > 0;
    const has_text = text.length > 0;
    return {
      has_output: has_text || reasoning || toolCall,
      has_text,
      has_reasoning: reasoning,
      text,
    };
  } catch {
    return { has_output: false, has_text: false, has_reasoning: false, text: "" };
  }
}

// llama.cpp includes per-request prompt/decode timings in the final SSE event.
// Those engine timings are more reliable than timing socket read boundaries.
function timingsFromJsonObject(obj) {
  const t = obj && typeof obj === "object" && obj.timings && typeof obj.timings === "object" ? obj.timings : null;
  if (!t) return null;
  const read = (key) => {
    if (t[key] === null || t[key] === undefined || t[key] === "") return null;
    const value = Number(t[key]);
    return Number.isFinite(value) && value >= 0 ? value : null;
  };
  const result = {
    prompt_tokens: read("prompt_n"),
    prompt_ms: read("prompt_ms"),
    prompt_tps: read("prompt_per_second"),
    decode_tokens: read("predicted_n"),
    decode_ms: read("predicted_ms"),
    decode_tps: read("predicted_per_second"),
    cached_prompt_tokens: read("cache_n"),
    draft_tokens: read("draft_n"),
    accepted_draft_tokens: read("draft_n_accepted"),
  };
  return Object.values(result).some((value) => value !== null) ? result : null;
}

// Cap for the raw per-read timing array kept in the JSON output.
const MAX_SAMPLES = 500;

// Pure reduction of a synthetic chunk timeline to probe metrics (no I/O;
// unit-asserted by --self-test). `samples` records output, text, and reasoning
// deltas separately. Socket-derived rates are only estimates; engine timings
// are kept separate and take precedence for prompt/decode throughput.
function summarizeProbe(samples, { prompt_tokens = null, completion_tokens = null, engine_timings = null } = {}) {
  const nonEmpty = samples.filter((s) => s.bytes > 0);
  const outputs = samples.filter((s) => s.has_output || s.has_delta);
  const texts = samples.filter((s) => s.has_text || s.has_delta);
  const reasoning = samples.filter((s) => s.has_reasoning);
  const buffered = nonEmpty.length <= 1;
  const first_output_ms = outputs.length ? outputs[0].t_ms : null;
  const last_output_ms = outputs.length ? outputs[outputs.length - 1].t_ms : null;
  const first_reasoning_ms = reasoning.length ? reasoning[0].t_ms : null;
  const last_reasoning_ms = reasoning.length ? reasoning[reasoning.length - 1].t_ms : null;
  const first_text_ms = texts.length ? texts[0].t_ms : null;
  const last_text_ms = texts.length ? texts[texts.length - 1].t_ms : null;
  const reasoning_duration_ms =
    first_reasoning_ms !== null && last_reasoning_ms !== null ? last_reasoning_ms - first_reasoning_ms : null;
  const reasoning_lead_ms =
    first_reasoning_ms !== null && first_text_ms !== null && first_text_ms >= first_reasoning_ms
      ? first_text_ms - first_reasoning_ms
      : null;
  let stream_decode_tps = null;
  let request_to_first_output_tps = null;
  if (!buffered && completion_tokens !== null && first_output_ms !== null && last_output_ms !== null) {
    const secs = (last_output_ms - first_output_ms) / 1000;
    if (secs > 0) stream_decode_tps = completion_tokens / secs;
  }
  if (!buffered && prompt_tokens !== null && first_output_ms !== null && first_output_ms > 0) {
    request_to_first_output_tps = prompt_tokens / (first_output_ms / 1000);
  }
  const engine_decode_tps = Number.isFinite(engine_timings?.decode_tps) ? engine_timings.decode_tps : null;
  const engine_prompt_tps = Number.isFinite(engine_timings?.prompt_tps) ? engine_timings.prompt_tps : null;
  return {
    first_output_ms,
    last_output_ms,
    first_reasoning_ms,
    last_reasoning_ms,
    reasoning_duration_ms,
    reasoning_lead_ms,
    first_text_ms,
    last_text_ms,
    // Backward-compatible names: TTFT is first visible text, not first output.
    ttft_ms: first_text_ms,
    last_token_ms: last_text_ms,
    stream_decode_tps,
    request_to_first_output_tps,
    engine_decode_tps,
    engine_prompt_tps,
    decode_tps: engine_decode_tps ?? stream_decode_tps,
    prefill_tps: engine_prompt_tps,
    decode_tps_source: engine_decode_tps === null ? (stream_decode_tps === null ? null : "sse_estimate") : "llama.cpp",
    buffered,
  };
}

// Never print undefined/NaN: missing values render as "n/a".
function fmt(v, digits = 2) {
  if (v === null || v === undefined) return "n/a";
  if (typeof v === "number") {
    if (!Number.isFinite(v)) return "n/a";
    return v.toFixed(digits);
  }
  return String(v);
}
function fmtInt(v) {
  if (v === null || v === undefined) return "n/a";
  if (typeof v === "number" && Number.isFinite(v)) return String(Math.round(v));
  return String(v);
}

// ---------------------------------------------------------------------------
// Compare mode (pure; unit-asserted by --self-test)
// ---------------------------------------------------------------------------

// Metrics compared by --compare: key in a bench row, label for the table,
// and which direction counts as "better".
const COMPARE_METRICS = [
  { key: "load_secs", label: "load_s", higherBetter: false, digits: 1 },
  { key: "first_output_ms", label: "first_output_ms", higherBetter: false, digits: 0 },
  { key: "ttft_ms", label: "ttft_ms", higherBetter: false, digits: 0 },
  { key: "decode_tps", label: "decode_tps", higherBetter: true, digits: 2 },
  { key: "prefill_tps", label: "prefill_tps", higherBetter: true, digits: 2 },
  { key: "cached_prompt_tokens", label: "cached_prompt_tok", higherBetter: true, digits: 0 },
  { key: "engine_gen_tps", label: "engine_gen_tps", higherBetter: true, digits: 2 },
  { key: "acceptance_decode_tps", label: "acceptance_decode_tps", higherBetter: true, digits: 2 },
];

const TIE_FRACTION = 0.02; // ±2% counts as a tie

// Finite number or null — never NaN/undefined downstream.
function numOrNull(v) {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

// Compare one metric between rows A and B. Returns
// {a, b, delta, pct, verdict} where delta = b−a, pct = 100·(b−a)/|a|,
// verdict ∈ "A" | "B" | "tie" | "n/a" (n/a when either side is missing).
function compareMetric(aRow, bRow, spec) {
  const a = numOrNull(aRow ? aRow[spec.key] : null);
  const b = numOrNull(bRow ? bRow[spec.key] : null);
  if (a === null || b === null) return { a, b, delta: null, pct: null, verdict: "n/a" };
  const delta = b - a;
  let pct = null;
  let tie = false;
  if (a === 0) {
    tie = b === 0;
    pct = b === 0 ? 0 : null; // % change from zero is undefined
  } else {
    pct = (delta / Math.abs(a)) * 100;
    tie = Math.abs(delta) / Math.abs(a) <= TIE_FRACTION;
  }
  let verdict;
  if (tie) verdict = "tie";
  else if (delta === 0) verdict = "tie";
  else verdict = spec.higherBetter ? (delta > 0 ? "B" : "A") : (delta < 0 ? "B" : "A");
  return { a, b, delta, pct, verdict };
}

function comparisonKey(row) {
  const target = Number.isFinite(row.prompt_tokens_target) ? `target:${row.prompt_tokens_target}` : "legacy";
  const context = Number.isFinite(row.context) ? `context:${row.context}` : "context:unknown";
  return `${row.profile}\u0000${context}\u0000${target}`;
}

function comparisonLabel(row) {
  return Number.isFinite(row.prompt_tokens_target)
    ? `${row.profile}@${fmtInt(row.prompt_tokens_target)}tok`
    : row.profile;
}

// Index by stable profile label, allocated context, and requested prompt depth.
// This keeps context allocations and real input-depth conditions separate.
function indexRowsByCondition(doc) {
  const map = new Map();
  const rows = doc && Array.isArray(doc.rows) ? doc.rows : [];
  for (const r of rows) {
    if (!r || typeof r.profile !== "string" || !r.profile) continue;
    const key = comparisonKey(r);
    if (!map.has(key)) map.set(key, r);
  }
  return map;
}

function profileControlDifferences(aRow, bRow) {
  const a = aRow?.profile_config;
  const b = bRow?.profile_config;
  if (!a || !b) return null; // Legacy unit fixtures/results lack this metadata.
  const fields = ["configured_context", "extra_flags_count", "extra_flags_sha256"];
  return fields.filter((key) => a[key] !== b[key]);
}

function resultSampleDifferences(doc) {
  if (doc?.config?.benchmark_schema_version !== BENCHMARK_SCHEMA_VERSION) return [];
  const differences = [];
  const runs = doc?.config?.runs;
  const target = doc?.config?.prompt_tokens_requested;
  const rows = doc?.rows;
  const samples = doc?.samples;
  if (!Number.isInteger(runs) || runs < 1 || !Number.isInteger(target) || target < 1 ||
      !Array.isArray(rows) || !rows.length || !Array.isArray(samples)) {
    return ["probe sample metadata missing or invalid"];
  }
  if (doc?.config?.summary_statistic !== "median") differences.push("summary_statistic missing or unsupported");
  if (samples.length !== rows.length * runs) differences.push("probe sample count does not match rows × runs");

  const depthTolerance = Math.max(8, Math.ceil(target * 0.002));
  const seenProfiles = new Set();
  for (const row of rows) {
    if (typeof row?.profile !== "string" || !row.profile || seenProfiles.has(row.profile)) {
      differences.push("profile names missing or duplicated");
      continue;
    }
    seenProfiles.add(row.profile);
    const profileSamples = samples.filter((sample) => sample?.profile === row.profile);
    const seenRuns = new Set();
    let invalidSample = profileSamples.length !== runs;
    for (const sample of profileSamples) {
      if (!Number.isInteger(sample.run) || sample.run < 1 || sample.run > runs || seenRuns.has(sample.run)) {
        invalidSample = true;
      } else {
        seenRuns.add(sample.run);
      }
      if (sample.workload !== doc.config.workload ||
          sample.prompt_tokens_target !== target || row.prompt_tokens_target !== target ||
          sample.prompt_tokens_calibrated !== true ||
          !Number.isFinite(sample.prompt_tokens_tokenized) ||
          Math.abs(sample.prompt_tokens_tokenized - target) > depthTolerance ||
          !Number.isFinite(sample.prompt_tokens) ||
          Math.abs(sample.prompt_tokens - target) > depthTolerance ||
          typeof sample.visible_marker_ok !== "boolean") {
        invalidSample = true;
      }
      if (doc.config.workload === "ram-cache-churn") {
        const churnTarget = doc.config.cache_churn_prompt_tokens_target;
        const seedTokens = sample.cache_setup?.seed?.prompt_tokens;
        const churnTokens = sample.cache_setup?.churn?.prompt_tokens;
        const churnTolerance = Math.max(8, Math.ceil(churnTarget * 0.002));
        if (!Number.isInteger(churnTarget) || !Number.isFinite(seedTokens) ||
            Math.abs(seedTokens - target) > depthTolerance || !Number.isFinite(churnTokens) ||
            Math.abs(churnTokens - churnTarget) > churnTolerance) invalidSample = true;
      }
    }
    if (invalidSample || seenRuns.size !== runs) differences.push(`probe samples incomplete or invalid for ${row.profile}`);

    const passes = profileSamples.filter((sample) => sample.visible_marker_ok === true).length;
    if (row.probe_runs_completed !== runs || row.visible_marker_runs !== runs ||
        row.visible_marker_passes !== passes || row.visible_marker_complete !== (passes === runs)) {
      differences.push(`probe summary metadata does not match samples for ${row.profile}`);
    }
    const summary = aggregateProbeSamples(profileSamples);
    let summaryMismatch = false;
    for (const field of PROBE_NUMERIC_SUMMARY_FIELDS) {
      const actual = row[field];
      const expected = summary[field];
      if (expected === null ? actual !== null : !Number.isFinite(actual) || Math.abs(actual - expected) > 1e-9) {
        summaryMismatch = true;
        break;
      }
    }
    if (!summaryMismatch) {
      const actualTimings = row.engine_timings || {};
      const expectedKeys = Object.keys(summary.engine_timings).sort();
      const actualKeys = Object.keys(actualTimings).sort();
      summaryMismatch = expectedKeys.length !== actualKeys.length ||
        expectedKeys.some((key) => {
          const expected = summary.engine_timings[key];
          const actual = actualTimings[key];
          return expected === null
            ? actual !== null
            : !Number.isFinite(actual) || Math.abs(actual - expected) > 1e-9;
        }) || row.decode_tps_source !== summary.decode_tps_source || row.buffered !== summary.buffered;
    }
    if (summaryMismatch) differences.push(`probe median does not match raw samples for ${row.profile}`);
  }
  return [...new Set(differences)];
}

function runIdentityDifferences(aDoc, bDoc) {
  const differences = [];
  const equalFields = [
    ["model", aDoc?.model, bDoc?.model],
    ["app_version", aDoc?.app_version, bDoc?.app_version],
    ["engine_build", aDoc?.engine_build, bDoc?.engine_build],
    ["engine_commit", aDoc?.engine_commit, bDoc?.engine_commit],
    ["max_tokens", aDoc?.config?.max_tokens, bDoc?.config?.max_tokens],
    ["temperature", aDoc?.config?.temperature, bDoc?.config?.temperature],
    ["reasoning_effort", aDoc?.config?.reasoning_effort, bDoc?.config?.reasoning_effort],
    ["probe_task", aDoc?.config?.probe_task, bDoc?.config?.probe_task],
    ["workload", aDoc?.config?.workload, bDoc?.config?.workload],
    ["prompt_tokens_requested", aDoc?.config?.prompt_tokens_requested, bDoc?.config?.prompt_tokens_requested],
    ["summary_statistic", aDoc?.config?.summary_statistic, bDoc?.config?.summary_statistic],
    ["benchmark_schema_version", aDoc?.config?.benchmark_schema_version, bDoc?.config?.benchmark_schema_version],
    ["visible_output_marker", aDoc?.config?.visible_output_marker, bDoc?.config?.visible_output_marker],
    ["cache_setup_max_tokens", aDoc?.config?.cache_setup_max_tokens, bDoc?.config?.cache_setup_max_tokens],
    ["cache_setup_reasoning_effort", aDoc?.config?.cache_setup_reasoning_effort, bDoc?.config?.cache_setup_reasoning_effort],
    ["runs", aDoc?.config?.runs, bDoc?.config?.runs],
  ];
  for (const [field, a, b] of equalFields) {
    if (a === undefined || a === null || b === undefined || b === null) differences.push(`${field} missing`);
    else if (a !== b) differences.push(`${field} differs (A=${a}, B=${b})`);
  }
  differences.push(...resultSampleDifferences(aDoc).map((item) => `A ${item}`));
  differences.push(...resultSampleDifferences(bDoc).map((item) => `B ${item}`));
  if (aDoc?.config?.workload === "ram-cache-churn" || bDoc?.config?.workload === "ram-cache-churn") {
    const churnA = aDoc?.config?.cache_churn_prompt_tokens_target;
    const churnB = bDoc?.config?.cache_churn_prompt_tokens_target;
    if (!Number.isInteger(churnA) || !Number.isInteger(churnB)) differences.push("cache_churn_prompt_tokens_target missing or invalid");
    else if (churnA !== churnB) differences.push(`cache_churn_prompt_tokens_target differs (A=${churnA}, B=${churnB})`);
  }
  const aEngine = aDoc?.engine_settings;
  const bEngine = bDoc?.engine_settings;
  if (!aEngine || !bEngine) {
    differences.push("engine_settings missing");
  } else {
    for (const field of ["device", "threads", "priority", "speculation_enabled", "batch", "ubatch", "extra_flags_count", "runtime_fingerprint_sha256"]) {
      if (aEngine[field] === undefined || bEngine[field] === undefined) differences.push(`engine_settings.${field} missing`);
      else if (aEngine[field] !== bEngine[field]) differences.push(`engine_settings.${field} differs (A=${aEngine[field]}, B=${bEngine[field]})`);
    }
    if (!/^[a-f0-9]{64}$/i.test(String(aEngine.runtime_fingerprint_sha256 || "")) ||
        !/^[a-f0-9]{64}$/i.test(String(bEngine.runtime_fingerprint_sha256 || ""))) {
      differences.push("engine_settings.runtime_fingerprint_sha256 missing or invalid");
    }
  }
  const aRows = aDoc?.rows;
  const bRows = bDoc?.rows;
  if (!Array.isArray(aRows) || !Array.isArray(bRows) || !aRows.length || !bRows.length) {
    differences.push("profile rows missing");
  } else {
    const complete = (row) => row.profile_config &&
      Number.isFinite(row.profile_config.configured_context) &&
      Number.isFinite(row.profile_config.cache_ram) &&
      Number.isInteger(row.profile_config.extra_flags_count) &&
      typeof row.profile_config.extra_flags_sha256 === "string" &&
      Number.isInteger(row.visible_marker_passes) &&
      Number.isInteger(row.visible_marker_runs) &&
      row.visible_marker_runs > 0 &&
      row.visible_marker_passes >= 0 &&
      row.visible_marker_passes <= row.visible_marker_runs &&
      typeof row.visible_marker_complete === "boolean" &&
      row.visible_marker_complete === (row.visible_marker_passes === row.visible_marker_runs) &&
      Number.isInteger(row.probe_runs_completed) && row.probe_runs_completed === aDoc.config.runs;
    if (aRows.some((row) => !complete(row) || row.visible_marker_runs !== aDoc.config.runs) ||
        bRows.some((row) => !complete(row) || row.visible_marker_runs !== bDoc.config.runs)) {
      differences.push("profile or visible-marker metadata missing or invalid");
    }
  }
  return differences;
}

// Union of profile/depth conditions across both docs: A's order first, then
// B-only extras.
// Robust to missing rows arrays and mismatched key orders.
function compareResults(aDoc, bDoc) {
  const aMap = indexRowsByCondition(aDoc);
  const bMap = indexRowsByCondition(bDoc);
  const conditions = [...aMap.keys()];
  for (const key of bMap.keys()) if (!aMap.has(key)) conditions.push(key);
  return conditions.map((key) => {
    const aRow = aMap.get(key) || null;
    const bRow = bMap.get(key) || null;
    const example = aRow || bRow;
    const profile = comparisonLabel(example);
    let prompt_depth_match = null;
    const calibratedDepth =
      (aRow && Number.isFinite(aRow.prompt_tokens_target)) ||
      (bRow && Number.isFinite(bRow.prompt_tokens_target));
    if (aRow && bRow && calibratedDepth) {
      const promptA = numOrNull(aRow.prompt_tokens);
      const promptB = numOrNull(bRow.prompt_tokens);
      const target = Math.max(
        numOrNull(aRow.prompt_tokens_target) || 0,
        numOrNull(bRow.prompt_tokens_target) || 0
      );
      const tolerance = Math.max(8, Math.ceil(target * 0.002));
      prompt_depth_match = promptA !== null && promptB !== null && Math.abs(promptA - promptB) <= tolerance;
    }
    const profileDifferences = aRow && bRow ? profileControlDifferences(aRow, bRow) : null;
    const visibleMarkerComparable =
      !aRow || !bRow ||
      (aRow.visible_marker_complete === undefined && bRow.visible_marker_complete === undefined) ||
      (aRow.visible_marker_complete === true && bRow.visible_marker_complete === true);
    const metrics = {};
    for (const spec of COMPARE_METRICS) {
      const metric = compareMetric(aRow, bRow, spec);
      metrics[spec.key] = prompt_depth_match === false || (profileDifferences && profileDifferences.length > 0) || !visibleMarkerComparable
        ? { ...metric, delta: null, pct: null, verdict: "n/a" }
        : metric;
    }
    return {
      profile,
      context: numOrNull(example.context),
      inA: aRow !== null,
      inB: bRow !== null,
      prompt_tokens_a: aRow ? numOrNull(aRow.prompt_tokens) : null,
      prompt_tokens_b: bRow ? numOrNull(bRow.prompt_tokens) : null,
      prompt_depth_match,
      profile_differences: profileDifferences,
      visible_marker_passes_a: aRow ? numOrNull(aRow.visible_marker_passes) : null,
      visible_marker_runs_a: aRow ? numOrNull(aRow.visible_marker_runs) : null,
      visible_marker_passes_b: bRow ? numOrNull(bRow.visible_marker_passes) : null,
      visible_marker_runs_b: bRow ? numOrNull(bRow.visible_marker_runs) : null,
      visible_marker_comparable: visibleMarkerComparable,
      cache_ram_a: aRow ? numOrNull(aRow.profile_config?.cache_ram) : null,
      cache_ram_b: bRow ? numOrNull(bRow.profile_config?.cache_ram) : null,
      metrics,
    };
  });
}

function fmtSigned(v, digits = 2) {
  if (v === null || v === undefined) return "n/a";
  if (typeof v !== "number" || !Number.isFinite(v)) return "n/a";
  return (v >= 0 ? "+" : "") + v.toFixed(digits);
}

function fmtPct(v) {
  if (v === null || v === undefined) return "n/a";
  if (typeof v !== "number" || !Number.isFinite(v)) return "n/a";
  return (v >= 0 ? "+" : "") + v.toFixed(1) + "%";
}

// ---------------------------------------------------------------------------
// --self-test: pure-helper assertions, no app needed
// ---------------------------------------------------------------------------

function selfTest() {
  let failures = 0;
  const check = (name, cond, detail = "") => {
    console.log(`${cond ? "PASS" : "FAIL"} ${name}${detail ? ` — ${detail}` : ""}`);
    if (!cond) failures += 1;
  };

  // 1-4. Profile -> context mapping.
  for (const [label, ctx] of [["32k", 32768], ["64k", 65536], ["128k", 131072], ["262k", 262144]]) {
    let got = null;
    try {
      got = normalizeProfile(label).context;
    } catch {}
    check(`profile ${label} -> context ${ctx}`, got === ctx, `got ${got}`);
  }
  // 5. Unknown profiles rejected.
  let rejected = false;
  try {
    normalizeProfile("turbo");
  } catch {
    rejected = true;
  }
  check("unknown profile rejected", rejected, rejected ? "threw as expected" : "did NOT throw");
  // 5b. Real engine ids are resolved (never sent raw to /api/start).
  check(
    "32k resolves to engine id velocidad",
    normalizeProfile("32k").id === "velocidad",
    `got ${normalizeProfile("32k").id}`
  );
  const customProfile = normalizeProfile("32k@velocidad_cache512");
  check(
    "custom profile keeps the comparison condition stable",
    customProfile.label === "32k" && customProfile.id === "velocidad_cache512" && customProfile.context === 32768,
    `got ${JSON.stringify(customProfile)}`
  );
  let badCustomProfileRejected = false;
  try {
    normalizeProfile("32k@Bad/Profile");
  } catch {
    badCustomProfileRejected = true;
  }
  check("invalid custom profile id rejected", badCustomProfileRejected);
  check(
    "same comparison condition cannot be duplicated in one result file",
    duplicateBenchmarkCondition([normalizeProfile("32k"), normalizeProfile("32k@cache512")]) !== null
  );

  // 6. Prompt builder within ±20% of the requested size.
  for (const target of [64, 512]) {
    const p = buildPrompt(target);
    const est = approxTokens(p);
    const lo = target * 0.8;
    const hi = target * 1.2;
    check(
      `prompt ~${target} tokens within ±20%`,
      typeof p === "string" && est >= lo && est <= hi,
      `estimated ${est} tokens for target ${target}`
    );
  }
  check(
    "calibrated probe has a visible-answer task after the filler",
    buildBenchmarkPrompt(buildPromptChars(123)).endsWith(BENCHMARK_TASK),
    "fixed task suffix is preserved"
  );
  check(
    "visible marker accepts the requested answer prefix",
    benchmarkVisibleMarkerFound("READY: answer"),
    "READY marker recognized"
  );
  check(
    "visible marker can be assembled across streamed chunks",
    benchmarkVisibleMarkerFound(["REA", "DY: answer"].reduce((prefix, chunk) => appendVisiblePrefix(prefix, chunk), "")),
    "split marker recognized"
  );
  check(
    "reasoning without visible text does not pass the marker check",
    !benchmarkVisibleMarkerFound("Let me think through this carefully"),
    "reasoning-only output rejected"
  );

  check(
    "prompt calibration scales char seed toward actual tokenizer count",
    scaledPromptChars(2048, 512, 726) === 1444 && buildPromptChars(123).length === 123,
    `scaled=${scaledPromptChars(2048, 512, 726)} chars=${buildPromptChars(123).length}`
  );
  check(
    "RAM-cache churn prompt differs from the measured prompt",
    buildPromptChars(128) !== buildPromptChars(128, CHURN_PROMPT_SENTENCE),
    "distinct deterministic fillers"
  );

  // 7-8. Percentile/mean math on a canned sample.
  const sample = [10, 20, 30, 40, 50];
  check("mean([10,20,30,40,50]) === 30", mean(sample) === 30, `got ${mean(sample)}`);
  check("p50 nearest-rank === 30", percentile(sample, 50) === 30, `got ${percentile(sample, 50)}`);
  check("p100 === 50", percentile(sample, 100) === 50, `got ${percentile(sample, 100)}`);
  check("mean([]) === null", mean([]) === null, `got ${mean([])}`);
  check("median odd sample === 30", median(sample) === 30, `got ${median(sample)}`);
  check("median even sample === 25", median([10, 20, 30, 40]) === 25, `got ${median([10, 20, 30, 40])}`);
  check("median ignores missing values and returns null when empty", median([null, 4, undefined]) === 4 && median([]) === null);
  {
    const mixedSources = aggregateProbeSamples([
      { decode_tps: 24, decode_tps_source: "llama.cpp", buffered: false },
      { decode_tps: 20, decode_tps_source: "sse_estimate", buffered: false },
    ]);
    check(
      "aggregate does not combine engine decode with socket estimates",
      mixedSources.decode_tps === null && mixedSources.decode_tps_source === "mixed",
      `got decode=${mixedSources.decode_tps} source=${mixedSources.decode_tps_source}`
    );
  }

  // 9. SSE usage parser extracts tokens from a canned final chunk.
  const canned =
    `data: {"id":"chatcmpl-1","choices":[{"delta":{"content":"hi"}}]}\n\n` +
    `data: {"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":512,"completion_tokens":256}}\n\n` +
    `data: [DONE]\n`;
  const u = parseSseUsage(canned);
  check(
    "SSE usage parser extracts prompt/completion tokens",
    u.prompt_tokens === 512 && u.completion_tokens === 256,
    `got prompt=${u.prompt_tokens} completion=${u.completion_tokens}`
  );

  // 10. Nulls when usage is absent.
  const empty = parseSseUsage(`data: {"choices":[{"delta":{"content":"hi"}}]}\n\ndata: [DONE]\n`);
  check(
    "SSE usage parser returns nulls when absent",
    empty.prompt_tokens === null && empty.completion_tokens === null,
    `got prompt=${empty.prompt_tokens} completion=${empty.completion_tokens}`
  );
  const nonJson = parseSseUsage("not json at all");
  check(
    "SSE usage parser returns nulls for non-JSON",
    nonJson.prompt_tokens === null && nonJson.completion_tokens === null,
    `got prompt=${nonJson.prompt_tokens} completion=${nonJson.completion_tokens}`
  );

  // 10b. Reasoning is model output but is not user-visible text; final engine
  // timings are parsed independently of the usage object.
  {
    const reasoning = chunkSignals({ choices: [{ delta: { reasoning_content: "let me think" } }] });
    const visible = chunkSignals({ choices: [{ delta: { content: "answer" } }] });
    const timing = timingsFromJsonObject({
      timings: { prompt_n: 700, prompt_ms: 3500, prompt_per_second: 200, predicted_n: 120, predicted_ms: 6000, predicted_per_second: 20 },
    });
    check(
      "reasoning delta counts as output without counting as visible text",
      reasoning.has_output && reasoning.has_reasoning && !reasoning.has_text && visible.has_text && !visible.has_reasoning,
      `reasoning=${JSON.stringify(reasoning)} visible=${JSON.stringify(visible)}`
    );
    check(
      "llama.cpp prompt/decode timings parsed from SSE",
      timing && timing.prompt_tps === 200 && timing.decode_tps === 20 && timing.prompt_ms === 3500,
      `got ${JSON.stringify(timing)}`
    );
  }

  // 11. fmt never leaks undefined/NaN.
  check("fmt guards NaN/undefined", fmt(NaN) === "n/a" && fmt(undefined) === "n/a", `got ${fmt(NaN)}/${fmt(undefined)}`);

  // 12. Synthetic timeline: deltas at 1200/1500/1800 ms, 100 completion tokens.
  {
    const tl = [
      { t_ms: 1200, bytes: 120, has_delta: true, has_output: true, has_text: true },
      { t_ms: 1500, bytes: 140, has_delta: true, has_output: true, has_text: true },
      { t_ms: 1800, bytes: 60, has_delta: true, has_output: true, has_text: true },
    ];
    const m = summarizeProbe(tl, { prompt_tokens: 512, completion_tokens: 100 });
    const okDecode = m.decode_tps !== null && Math.abs(m.decode_tps - 100 / 0.6) < 0.01;
    const okRequestRate = m.request_to_first_output_tps !== null && Math.abs(m.request_to_first_output_tps - 512 / 1.2) < 0.01;
    check(
      "timeline 1200/1500/1800ms -> visible text 1200, decode ~166.7 t/s, no inferred prefill",
      m.ttft_ms === 1200 && m.last_token_ms === 1800 && !m.buffered && okDecode && okRequestRate && m.prefill_tps === null,
      `got ttft=${m.ttft_ms} last=${m.last_token_ms} decode=${m.decode_tps} request-rate=${m.request_to_first_output_tps} prefill=${m.prefill_tps} buffered=${m.buffered}`
    );
  }

  // 13. Single-shot response: buffered:true, null rates.
  {
    const m = summarizeProbe(
      [{ t_ms: 2500, bytes: 4096, has_delta: true }],
      { prompt_tokens: 512, completion_tokens: 100 }
    );
    check(
      "single read -> buffered:true, null rates",
      m.buffered === true && m.decode_tps === null && m.prefill_tps === null,
      `got buffered=${m.buffered} decode=${m.decode_tps} prefill=${m.prefill_tps}`
    );
  }

  // 13b. Engine timings remain valid when HTTP delivery coalesces the response
  // into one read; reasoning latency and visible-text TTFT are still separate.
  {
    const reasoningThenText = [
      { t_ms: 1000, bytes: 200, has_output: true, has_reasoning: true },
      { t_ms: 2000, bytes: 200, has_output: true, has_reasoning: true },
      { t_ms: 2500, bytes: 100, has_output: true, has_text: true, has_delta: true },
      { t_ms: 3000, bytes: 100, has_output: true, has_text: true, has_delta: true },
    ];
    const m = summarizeProbe(reasoningThenText, {
      prompt_tokens: 700,
      completion_tokens: 120,
      engine_timings: { prompt_tps: 200, decode_tps: 24 },
    });
    check(
      "reasoning lead time is separate from first visible text and engine rates",
      m.first_output_ms === 1000 && m.first_reasoning_ms === 1000 && m.reasoning_duration_ms === 1000 && m.reasoning_lead_ms === 1500 &&
        m.ttft_ms === 2500 && m.decode_tps === 24 && m.prefill_tps === 200,
      `got first_output=${m.first_output_ms} first_reasoning=${m.first_reasoning_ms} reasoning=${m.reasoning_duration_ms} text=${m.ttft_ms} decode=${m.decode_tps} prefill=${m.prefill_tps}`
    );
    const buffered = summarizeProbe(
      [{ t_ms: 2500, bytes: 4096, has_output: true, has_text: true }],
      { prompt_tokens: 700, completion_tokens: 120, engine_timings: { prompt_tps: 200, decode_tps: 24 } }
    );
    check(
      "buffered HTTP still reports engine timings but no socket-derived rate",
      buffered.buffered && buffered.decode_tps === 24 && buffered.prefill_tps === 200 && buffered.stream_decode_tps === null,
      `got buffered=${buffered.buffered} decode=${buffered.decode_tps} prefill=${buffered.prefill_tps} stream=${buffered.stream_decode_tps}`
    );
  }

  // 14. Usage arriving in a later read after the deltas still counts,
  // and comment-only reads do not move first/last.
  {
    const tl = [
      { t_ms: 1000, bytes: 5, has_delta: false, has_text: false, has_output: false },
      { t_ms: 1200, bytes: 120, has_delta: true, has_text: true, has_output: true },
      { t_ms: 1800, bytes: 90, has_delta: true, has_text: true, has_output: true },
      { t_ms: 1850, bytes: 110, has_delta: false, has_text: false, has_output: false },
    ];
    const m = summarizeProbe(tl, { prompt_tokens: 64, completion_tokens: 50 });
    const okDecode = m.decode_tps !== null && Math.abs(m.decode_tps - 50 / 0.6) < 0.01;
    check(
      "usage-after-deltas counted, comments ignored",
      m.ttft_ms === 1200 && m.last_token_ms === 1800 && !m.buffered && okDecode,
      `got ttft=${m.ttft_ms} last=${m.last_token_ms} decode=${m.decode_tps} buffered=${m.buffered}`
    );
    const evt =
      `data: {"choices":[{"delta":{"content":"a"}}]}\n\n` +
      `data: {"choices":[],"usage":{"prompt_tokens":64,"completion_tokens":50}}\n\n` +
      `data: [DONE]\n`;
    const uu = parseSseUsage(evt);
    check(
      "usage chunk after deltas parsed",
      uu.prompt_tokens === 64 && uu.completion_tokens === 50,
      `got prompt=${uu.prompt_tokens} completion=${uu.completion_tokens}`
    );
  }

  // 15. Compare: clearly better run wins rate columns (gen 34 vs 30 t/s),
  // lower load/ttft wins the latency columns.
  {
    const a = { rows: [{ profile: "32k", load_secs: 24, first_output_ms: 1900, ttft_ms: 2000, decode_tps: 30, prefill_tps: 10, cached_prompt_tokens: 0, engine_gen_tps: 30, acceptance_decode_tps: 32 }] };
    const b = { rows: [{ profile: "32k", load_secs: 20, first_output_ms: 1400, ttft_ms: 1500, decode_tps: 35, prefill_tps: 12, cached_prompt_tokens: 500, engine_gen_tps: 34, acceptance_decode_tps: 36 }] };
    const [row] = compareResults(a, b);
    const m = row.metrics;
    const okDelta = m.engine_gen_tps.delta === 4 && Math.abs(m.engine_gen_tps.pct - (4 / 30) * 100) < 1e-9;
    check(
      "compare better run wins (gen 34 vs 30)",
      row.profile === "32k" && m.engine_gen_tps.verdict === "B" && m.decode_tps.verdict === "B" &&
        m.load_secs.verdict === "B" && m.ttft_ms.verdict === "B" && okDelta,
      `got gen=${m.engine_gen_tps.verdict}Δ${m.engine_gen_tps.delta} load=${m.load_secs.verdict} ttft=${m.ttft_ms.verdict}`
    );
  }

  // 16. Compare: within ±2% is a tie.
  {
    const a = { rows: [{ profile: "64k", load_secs: 20, first_output_ms: 1200, ttft_ms: 1500, decode_tps: 30, prefill_tps: 10, cached_prompt_tokens: 100, engine_gen_tps: 30, acceptance_decode_tps: 32 }] };
    const b = { rows: [{ profile: "64k", load_secs: 20.2, first_output_ms: 1215, ttft_ms: 1515, decode_tps: 30.3, prefill_tps: 10.1, cached_prompt_tokens: 101, engine_gen_tps: 30.3, acceptance_decode_tps: 32.3 }] };
    const [row] = compareResults(a, b);
    const allTie = COMPARE_METRICS.every((s) => row.metrics[s.key].verdict === "tie");
    check(
      "compare within 2% is a tie",
      allTie,
      `got ${COMPARE_METRICS.map((s) => `${s.key}=${row.metrics[s.key].verdict}`).join(" ")}`
    );
  }

  // Prompt-depth mismatches must never be presented as an A/B comparison.
  {
    const a = { rows: [{ profile: "32k", prompt_tokens_target: 4096, decode_tps: 30 }] };
    const b = { rows: [{ profile: "32k", prompt_tokens_target: 8192, decode_tps: 30 }] };
    const compared = compareResults(a, b);
    check(
      "compare keeps different requested prompt depths separate",
      compared.length === 2 && compared.every((row) => row.inA !== row.inB) &&
        compared.every((row) => row.metrics.decode_tps.verdict === "n/a"),
      `got ${compared.map((row) => `${row.profile}:${row.inA}/${row.inB}`).join(",")}`
    );
  }

  {
    const a = { rows: [{ profile: "32k", prompt_tokens_target: 4096, prompt_tokens: 4096, decode_tps: 30 }] };
    const b = { rows: [{ profile: "32k", prompt_tokens_target: 4096, prompt_tokens: 4000, decode_tps: 30 }] };
    const [row] = compareResults(a, b);
    check(
      "compare refuses same-target runs whose achieved input depths diverge",
      row.prompt_depth_match === false && row.metrics.decode_tps.verdict === "n/a",
      `got depth_match=${row.prompt_depth_match} decode=${row.metrics.decode_tps.verdict}`
    );
  }

  {
    const a = { rows: [{ profile: "32k", context: 32768, prompt_tokens_target: 4096, decode_tps: 30 }] };
    const b = { rows: [{ profile: "32k", context: 65536, prompt_tokens_target: 4096, decode_tps: 31 }] };
    const compared = compareResults(a, b);
    check(
      "compare keeps allocated-context differences separate",
      compared.length === 2 && compared.every((row) => row.inA !== row.inB) &&
        compared.every((row) => row.metrics.decode_tps.verdict === "n/a"),
      `got ${compared.map((row) => `${row.context}:${row.inA}/${row.inB}`).join(",")}`
    );
  }

  {
    const profileConfig = { configured_context: 32768, cache_ram: 0, extra_flags_count: 0, extra_flags_sha256: "same" };
    const a = { rows: [{ profile: "32k", context: 32768, prompt_tokens_target: 4096, prompt_tokens: 4096, profile_config: profileConfig, decode_tps: 30 }] };
    const b = { rows: [{ profile: "32k", context: 32768, prompt_tokens_target: 4096, prompt_tokens: 4096, profile_config: { ...profileConfig, extra_flags_sha256: "different" }, decode_tps: 31 }] };
    const [row] = compareResults(a, b);
    check(
      "compare refuses profile changes outside the measured cache size",
      row.profile_differences?.includes("extra_flags_sha256") && row.metrics.decode_tps.verdict === "n/a",
      `got differences=${row.profile_differences} verdict=${row.metrics.decode_tps.verdict}`
    );
  }

  {
    const a = {
      rows: [{ profile: "32k", context: 32768, prompt_tokens_target: 4096, prompt_tokens: 4096, visible_marker_passes: 0, visible_marker_runs: 3, visible_marker_complete: false, decode_tps: 30 }],
    };
    const b = {
      rows: [{ profile: "32k", context: 32768, prompt_tokens_target: 4096, prompt_tokens: 4096, visible_marker_passes: 3, visible_marker_runs: 3, visible_marker_complete: true, decode_tps: 31 }],
    };
    const [row] = compareResults(a, b);
    check(
      "compare refuses performance claims when the visible task is truncated",
      row.visible_marker_passes_a === 0 && row.visible_marker_passes_b === 3 && row.metrics.decode_tps.verdict === "n/a",
      `got visible=${row.visible_marker_passes_a}/${row.visible_marker_runs_a} vs ${row.visible_marker_passes_b}/${row.visible_marker_runs_b}; decode=${row.metrics.decode_tps.verdict}`
    );
  }

  {
    const fixtureSamples = Array.from({ length: 3 }, (_, index) => ({
      profile: "32k",
      run: index + 1,
      workload: "repeat",
      prompt_tokens_target: 4096,
      prompt_tokens_tokenized: 4096,
      prompt_tokens_calibrated: true,
      prompt_tokens: 4096,
      completion_tokens: 64,
      cached_prompt_tokens: 0,
      visible_marker_ok: true,
      first_output_ms: 1000,
      last_output_ms: 4000,
      first_reasoning_ms: 1000,
      last_reasoning_ms: 1500,
      reasoning_duration_ms: 500,
      reasoning_lead_ms: 1000,
      first_text_ms: 2000,
      last_text_ms: 4000,
      ttft_ms: 2000,
      last_token_ms: 4000,
      decode_secs: 3,
      stream_decode_tps: 64 / 3,
      request_to_first_output_tps: 4096,
      decode_tps: 24,
      decode_tps_source: "llama.cpp",
      prefill_tps: 200,
      engine_timings: { cached_prompt_tokens: 0, decode_tps: 24, prompt_tps: 200 },
      buffered: false,
    }));
    const identityRow = {
      profile: "32k",
      prompt_tokens_target: 4096,
      profile_config: { configured_context: 32768, cache_ram: 0, extra_flags_count: 0, extra_flags_sha256: "empty" },
      ...aggregateProbeSamples(fixtureSamples),
      visible_marker_passes: 3,
      visible_marker_runs: 3,
      visible_marker_complete: true,
    };
    const identity = {
      model: "Qwen3.8-27B-IQ4_XS_4BPW.gguf",
      app_version: "2.1.1",
      engine_build: "10743",
      engine_commit: "adfffbe41",
      engine_settings: {
        device: "Vulkan0",
        threads: null,
        priority: "2",
        speculation_enabled: true,
        batch: 1024,
        ubatch: 512,
        extra_flags_count: 0,
        runtime_fingerprint_sha256: "a".repeat(64),
      },
      config: {
        max_tokens: 64,
        temperature: 0,
        reasoning_effort: "medium",
        probe_task: "same task",
        workload: "repeat",
        prompt_tokens_requested: 4096,
        summary_statistic: "median",
        benchmark_schema_version: BENCHMARK_SCHEMA_VERSION,
        visible_output_marker: BENCHMARK_VISIBLE_MARKER,
        cache_setup_max_tokens: 1,
        cache_setup_reasoning_effort: "none",
        cache_churn_prompt_tokens_target: MAX_CHURN_PROMPT_TOKENS,
        runs: 3,
      },
      rows: [identityRow],
      samples: fixtureSamples,
    };
    check(
      "run identity accepts same model/build/request controls",
      runIdentityDifferences(identity, structuredClone(identity)).length === 0
    );
    const mismatched = structuredClone(identity);
    mismatched.engine_build = "10683";
    check(
      "run identity rejects build mismatch",
      runIdentityDifferences(identity, mismatched).some((item) => item.startsWith("engine_build differs"))
    );
    const changedRuntime = structuredClone(identity);
    changedRuntime.engine_settings.runtime_fingerprint_sha256 = "b".repeat(64);
    check(
      "run identity rejects hidden engine tuning changes",
      runIdentityDifferences(identity, changedRuntime).some((item) => item.startsWith("engine_settings.runtime_fingerprint_sha256 differs"))
    );
    const wrongDepth = structuredClone(identity);
    wrongDepth.samples[1].prompt_tokens = 4000;
    check(
      "run identity checks every sample's achieved prompt depth",
      runIdentityDifferences(identity, wrongDepth).some((item) => item.includes("probe samples incomplete or invalid"))
    );
    const wrongSummary = structuredClone(identity);
    wrongSummary.rows[0].decode_tps += 1;
    check(
      "run identity checks row medians against raw samples",
      runIdentityDifferences(identity, wrongSummary).some((item) => item.includes("probe median does not match raw samples"))
    );
  }

  // 17. Compare: missing metric -> n/a verdict, never NaN; one-sided
  // profiles reported; key order does not matter.
  {
    const a = { rows: [{ profile: "32k", engine_gen_tps: 30 }, { profile: "only-a", engine_gen_tps: 10 }] };
    const b = { rows: [{ profile: "only-b", engine_gen_tps: 11 }, { profile: "32k" }] };
    const rows = compareResults(a, b);
    const r32 = rows.find((r) => r.profile === "32k");
    const rA = rows.find((r) => r.profile === "only-a");
    const rB = rows.find((r) => r.profile === "only-b");
    const g32 = r32.metrics.engine_gen_tps;
    check(
      "compare missing metric is n/a, one-sided profiles kept, order-proof",
      rows.length === 3 && rows[0].profile === "32k" && g32.verdict === "n/a" &&
        g32.delta === null && g32.pct === null && rA.inA && !rA.inB && !rB.inA && rB.inB &&
        rA.metrics.engine_gen_tps.verdict === "n/a",
      `got profiles=${rows.map((r) => r.profile).join(",")} gen32=${g32.verdict} onlyA=inA:${rA.inA}/inB:${rA.inB}`
    );
  }

  if (failures > 0) {
    console.log(`\nself-test: ${failures} assertion(s) FAILED`);
    process.exit(1);
  }
  console.log("\nself-test: all assertions PASS");
}

// ---------------------------------------------------------------------------
// HTTP layer (copied style from tests/smoke.mjs)
// ---------------------------------------------------------------------------

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
  return { key: null, source: "none" };
}

const TIMEOUT_MS = 10_000;
const POLL_INTERVAL_MS = 2000;
const START_WAIT_MS = 30 * 60 * 1000; // 30 min: cold 262K loads take minutes
const STOP_WAIT_MS = 5 * 60 * 1000;
const CHAT_TIMEOUT_MS = 30 * 60 * 1000; // generation of max_tokens takes minutes

function isUnreachable(err) {
  const msg = String((err && err.message) || err || "");
  const code = (err && err.cause && err.cause.code) || "";
  return (
    code === "ECONNREFUSED" ||
    code === "ECONNRESET" ||
    msg.includes("fetch failed") ||
    msg.includes("ECONNREFUSED")
  );
}

function unreachableExit(baseUrl) {
  console.log(`LocalMind is unreachable at ${baseUrl} (is LocalMind running?)`);
  console.log("Bench aborted: the app must be running. The engine was not touched.");
  process.exit(1);
}

function makeClient(baseUrl, apiKey) {
  const auth = apiKey ? { Authorization: `Bearer ${apiKey}` } : {};
  async function call(path, { method = "GET", body, timeout = TIMEOUT_MS } = {}) {
    const ctrl = new AbortController();
    const t = setTimeout(() => ctrl.abort(new Error(`timeout after ${timeout}ms`)), timeout);
    try {
      const res = await fetch(baseUrl + path, {
        method,
        signal: ctrl.signal,
        headers: {
          ...auth,
          ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
        },
        body: body !== undefined ? JSON.stringify(body) : undefined,
      });
      const text = await res.text();
      let json = null;
      try {
        json = JSON.parse(text);
      } catch {}
      return { status: res.status, text, json };
    } finally {
      clearTimeout(t);
    }
  }
  return { call };
}

async function getStatus(client) {
  const r = await client.call("/api/status");
  if (r.status !== 200 || !r.json) throw new Error(`GET /api/status -> ${r.status} ${r.text.slice(0, 200)}`);
  return r.json;
}

// Poll every 2 s until done() returns a value, or throw on timeoutMs.
async function pollEvery(prompt, intervalMs, timeoutMs, fn) {
  const start = Date.now();
  for (;;) {
    const v = await fn();
    if (v !== null && v !== undefined) return v;
    if (Date.now() - start > timeoutMs) throw new Error(`${prompt}: timeout after ${Math.round(timeoutMs / 1000)}s`);
    await new Promise((r) => setTimeout(r, intervalMs));
  }
}

// Streaming chat completion read incrementally. Socket arrival timing records
// first model output, hidden reasoning, and visible text separately. Exact
// prompt/decode timings are captured from llama.cpp's final `timings` object.
async function streamChat(baseUrl, apiKey, { prompt, maxTokens, timeoutMs, reasoningEffort = "medium" }) {
  const auth = apiKey ? { Authorization: `Bearer ${apiKey}` } : {};
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(new Error(`timeout after ${timeoutMs}ms`)), timeoutMs);
  const body = JSON.stringify({
    model: "localmind",
    messages: [{ role: "user", content: prompt }],
    max_tokens: maxTokens,
    temperature: 0,
    reasoning_effort: reasoningEffort,
    stream: true,
    stream_options: { include_usage: true },
  });
  const sendAt = Date.now();
  const samples = [];
  let samples_truncated = false;
  const pushSample = (now, bytes, signals) => {
    if (samples.length >= MAX_SAMPLES) {
      samples_truncated = true;
      return;
    }
    samples.push({
      t_ms: now - sendAt,
      bytes,
      has_delta: signals.has_text,
      has_output: signals.has_output,
      has_text: signals.has_text,
      has_reasoning: signals.has_reasoning,
    });
  };
  let prompt_tokens = null;
  let completion_tokens = null;
  const engine_timings = {};
  let chunkCount = 0;
  let readCount = 0;
  let visiblePrefix = "";
  // Parse complete SSE lines. Content is never persisted, only signal flags
  // and llama.cpp timing counters.
  const scanLines = (lines) => {
    const signals = { has_output: false, has_text: false, has_reasoning: false };
    for (const line of lines) {
      const tr = line.trim();
      if (!tr.startsWith("data:")) continue;
      const data = tr.slice(5).trim();
      if (!data || data === "[DONE]") continue;
      let obj = null;
      try {
        obj = JSON.parse(data);
      } catch {
        continue;
      }
      chunkCount += 1;
      const next = chunkSignals(obj);
      signals.has_output ||= next.has_output;
      signals.has_text ||= next.has_text;
      signals.has_reasoning ||= next.has_reasoning;
      visiblePrefix = appendVisiblePrefix(visiblePrefix, next.text);
      const u = usageFromJsonObject(obj);
      if (u.prompt_tokens !== null) prompt_tokens = u.prompt_tokens;
      if (u.completion_tokens !== null) completion_tokens = u.completion_tokens;
      const timing = timingsFromJsonObject(obj);
      if (timing) {
        for (const [key, value] of Object.entries(timing)) {
          if (value !== null) engine_timings[key] = value;
        }
      }
    }
    return signals;
  };
  try {
    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      signal: ctrl.signal,
      headers: { ...auth, "Content-Type": "application/json", Accept: "text/event-stream" },
      body,
    });
    if (res.status === 401 || res.status === 403) {
      throw new Error(
        `POST /v1/chat/completions -> ${res.status} unauthorized — set LM_KEY or ensure %APPDATA%/LocalMind/gateway.key exists`
      );
    }
    if (res.status !== 200 || !res.body) {
      const text = await res.text().catch(() => "");
      throw new Error(`POST /v1/chat/completions -> ${res.status} ${String(text).slice(0, 200)}`);
    }
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    for (;;) {
      const { done, value } = await reader.read();
      const now = Date.now();
      if (value && value.length) {
        readCount += 1;
        buf += decoder.decode(value, { stream: !done });
        const parts = buf.split("\n");
        buf = parts.pop();
        const signals = scanLines(parts);
        pushSample(now, value.length, signals);
      }
      if (done) break;
    }
    const endAt = Date.now();
    if (buf.trim()) {
      // Trailing bytes without a final newline: parse as one last read.
      readCount += 1;
      const signals = scanLines([buf]);
      pushSample(endAt, buf.length, signals);
      buf = "";
    }
    const m = summarizeProbe(samples, { prompt_tokens, completion_tokens, engine_timings });
    const decode_secs =
      m.first_output_ms !== null && m.last_output_ms !== null
        ? (m.last_output_ms - m.first_output_ms) / 1000
        : null;
    return {
      sendAt,
      endAt,
      readCount,
      chunkCount,
      prompt_tokens,
      completion_tokens,
      first_output_ms: m.first_output_ms,
      last_output_ms: m.last_output_ms,
      first_reasoning_ms: m.first_reasoning_ms,
      last_reasoning_ms: m.last_reasoning_ms,
      reasoning_duration_ms: m.reasoning_duration_ms,
      reasoning_lead_ms: m.reasoning_lead_ms,
      first_text_ms: m.first_text_ms,
      last_text_ms: m.last_text_ms,
      ttft_ms: m.ttft_ms,
      last_token_ms: m.last_token_ms,
      decode_secs,
      stream_decode_tps: m.stream_decode_tps,
      request_to_first_output_tps: m.request_to_first_output_tps,
      engine_timings,
      decode_tps: m.decode_tps,
      prefill_tps: m.prefill_tps,
      decode_tps_source: m.decode_tps_source,
      visible_marker_ok: benchmarkVisibleMarkerFound(visiblePrefix),
      buffered: m.buffered,
      samples,
      samples_truncated,
    };
  } finally {
    clearTimeout(t);
  }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

function printHelp() {
  console.log(`LocalMind performance benchmark harness
Measures engine throughput per context profile (SRS LM-NF-1 / LM-NF-2).

Usage:
  node tests/bench.mjs --profiles 32k,64k,128k,262k [--prompt-tokens 512]
    [--runs 1] [--max-tokens 256] [--workload repeat|ram-cache-churn]
    [--base-url http://127.0.0.1:17860]
    [--out tests/bench-results] [--dry-run] [--self-test] [--stop-after]
  node tests/bench.mjs --compare <a.json> <b.json>

Options:
  --profiles      Comma list of: 32k, 64k, 128k, 262k or <context>@<profile-id>
                  (for example 32k@cache512 to keep the 32k comparison label).
  --prompt-tokens Target prompt size in tokens after model-template/tokenizer
                  calibration (default 512; actual usage is recorded too).
  --runs          Streaming completions per profile (default 1; table metrics
                  are medians across completed runs, with raw samples stored).
  --max-tokens    max_tokens for the probe completion (default 256).
  --workload      repeat uses identical prompts; ram-cache-churn primes a target,
                  displaces it with a distinct prompt, then measures the target.
                  Use the latter to measure --cache-ram host-cache reuse.
  --base-url      LocalMind app URL (default http://127.0.0.1:17860).
  --out           Results directory (default tests/bench-results).
  --dry-run       Print the plan per profile; issue no requests, write nothing.
  --stop-after    Stop the engine when the run finishes (default: leave the
                  engine running with the LAST profile).
  --compare a b  Compare two files only when model/build/request settings match;
                  context, actual prompt depth, and profile controls are checked.
  --help          Print this help.

Per profile the harness verifies the id against GET /api/profiles, records its
cache_ram and a SHA-256 fingerprint of extra_flags without writing the values.
It waits out starting, stops a mismatched running engine, POSTs /api/start, and
requires the reported context to match. It then sends the streaming chat probe,
reads /api/metrics, and records exact llama.cpp prompt/decode timings when
present, plus separate first-output, reasoning, and first-visible-text times.
The ram-cache-churn workload runs an unmeasured target primer and a distinct
prompt before every measured target request, so the A/B probes test host-memory
prompt cache reuse rather than only the active slot's repeated-prefix reuse.

The app proxy streams upstream SSE incrementally (server.rs TeeLogReader),
so per-read arrival times supplement the engine per-request timing object.
Without engine timings, socket-derived decode is labeled as an estimate and
prefill remains n/a. A buffered response still reports valid engine timings.
`);
}

function parseArgs(argv) {
  const o = {
    profiles: ["32k", "64k", "128k", "262k"],
    promptTokens: 512,
    runs: 1,
    maxTokens: 256,
    workload: "repeat",
    baseUrl: "http://127.0.0.1:17860",
    out: "tests/bench-results",
    dryRun: false,
    selfTest: false,
    stopAfter: false,
    compare: null,
    help: false,
  };
  const args = argv.slice(2);
  for (let i = 0; i < args.length; i += 1) {
    const a = args[i];
    const next = () => {
      const v = args[i + 1];
      if (v === undefined) throw new Error(`missing value for ${a}`);
      i += 1;
      return v;
    };
    if (a === "--profiles") o.profiles = next().split(",").map((s) => s.trim()).filter(Boolean);
    else if (a === "--prompt-tokens") o.promptTokens = Number(next());
    else if (a === "--runs") o.runs = Number(next());
    else if (a === "--max-tokens") o.maxTokens = Number(next());
    else if (a === "--workload") o.workload = next();
    else if (a === "--base-url") o.baseUrl = next().replace(/\/+$/, "");
    else if (a === "--out") o.out = next();
    else if (a === "--dry-run") o.dryRun = true;
    else if (a === "--self-test") o.selfTest = true;
    else if (a === "--stop-after") o.stopAfter = true;
    else if (a === "--compare") {
      const x = next();
      const y = args[i + 1] !== undefined && !args[i + 1].startsWith("--") ? args[++i] : undefined;
      if (y === undefined) throw new Error("--compare needs exactly two files: --compare <a.json> <b.json>");
      o.compare = [x, y];
    }
    else if (a === "--help" || a === "-h") o.help = true;
    else throw new Error(`unknown argument "${a}" (see --help)`);
  }
  return o;
}

function stampName(date = new Date()) {
  const p = (n) => String(n).padStart(2, "0");
  return (
    `${date.getFullYear()}${p(date.getMonth() + 1)}${p(date.getDate())}-` +
    `${p(date.getHours())}${p(date.getMinutes())}${p(date.getSeconds())}`
  );
}

function printTable(rows) {
  const cols = [
    ["profile", (r) => r.profile],
    ["context", (r) => fmtInt(r.context)],
    ["cache RAM", (r) => fmtInt(r.profile_config?.cache_ram)],
    ["load s", (r) => fmt(r.load_secs, 1)],
    ["prompt tok", (r) => fmtInt(r.prompt_tokens)],
    ["compl tok", (r) => fmtInt(r.completion_tokens)],
    ["first output ms", (r) => fmtInt(r.first_output_ms)],
    ["first reasoning ms", (r) => fmtInt(r.first_reasoning_ms)],
    ["reasoning lead ms", (r) => fmtInt(r.reasoning_lead_ms)],
    ["first text ms", (r) => fmtInt(r.ttft_ms)],
    ["visible marker", (r) => `${fmtInt(r.visible_marker_passes)}/${fmtInt(r.visible_marker_runs)}`],
    ["cached prompt tok", (r) => fmtInt(r.cached_prompt_tokens)],
    ["decode t/s", (r) => fmt(r.decode_tps)],
    ["decode source", (r) => r.decode_tps_source || "n/a"],
    ["prefill t/s", (r) => fmt(r.prefill_tps)],
    ["buf?", (r) => (r.buffered === null || r.buffered === undefined ? "n/a" : r.buffered === "mixed" ? "mixed" : r.buffered ? "YES" : "no")],
    ["gen_tps", (r) => fmt(r.engine_gen_tps)],
    ["accept tps", (r) => fmt(r.acceptance_decode_tps)],
  ];
  const widths = cols.map(([h, f], i) =>
    Math.max(h.length, ...rows.map((r) => String(f(r)).length), i === 0 ? 7 : 0)
  );
  const line = (cells) => cells.map((c, i) => String(c).padEnd(widths[i])).join("  ");
  console.log(line(cols.map(([h]) => h)));
  console.log(line(cols.map((_, i) => "-".repeat(widths[i]))));
  for (const r of rows) console.log(line(cols.map(([, f]) => f(r))));
}

// Print a B−A comparison table plus a per-profile verdict line.
function printComparison(aPath, bPath, compared) {
  console.log(`compare A=${aPath} B=${bPath} (deltas B−A)`);
  const head = ["condition", "context", "prompt tok A/B", "cache RAM A/B", "visible marker A/B", ...COMPARE_METRICS.flatMap((s) => [`${s.label} delta`, `${s.label} %`])];
  const bodyRows = compared.map((row) =>
    [row.profile, fmtInt(row.context), `${fmtInt(row.prompt_tokens_a)}/${fmtInt(row.prompt_tokens_b)}`, `${fmtInt(row.cache_ram_a)}/${fmtInt(row.cache_ram_b)}`,
      `${fmtInt(row.visible_marker_passes_a)}/${fmtInt(row.visible_marker_runs_a)}:${fmtInt(row.visible_marker_passes_b)}/${fmtInt(row.visible_marker_runs_b)}`,
      ...COMPARE_METRICS.flatMap((s) => {
      const m = row.metrics[s.key];
      return [fmtSigned(m.delta, s.digits), fmtPct(m.pct)];
    })]
  );
  const widths = head.map((h, i) => Math.max(h.length, ...bodyRows.map((r) => r[i].length)));
  const line = (cells) => cells.map((c, i) => String(c).padEnd(widths[i])).join("  ");
  console.log(line(head));
  console.log(line(head.map((_, i) => "-".repeat(widths[i]))));
  for (const r of bodyRows) console.log(line(r));
  for (const row of compared) {
    if (!row.inA || !row.inB) {
      console.log(`verdict ${row.profile}: only in ${row.inA ? "A" : "B"} — no comparison`);
      continue;
    }
    if (row.prompt_depth_match === false) {
      console.log(
        `verdict ${row.profile}: no comparison; actual prompt depth differs or is missing ` +
          `(A=${fmtInt(row.prompt_tokens_a)} B=${fmtInt(row.prompt_tokens_b)})`
      );
      continue;
    }
    if (row.profile_differences?.length) {
      console.log(`verdict ${row.profile}: no comparison; profile controls differ: ${row.profile_differences.join(", ")}`);
      continue;
    }
    const wins = COMPARE_METRICS.map((s) => `${s.label}: ${row.metrics[s.key].verdict}`).join(", ");
    console.log(`verdict ${row.profile}: ${wins} (higher better for rates, lower better for load/ttft, tie within ±2%)`);
  }
}

function readResultFile(path) {
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch (e) {
    throw new Error(`cannot read ${path}: ${e.message}`);
  }
  try {
    return JSON.parse(text);
  } catch (e) {
    throw new Error(`cannot parse ${path}: ${e.message}`);
  }
}
// ---------------------------------------------------------------------------
// Main run
// ---------------------------------------------------------------------------

async function main() {
  let opts;
  try {
    opts = parseArgs(process.argv);
  } catch (e) {
    console.log(`error: ${e.message}`);
    process.exit(1);
  }
  if (opts.help) {
    printHelp();
    return;
  }
  if (opts.selfTest) {
    selfTest();
    return;
  }
  if (opts.compare) {
    const [aPath, bPath] = opts.compare;
    let aDoc;
    let bDoc;
    try {
      aDoc = readResultFile(aPath);
    } catch (e) {
      console.log(`error: ${e.message}`);
      process.exit(1);
    }
    try {
      bDoc = readResultFile(bPath);
    } catch (e) {
      console.log(`error: ${e.message}`);
      process.exit(1);
    }
    const identityDifferences = runIdentityDifferences(aDoc, bDoc);
    if (identityDifferences.length) {
      console.log(`not comparable: ${identityDifferences.join("; ")}`);
      process.exit(1);
    }
    printComparison(aPath, bPath, compareResults(aDoc, bDoc));
    return;
  }

  // Resolve + validate profiles BEFORE any I/O (dry-run included).
  let profiles;
  try {
    profiles = opts.profiles.map(normalizeProfile);
    if (!profiles.length) throw new Error("--profiles is empty");
    const duplicate = duplicateBenchmarkCondition(profiles);
    if (duplicate) throw new Error(`duplicate benchmark condition ${duplicate.label}@${duplicate.context}; run profile variants in separate files`);
  } catch (e) {
    console.log(`error: ${e.message}`);
    process.exit(1);
  }
  if (!Number.isInteger(opts.promptTokens) || opts.promptTokens < 1) {
    console.log("error: --prompt-tokens must be a positive integer");
    process.exit(1);
  }
  if (!["repeat", "ram-cache-churn"].includes(opts.workload)) {
    console.log('error: --workload must be "repeat" or "ram-cache-churn"');
    process.exit(1);
  }
  if (!Number.isFinite(opts.runs) || opts.runs < 1) {
    console.log("error: --runs must be a positive integer");
    process.exit(1);
  }
  if (!Number.isInteger(opts.runs)) {
    console.log("error: --runs must be a positive integer");
    process.exit(1);
  }
  if (!Number.isFinite(opts.maxTokens) || opts.maxTokens < 1) {
    console.log("error: --max-tokens must be a positive number");
    process.exit(1);
  }
  const contextMargin = 16;
  const tooSmall = profiles.find((p) => opts.promptTokens + Math.floor(opts.maxTokens) + contextMargin > p.context);
  if (tooSmall) {
    console.log(
      `error: prompt target ${opts.promptTokens} + max_tokens ${Math.floor(opts.maxTokens)} exceeds ${tooSmall.label} context ${tooSmall.context} with ${contextMargin} token safety margin`
    );
    process.exit(1);
  }

  const promptSeed = buildPrompt(opts.promptTokens);
  const promptEst = approxTokens(promptSeed);

  if (opts.dryRun) {
    console.log(`dry-run: base ${opts.baseUrl}, prompt target ${opts.promptTokens} tokens (uncalibrated char estimate ~${promptEst}), max_tokens ${opts.maxTokens}, runs ${opts.runs}, workload ${opts.workload}`);
    for (const p of profiles) {
      console.log(`- profile ${p.label} (engine id "${p.id}", context ${p.context}):`);
      console.log(`    1. GET /api/status (wait out "starting"; stop engine if running with another profile/context)`);
      console.log(`    2. POST /api/start {"profile":"${p.id}","context":${p.context}}`);
      console.log(`    3. poll GET /api/status every 2 s until running + acceptance_ok (timeout 30 min)`);
      console.log(`    4. POST engine /apply-template + /tokenize to fit prompt target ${opts.promptTokens} tokens`);
      if (opts.workload === "ram-cache-churn") {
        const churnDepth = Math.max(1, Math.min(MAX_CHURN_PROMPT_TOKENS, Math.floor(opts.promptTokens / 2)));
        console.log(`    5. Repeat ${opts.runs}x: seed ${opts.promptTokens}-token target (1 token, reasoning none) -> distinct ${churnDepth}-token churn prompt (1 token) -> measure target (max_tokens ${opts.maxTokens}, reasoning medium)`);
      } else {
        console.log(`    5. POST /v1/chat/completions stream:true x${opts.runs} (temperature 0, max_tokens ${opts.maxTokens})`);
      }
      console.log(`    6. GET /api/metrics right after the probe`);
      console.log(`    would append row to ${opts.out}/bench-<YYYYMMDD-HHMMSS>.json (new file, never overwrite)`);
    }
    console.log(opts.stopAfter ? "would POST /api/stop at the end (--stop-after)" : "would leave the engine running with the LAST profile");
    console.log("dry-run: no requests issued, nothing written.");
    return;
  }

  const { key: API_KEY, source: KEY_SOURCE } = resolveKey();
  console.log(`key source: ${KEY_SOURCE}`);
  const client = makeClient(opts.baseUrl, API_KEY);

  // First contact: fail fast with a clear message if the app is down.
  let initial;
  try {
    initial = await getStatus(client);
  } catch (e) {
    if (isUnreachable(e)) unreachableExit(opts.baseUrl);
    console.log(`error: cannot read /api/status: ${e.message}`);
    process.exit(1);
  }

  let settings = null;
  try {
    const r = await client.call("/api/settings");
    if (r.status === 200 && r.json) settings = r.json;
  } catch {}

  const profileResponse = await client.call("/api/profiles");
  if (profileResponse.status !== 200 || !Array.isArray(profileResponse.json)) {
    throw new Error(`GET /api/profiles -> ${profileResponse.status}; cannot verify benchmark profile IDs`);
  }
  const profileCatalog = new Map(profileResponse.json.map((profile) => [profile.id, profile]));
  for (const p of profiles) {
    const profile = profileCatalog.get(p.id);
    if (!profile) throw new Error(`profile id "${p.id}" is not present in GET /api/profiles`);
    const flags = Array.isArray(profile.extra_flags) ? profile.extra_flags.map(String) : [];
    const configuredContext = numOrNull(profile.context);
    const cacheRam = numOrNull(profile.cache_ram);
    if (configuredContext === null || cacheRam === null) {
      throw new Error(`profile "${p.id}" is missing context or cache_ram metadata`);
    }
    p.profile_config = {
      configured_context: configuredContext,
      cache_ram: cacheRam,
      extra_flags_count: flags.length,
      extra_flags_sha256: createHash("sha256").update(JSON.stringify(flags)).digest("hex"),
    };
  }

  let engineSettings = null;
  try {
    const r = await client.call("/api/config");
    const engine = r.status === 200 ? r.json?.engine : null;
    if (engine && typeof engine === "object") {
      engineSettings = Object.fromEntries(
        ["device", "threads", "priority", "speculation_enabled", "batch", "ubatch"].map((key) => [key, engine[key]])
      );
      engineSettings.extra_flags_count = engine.extra_flags_count;
      engineSettings.runtime_fingerprint_sha256 = engine.runtime_fingerprint_sha256;
    }
  } catch {}

  let apiVersion = null;
  try {
    const r = await client.call("/api/version");
    if (r.status === 200 && r.json) apiVersion = r.json;
  } catch {}

  const startedAt = new Date().toISOString();
  const rows = [];
  const samples = [];
  let anyFailure = false;
  for (const p of profiles) {
    const row = {
      profile: p.label,
      engine_profile_id: p.id,
      workload: opts.workload,
      profile_config: p.profile_config,
      requested_context: p.context,
      context: null,
      model: null,
      load_secs: null,
      prompt_tokens: null,
      completion_tokens: null,
      first_text_ms: null,
      last_text_ms: null,
      last_output_ms: null,
      last_reasoning_ms: null,
      decode_secs: null,
      request_to_first_output_tps: null,
      ttft_ms: null,
      last_token_ms: null,
      decode_tps: null,
      prefill_tps: null,
      buffered: null,
      engine_gen_tps: null,
      engine_prompt_tps: null,
      acceptance_decode_tps: null,
      acceptance_ok: null,
      cache_churn_prompt_tokens_target: null,
      cache_churn_prompt_tokens_tokenized: null,
      cached_prompt_tokens: null,
      probe_runs_completed: 0,
      engine_timings: {},
      visible_marker_passes: 0,
      visible_marker_runs: 0,
      visible_marker_complete: false,
      error: null,
      last_error: null,
    };
    let visibleMarkerPasses = 0;
    const profileSamples = [];
    console.log(`\n=== profile ${p.label} (engine id "${p.id}", context ${p.context}) ===`);
    try {
      // (1) Reconcile current engine state.
      let st = await getStatus(client);
      if (st.status === "starting") {
        console.log("engine is starting; waiting for it to settle...");
        st = await pollEvery("wait-for-settle", POLL_INTERVAL_MS, START_WAIT_MS, async () => {
          const s = await getStatus(client);
          return s.status === "starting" ? null : s;
        });
      }
      if (st.status === "running" && (st.profile !== p.id || Number(st.context) !== p.context)) {
        console.log(`engine running with profile="${st.profile ?? "?"}" context=${st.context ?? "?"}; stopping...`);
        await client.call("/api/stop", { method: "POST", body: {} });
        await pollEvery("wait-for-stop", POLL_INTERVAL_MS, STOP_WAIT_MS, async () => {
          const s = await getStatus(client);
          return s.status === "stopped" ? s : null;
        });
        st = await getStatus(client);
      }

      // (2) Start with the resolved engine profile id (+ explicit context).
      if (st.status !== "running" || st.profile !== p.id) {
        console.log(`POST /api/start {"profile":"${p.id}","context":${p.context}}`);
        const r = await client.call("/api/start", {
          method: "POST",
          body: { profile: p.id, context: p.context },
          timeout: TIMEOUT_MS,
        });
        if (r.status !== 200) throw new Error(`POST /api/start -> ${r.status} ${r.text.slice(0, 300)}`);

        // (3) Poll until running AND acceptance_ok (timeout 30 min).
        const loadStart = Date.now();
        st = await pollEvery("wait-for-running", POLL_INTERVAL_MS, START_WAIT_MS, async () => {
          const s = await getStatus(client);
          if (s.status === "error") throw new Error(`engine error: ${(s.last_error ?? "unknown").toString().slice(0, 500)}`);
          if (s.status === "running" && s.acceptance_ok === true) return s;
          return null;
        });
        row.load_secs = (Date.now() - loadStart) / 1000;
      } else {
        console.log("engine already running with this profile; reusing (load_secs n/a).");
        row.load_secs = null;
      }
      row.acceptance_decode_tps =
        typeof st.decode_tps === "number" && Number.isFinite(st.decode_tps) ? st.decode_tps : null;
      row.acceptance_ok = st.acceptance_ok === undefined ? null : st.acceptance_ok;
      const actualContext = Number(st.context);
      row.context = Number.isInteger(actualContext) ? actualContext : null;
      if (!Number.isInteger(actualContext) || actualContext !== p.context) {
        throw new Error(`engine reported context ${st.context}; expected ${p.context}`);
      }
      row.model = typeof st.model === "string" && st.model ? st.model : null;

      const enginePort = Number(st.port);
      if (!Number.isInteger(enginePort) || enginePort < 1 || enginePort > 65535) {
        throw new Error(`invalid local engine port in /api/status: ${st.port}`);
      }
      const promptPlan = await preparePromptForDepth(
        `http://127.0.0.1:${enginePort}`,
        API_KEY,
        Math.floor(opts.promptTokens),
        Number(st.context) || p.context,
        Math.floor(opts.maxTokens)
      );
      const churnTargetTokens = Math.max(1, Math.min(MAX_CHURN_PROMPT_TOKENS, Math.floor(opts.promptTokens / 2)));
      const churnPromptPlan = opts.workload === "ram-cache-churn"
        ? await preparePromptForDepth(
            `http://127.0.0.1:${enginePort}`,
            API_KEY,
            churnTargetTokens,
            Number(st.context) || p.context,
            1,
            CHURN_PROMPT_SENTENCE,
            "none"
          )
        : null;
      row.prompt_tokens_target = promptPlan.target_tokens;
      row.prompt_tokens_tokenized = promptPlan.tokenized_prompt_tokens;
      row.cache_churn_prompt_tokens_target = churnPromptPlan ? churnTargetTokens : null;
      row.cache_churn_prompt_tokens_tokenized = churnPromptPlan?.tokenized_prompt_tokens ?? null;
      row.prompt_tokenization_iterations = promptPlan.tokenization_iterations;
      row.prompt_tokenization_mode = promptPlan.tokenization_mode;
      row.prompt_tokens_calibrated = promptPlan.within_tolerance;
      console.log(
        `prompt depth target=${promptPlan.target_tokens}, tokenized=${promptPlan.tokenized_prompt_tokens}, ` +
          `calibrated=${promptPlan.within_tolerance} in ${promptPlan.tokenization_iterations} iteration(s)`
      );

      // (4) Streaming probe(s): per-read arrival timing, honest buffered flag.
      for (let run = 0; run < opts.runs; run += 1) {
        console.log(`probe run ${run + 1}/${opts.runs}: streaming chat completion...`);
        let cacheSetup = null;
        if (churnPromptPlan) {
          const seed = await streamChat(opts.baseUrl, API_KEY, {
            prompt: promptPlan.prompt,
            maxTokens: 1,
            timeoutMs: CHAT_TIMEOUT_MS,
            reasoningEffort: "none",
          });
          const churn = await streamChat(opts.baseUrl, API_KEY, {
            prompt: churnPromptPlan.prompt,
            maxTokens: 1,
            timeoutMs: CHAT_TIMEOUT_MS,
            reasoningEffort: "none",
          });
          cacheSetup = {
            seed: {
              prompt_tokens: seed.prompt_tokens,
              completion_tokens: seed.completion_tokens,
              cached_prompt_tokens: seed.engine_timings.cached_prompt_tokens ?? null,
            },
            churn: {
              prompt_tokens: churn.prompt_tokens,
              completion_tokens: churn.completion_tokens,
              cached_prompt_tokens: churn.engine_timings.cached_prompt_tokens ?? null,
            },
          };
          console.log(
            `  cache setup: target seed ${fmtInt(seed.prompt_tokens)} tokens -> ` +
              `churn ${fmtInt(churn.prompt_tokens)} tokens -> target measurement`
          );
        }
        const s = await streamChat(opts.baseUrl, API_KEY, {
          prompt: promptPlan.prompt,
          maxTokens: Math.floor(opts.maxTokens),
          timeoutMs: CHAT_TIMEOUT_MS,
        });
        const sample = {
          profile: p.label,
          run: run + 1,
          prompt_tokens_target: promptPlan.target_tokens,
          prompt_tokens_tokenized: promptPlan.tokenized_prompt_tokens,
          prompt_tokens_calibrated: promptPlan.within_tolerance,
          prompt_tokenization_iterations: promptPlan.tokenization_iterations,
          prompt_tokenization_mode: promptPlan.tokenization_mode,
          workload: opts.workload,
          cache_setup: cacheSetup,
          prompt_tokens: s.prompt_tokens,
          completion_tokens: s.completion_tokens,
          cached_prompt_tokens: s.engine_timings.cached_prompt_tokens ?? null,
          visible_marker_ok: s.visible_marker_ok,
          first_output_ms: s.first_output_ms,
          last_output_ms: s.last_output_ms,
          first_reasoning_ms: s.first_reasoning_ms,
          last_reasoning_ms: s.last_reasoning_ms,
          reasoning_duration_ms: s.reasoning_duration_ms,
          reasoning_lead_ms: s.reasoning_lead_ms,
          first_text_ms: s.first_text_ms,
          last_text_ms: s.last_text_ms,
          ttft_ms: s.ttft_ms,
          last_token_ms: s.last_token_ms,
          decode_secs: s.decode_secs,
          stream_decode_tps: s.stream_decode_tps,
          request_to_first_output_tps: s.request_to_first_output_tps,
          decode_tps: s.decode_tps,
          decode_tps_source: s.decode_tps_source,
          prefill_tps: s.prefill_tps,
          engine_timings: s.engine_timings,
          buffered: s.buffered,
          reads: s.readCount,
          samples_truncated: s.samples_truncated,
          samples: s.samples,
          ended_at: new Date(s.endAt).toISOString(),
        };
        samples.push(sample);
        profileSamples.push(sample);
        if (s.visible_marker_ok) visibleMarkerPasses += 1;
        Object.assign(row, aggregateProbeSamples(profileSamples));
        row.visible_marker_passes = visibleMarkerPasses;
        row.visible_marker_runs = profileSamples.length;
        row.visible_marker_complete = profileSamples.length === opts.runs && visibleMarkerPasses === opts.runs;
        if (s.buffered) {
          console.log(
            `  run ${run + 1}: prompt=${fmtInt(s.prompt_tokens)} completion=${fmtInt(s.completion_tokens)} ` +
            `BUFFERED in one read; marker=${s.visible_marker_ok ? "ok" : "missing"}; engine decode=${fmt(s.engine_timings.decode_tps)} t/s ` +
              `prefill=${fmt(s.engine_timings.prompt_tps)} t/s; socket estimate=n/a`
          );
        } else {
          console.log(
            `  run ${run + 1}: prompt=${fmtInt(s.prompt_tokens)} completion=${fmtInt(s.completion_tokens)} ` +
              `first output=${fmtInt(s.first_output_ms)}ms reasoning=${fmtInt(s.first_reasoning_ms)}ms ` +
              `reasoning lead=${fmtInt(s.reasoning_lead_ms)}ms ` +
              `first text=${fmtInt(s.ttft_ms)}ms marker=${s.visible_marker_ok ? "ok" : "missing"} ` +
              `decode=${fmt(s.decode_tps)} t/s (${s.decode_tps_source || "n/a"}) ` +
              `prefill=${fmt(s.prefill_tps)} t/s`
          );
        }
      }

      // (5) Engine metrics right after the probe.
      try {
        const m = await client.call("/api/metrics");
        if (m.status === 200 && m.json) {
          row.engine_gen_tps =
            typeof m.json.gen_tps === "number" && Number.isFinite(m.json.gen_tps) ? m.json.gen_tps : null;
          row.engine_prompt_tps =
            typeof m.json.prompt_tps === "number" && Number.isFinite(m.json.prompt_tps) ? m.json.prompt_tps : null;
        } else {
          console.log(`GET /api/metrics -> ${m.status} (recorded as absent)`);
        }
      } catch (e) {
        console.log(`GET /api/metrics failed: ${e.message} (recorded as absent)`);
      }
    } catch (e) {
      // Never silently skip: record the failure verbatim, continue.
      row.error = String((e && e.message) || e);
      try {
        const s = await getStatus(client);
        row.last_error = s.last_error === undefined || s.last_error === null ? null : String(s.last_error);
      } catch {}
      anyFailure = true;
      console.log(`profile ${p.label} FAILED: ${row.error}`);
    }
    rows.push(row);
  }

  if (opts.stopAfter) {
    console.log("\n--stop-after: stopping the engine...");
    try {
      await client.call("/api/stop", { method: "POST", body: {} });
    } catch (e) {
      console.log(`POST /api/stop failed: ${e.message}`);
    }
  } else if (rows.length) {
    console.log(`\nLeaving the engine running with the LAST profile (${profiles[profiles.length - 1].label}).`);
  }

  console.log("\n--- results ---");
  console.log("Probe token and timing columns use medians across completed runs; missing values stay n/a.");
  printTable(rows);

  // Write the full JSON result (new file, never overwrite).
  mkdirSync(opts.out, { recursive: true });
  const base = `bench-${stampName()}.json`;
  let dest = join(opts.out, base);
  for (let n = 1; existsSync(dest); n += 1) {
    dest = join(opts.out, base.replace(/\.json$/, `-${String(n).padStart(2, "0")}.json`));
  }
  const payload = {
    tool: "tests/bench.mjs",
    started_at: startedAt,
    finished_at: new Date().toISOString(),
    base_url: opts.baseUrl,
    key_source: KEY_SOURCE,
    config: {
      profiles: opts.profiles,
      prompt_tokens_requested: Math.floor(opts.promptTokens),
      summary_statistic: "median",
      prompt_tokens_estimated: promptEst,
      runs: opts.runs,
      max_tokens: opts.maxTokens,
      temperature: 0,
      reasoning_effort: "medium",
      probe_task: BENCHMARK_TASK,
      workload: opts.workload,
      benchmark_schema_version: BENCHMARK_SCHEMA_VERSION,
      visible_output_marker: BENCHMARK_VISIBLE_MARKER,
      cache_setup_max_tokens: 1,
      cache_setup_reasoning_effort: "none",
      cache_churn_prompt_tokens_target: opts.workload === "ram-cache-churn"
        ? Math.max(1, Math.min(MAX_CHURN_PROMPT_TOKENS, Math.floor(opts.promptTokens / 2)))
        : null,
      stop_after: opts.stopAfter,
    },
    model: rows.find((row) => row.model)?.model ?? null,
    app_version: apiVersion?.app ?? null,
    engine_build: apiVersion?.engine_build ?? null,
    engine_commit: apiVersion?.engine_commit ?? null,
    engine_settings: engineSettings,
    settings,
    rows,
    samples,
  };
  writeFileSync(dest, JSON.stringify(payload, null, 2), "utf8");
  console.log(`\nfull result written to ${dest}`);

  if (anyFailure) {
    console.log("bench finished WITH FAILURES (see rows with error above); exit 1.");
    process.exit(1);
  }
}

process.exit(await main().catch((e) => {
  if (isUnreachable(e)) unreachableExit("(base URL)");
  console.log(`fatal: ${(e && e.message) || e}`);
  process.exit(1);
}));
