# LocalMind smoke test (read-only)

`tests/smoke.mjs` — health check of the LocalMind HTTP API on loopback.
Read-only: the script never starts/stops the engine, and its only POST
(check 14) is a malformed `/api/start` body the server rejects with 400
before touching anything. Contract: `docs/SRS.md` §3.5 +
`src-rust/src/server.rs`. No external dependencies, Node >= 18 (global
`fetch`).

## What it checks

1. `GET /` → 200, `Content-Type` contains `text/html`, body contains
   `<html` (or the embedded fallback marker
   `LocalMind Studio: Cargando interfaz...` from
   `src-rust/ui_fallback.html`).
2. `GET /localmind.ico`, `GET /localmind.png`, `GET /omni.ico`,
   `GET /omni.png` → 200 or 404 (both accepted, the script prints which).
3. `GET /api/status` → 200 JSON with the `ServerStatus` shape
   (`process.rs:27-38`): requires `status` (string, one of
   `stopped|starting|running|error`); `idle_remaining_secs` is
   `skip_serializing_if None`, so it is accepted ABSENT (idle engine,
   printed as `absent (motor en reposo)`) or as a number — present and
   non-numeric → FAIL. Never fails for an idle engine.
4. `GET /api/hardware` → 200 JSON with a `gpus` array (`HardwareInfo`,
   `process.rs:48-52`); asserts array presence, length >= 0.
5. `GET /api/profiles` → 200 JSON array, length >= 1.
6. `GET /api/settings` → 200 JSON with `config_path`, `http_port`,
   `llama_port` (`server.rs:45-53`).
7. `GET /api/models` → 200 JSON array; each entry has `name: string`
   and `size_gb: number` (`ModelInfo`, `process.rs:19-24`).
8. `GET /api/logs` → 200 JSON array, length <= 250.
9. `GET /api/metrics` → reads `/api/status` first and asserts the
   matching branch: engine up → 200; engine down → 502
   `{"error":"engine_down"}` (`server.rs:328-333`). Prints the branch.
10. `OPTIONS /api/start` with `Origin: http://localhost` → preflight
    answered (2xx + `Access-Control-Allow-Origin`); prints status and
    the `Access-Control-*` headers.
11. `GET /definitely-not-a-route` → 404 `{"error":"not_found"}`
    (`server.rs:649`).
12. `GET /api/events` → within 8 s either a `data:` event line arrives
    or the stream is open as `text/event-stream` with no event yet;
    aborted after the check via `AbortController`. Prints the branch.
13. `GET /v1/models` → 200 JSON with a `data` array whose ids include
    the CLI aliases (`localmind`, `qwen3.8-27b`); when the server
    enforces the key and none is known, 401/403 is the expected branch.
    Prints the branch and the ids found.
14. `POST /api/start` with `{"context":"abc"}` (wrong type) → 400 with an
    `error` containing `JSON inválido`. This is the script's only POST: the
    request is rejected before the engine is touched, so it starts no engine
    and mutates no state. It guards the fix that stopped a malformed body from
    silently booting the engine with defaults and answering 200.
Every check prints `PASS/FAIL <check> — <observed>`. Exit code 0 only
if every check passed, else 1 plus a summary block of failures with
raw observed status/body (truncated to 400 chars).

Unreachable base URL prints `is LocalMind running?`.

## How to run it

```bash
node tests/smoke.mjs [baseUrl]   # default http://127.0.0.1:17860
```

At startup the script prints one line stating the key source in use:
`key source: LM_KEY | gateway.key | none`.

Sin clave no se envía credencial; los checks protegidos responden 401
(la app exige clave por la puerta D-7):
```bash
node tests/smoke.mjs
```

Env-key mode:

```bash
LM_KEY=<key> node tests/smoke.mjs
# Windows cmd: set LM_KEY=<key> && node tests/smoke.mjs
```

File-key mode (automatic fallback): when `LM_KEY` is unset, the script
reads `%APPDATA%/LocalMind/gateway.key` (`process.env.APPDATA`,
whitespace/newline trimmed, ignored if empty or absent).

A known key is sent as `Authorization: Bearer <key>` on every request.
If any check receives 401/403, its FAIL line says:
`401 unauthorized — set LM_KEY or ensure
%APPDATA%/LocalMind/gateway.key exists (the server now requires the
key)`.


## How to capture fixtures

Flag-based (preferred) — after the checks, captures the raw bodies of
`GET /api/status`, `/api/hardware`, `/api/profiles`, `/api/settings`,
`/api/models`, `/v1/models` into `tests/fixtures/<name>.json`
(`v1-models.json` for `/v1/models`), pretty-printed with 2 spaces:

```bash
node tests/smoke.mjs --write-fixtures [--force] [baseUrl]
# flags may come before or after baseUrl
```

- Refuses to overwrite an existing fixture unless `--force` is passed
  (prints `SKIP <file> — exists` otherwise).
- Each body is parsed with `JSON.parse` first; unparseable bodies are
  skipped and reported, never written (no partial/invalid files — the
  write goes to a temp file + atomic rename).
- 401/403 or non-200 responses are skipped with the reason printed.
- Fixture capture never affects the check results or the exit code.

Manual fallback (curl + python):

```bash
BASE=http://127.0.0.1:17860
for n in status hardware profiles settings models; do
  curl -s "$BASE/api/$n" | python -m json.tool > "tests/fixtures/$n.json"
done
curl -s "$BASE/v1/models" | python -m json.tool > tests/fixtures/v1-models.json
```

(`curl.exe` on Windows; `python -m json.tool` pretty-prints. Send the
key header too if the server enforces it, e.g.
`curl -s -H "Authorization: Bearer $LM_KEY" …`.)

## Known limitation

Read-only salvo el POST de contrato: el único POST es el check 14
(`POST /api/start` con cuerpo mal formado → 400, no toca el motor).
Los checks 9 y 10 solo leen estado / envían OPTIONS, sea cual sea el estado
del motor: el script afirma la rama observada, no la dirige.

## Performance benchmark

`tests/bench.mjs` — reproducible per-profile throughput harness for the
“30 t/s at 262K” target (SRS LM-NF-1 / LM-NF-2). Node ESM, no dependencies,
Node >= 18 (global `fetch`). Key resolution and timeout style copied from
`tests/smoke.mjs`: env `LM_KEY`, else `%APPDATA%/LocalMind/gateway.key`
(trimmed); every HTTP call carries an `AbortSignal` timeout (10 s default,
30 min for the start-wait poll and the streaming probe — a real generation
takes minutes, so a 10 s cap on the probe would abort every measurement).

```bash
node tests/bench.mjs --profiles 32k,64k,128k,262k [--prompt-tokens 512]
  [--runs 1] [--max-tokens 256] [--base-url http://127.0.0.1:17860]
  [--out tests/bench-results] [--dry-run] [--self-test] [--stop-after]
node tests/bench.mjs --compare <a.json> <b.json>
```

CLI shorthands `32k/64k/128k/262k` are translated to the real engine profile
ids (`velocidad`, `multi_doc`, `libros`, `max_contexto`) before calling
`POST /api/start` — the engine silently falls back on unknown ids
(`profiles.rs`), so the raw shorthand is never sent.
For controlled A/B variants, a selector like `32k@velocidad_cache512` uses the
custom profile id while keeping the stable comparison label and requested
context at 32K. The live catalog must contain that id; its `cache_ram` and a
SHA-256 fingerprint of `extra_flags` are saved in the result (flag values are
not written). The app config also provides a SHA-256 fingerprint of engine
runtime settings so batch, polling, power-safe, speculation, device, or hidden
extra-flag changes cannot be silently compared. The comparator requires the
same model, app/engine build, request settings and engine fingerprint; it also
checks allocated context, achieved input depth and profile controls. A cache
size difference is recorded as the planned
variant; other profile control differences make that row non-comparable. Each
result also records whether every probe started its visible answer with `READY`.
The generated answer text is not saved. If either A/B side misses the marker in
any run, its performance metrics are shown as non-comparable.

Per profile, in the given order: wait out `starting`; stop a `running` engine
whose `profile`/`context` differs; `POST /api/start {profile, context}`; poll
`GET /api/status` every 2 s until `running` + `acceptance_ok === true`
(30 min timeout; `error` records the failure verbatim and continues, exit 1
at the end); the requested streaming `POST /v1/chat/completions` probes
(`temperature: 0`, `stream_options.include_usage: true`); `GET /api/metrics`
right after. The
engine is left running with the LAST profile unless `--stop-after` is passed.
`--dry-run` prints the plan and touches nothing; `--self-test` asserts the
pure helpers with no app needed.

`--prompt-tokens` is a target for the full input after the model chat template.
Once the engine is ready, the harness uses its `/apply-template` and `/tokenize`
endpoints to tune the deterministic prompt to that depth; the final response's
`usage.prompt_tokens` is recorded as the achieved count. It refuses a target
that leaves too little room for `max_tokens` inside the allocated context.
The calibrated filler is followed by a fixed instruction to begin a 40-word
answer with `READY`; this gives the probe a visible-text target in addition to
hidden reasoning. The exact task and marker contract are saved in each result
file so different probe tasks are not treated as equivalent. The harness records
only whether the visible stream began with `READY`, not the generated answer;
the comparator refuses to claim a performance winner if any probe in either
variant fails to produce that marker. This catches runs where `max_tokens` ends
inside reasoning before the answer begins.
The `context` table column means allocated context, not used context. The
default 512-token prompt is only a short-prompt baseline, not a full-context
measurement.

To measure decode at multiple real prompt depths, reuse the same profile and
run a sequence of target depths, then compare only runs with the same target:

```bash
node tests/bench.mjs --profiles 32k --prompt-tokens 4096 --runs 3
node tests/bench.mjs --profiles 32k --prompt-tokens 16384 --runs 3
node tests/bench.mjs --profiles 32k --prompt-tokens 28672 --runs 3
```

The engine stays loaded between matching profile/context runs unless
`--stop-after` is passed. Each command writes a separate JSON result; the
comparator matches both profile and requested prompt depth, so a depth mismatch
cannot be mistaken for an A/B performance change.

With `--runs N`, the table and row-level probe timings are medians across all N
completed probes; the JSON also retains every raw probe. Missing values in any
run stay `n/a` in the median instead of being silently dropped. The comparator
checks every sample's calibrated and achieved prompt depth, visible `READY`
marker, run count, and that the saved row median matches its raw samples.
`load_secs` and the startup acceptance rate are single startup observations;
`/api/metrics` is a snapshot taken after the final probe, not a per-run median.

Example host-memory prompt-cache A/B, after creating both profiles in the
isolated benchmark configuration with the same 32K context and flags. Use
`ram-cache-churn`: repeating the exact same prompt can reuse the active slot and
does not isolate the `--cache-ram` host cache. Each measured request is preceded
by a 1-token target seed and a distinct 256-token prompt to displace the active
target. Keep the same gateway process for both files (the engine itself stops
after each run), so its global engine settings cannot drift between A and B:

```bash
node tests/bench.mjs --profiles 32k@velocidad_cache0 --prompt-tokens 4096 --runs 3 --workload ram-cache-churn --stop-after --out tests/bench-results/engine-audit/A-cache0
node tests/bench.mjs --profiles 32k@velocidad_cache512 --prompt-tokens 4096 --runs 3 --workload ram-cache-churn --stop-after --out tests/bench-results/engine-audit/B-cache512
node tests/bench.mjs --compare <A-result.json> <B-result.json>
```

The run files remain incomparable if model/build/request settings differ, actual
context or achieved input depth diverges, or the profile flags differ. The RAM
precheck remains active for both conditions. The comparison reports
`cached_prompt_tokens` alongside throughput; this shows whether the host cache
actually reused the target after the churn request. A result with any missing
visible `READY` marker is not used to claim a performance winner.

Capture resource pressure beside each A/B result in a second terminal. The
sampler writes timestamped JSONL every 5 s with free/committed RAM, CPU, GPU
engine utilization, adapter dedicated/shared memory, and `llama-server.exe`
working set and per-process GPU memory when Windows exposes those counters. It
does not start or stop the engine and does not infer temperature, clocks, or
power:

```powershell
node tests/pressure-sampler.mjs --duration 900 --base-url http://127.0.0.1:17860 --out tests/bench-results/engine-audit/A-cache0/pressure.jsonl
# In another terminal, run the A benchmark. Repeat with a separate B-cache512 path.
```

Match the sampler and benchmark by timestamps and keep each pressure file beside
its benchmark JSON. This makes peak RAM/VRAM and GPU utilization attributable to
the same profile and request sequence.

Table columns: `profile` (CLI shorthand), `context` (allocated context tokens),
`load s` (start to `running` + `acceptance_ok`, `n/a` when the engine was reused),
`prompt tok` / `compl tok` (final SSE `usage` chunk), `first output ms`
(request start to first reasoning, content, or tool delta), `first reasoning ms`,
`reasoning lead ms` (first reasoning to first visible text), `TTFT ms`
(first user-visible text), `decode t/s` (llama.cpp per-request
`predicted_per_second`; when missing, an SSE estimate over first-output to
last-output time), `decode source` (`llama.cpp` or `sse_estimate`), `visible
marker` (successful visible task starts / measured runs), `cached prompt tok`
(llama.cpp reported reused prompt tokens), and
`prefill t/s` (llama.cpp per-request `prompt_per_second`; otherwise `n/a`).
`buf?` is `YES` when the response arrived in one HTTP read: socket-derived
timings are then unavailable, while engine timings remain valid. `gen_tps` is
from `GET /api/metrics`; the gate's `request_tps` in `/api/status` is
completion tokens divided by the full request duration. `decode_tps` is kept
separate and comes only from llama.cpp's `timings.predicted_per_second`; it is
null when the engine does not return that field.

The acceptance gate is only a startup response threshold: one of at most two
200-token probes can pass, and its request rate includes reading the response
body. It does not prove that the discrete GPU was selected or substitute for
the decode rate reported by the engine.

Request-to-first-output time includes gateway, scheduling, prompt evaluation,
and any reasoning before output; it is not pure prefill time. `TTFT` remains
the time to first visible text, so the delay caused by reasoning is visible.
Exact prompt/decode rates come from the same SSE response final `timings`
object. `request_to_first_output_tps` and `stream_decode_tps` are separately
recorded estimates when only socket arrival timing is available. Rates without
supporting token counts/timings are `null` in JSON / `n/a` in the table, never
`0`. A one-shot response is flagged explicitly (`buffered: true` in JSON,
`YES` in the `buf?` column) without discarding valid engine timings. Failing
profiles are recorded (`error` + verbatim `last_error`), never skipped
silently. Full JSON (config + rows + raw per-run samples + timestamps +
`/api/settings` if reachable) goes to a new
`<out>/bench-<YYYYMMDD-HHMMSS>.json` file (dir created; existing files never
overwritten). Each result carries engine timings plus per-read samples
`[{t_ms, bytes, has_output, has_text, has_reasoning}]`, capped at 500 entries
(`samples_truncated: true` when capped). Generated text is never written to
the benchmark file. Missing status keys are treated as absent, never printed
as `undefined`/`NaN`.

Streaming: the app proxy forwards upstream SSE incrementally (`server.rs`
`proxy::TeeLogReader`: pass-through reads, only a 64 KiB tail kept for the
usage log). Per-read timestamps describe response arrival through the proxy;
llama.cpp timings describe the isolated model request.

## Local model quality checks

```bash
node tests/engine-quality.mjs --prompt-tokens 4096
node tests/engine-quality.mjs --compare tests/quality-results/quality-A.json tests/quality-results/quality-B.json
node tests/engine-quality.mjs --self-test
```

This harness is read-only: it requires an already-running model with
`acceptance_ok === true` and never starts or stops the engine. It checks exact
arithmetic, identifier extraction, JSON shape, output ordering, and retrieval
of a needle among 80 records. Each prompt is padded and calibrated with the
loaded model's `/apply-template` + `/tokenize`; requests use temperature 0,
seed 42, and `reasoning_effort: medium`. The final `usage.prompt_tokens` is
checked against the target before a result counts in an A/B score. The JSON
stores each answer so failures can be reviewed; do not add private data to the
fixed prompts. This is a small regression set, not a general model benchmark.

## Comparing two runs

`node tests/bench.mjs --compare <a.json> <b.json>` reads two result files
(same shape as `tests/bench-results/*.json`) and prints, per profile present
in either file, the B−A deltas and percentage changes for `load_s`,
`first_output_ms`, `ttft_ms`, `decode_tps`, `prefill_tps`, `engine_gen_tps` and
`acceptance_decode_tps`, plus a verdict line per profile naming the winner
per metric (`A`/`B`/`tie`; higher wins the rate columns, lower wins
load/ttft; within ±2% is a tie). Missing keys print `n/a` (never
`NaN`/`undefined`); profiles present in one file only are listed with an
`only in A/B — no comparison` verdict; key order does not matter. Exit 0
when the table is produced, 1 with a clear message when either file cannot
be read or parsed. Runs no engine, needs no app:

Rows match by profile and prompt-token target. If final `usage.prompt_tokens`
counts differ by more than 0.2% (with an 8-token minimum tolerance), the
comparator reports `n/a` instead of declaring a winner at different input
depths.

```bash
node tests/bench.mjs --compare tests/bench-results/bench-a.json tests/bench-results/bench-b.json
```
