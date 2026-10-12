#!/usr/bin/env node
// LocalMind pressure sampler (Node ESM, no dependencies).
// Samples every 5 s for a given duration and writes JSONL: free/committed RAM,
// page-file usage, CPU total + llama-server.exe CPU/RAM, GPU engine
// utilization, Windows per-adapter/per-engine GPU memory, and gen_tps from
// /api/metrics when the app answers. GPU memory counters report MiB converted
// from Windows performance-counter bytes; clocks/power/temperature are not
// exposed by these generic counters and are not inferred.
// Starts nothing itself: run ALONGSIDE a harness run.
//
// Usage: node tests/pressure-sampler.mjs --duration 600 [--out <file>]
//   [--base-url http://127.0.0.1:17860]  (default out: tests/harness-bench/results/pressure-<ts>.jsonl)

import { execSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

const ARGS = process.argv.slice(2);
function flag(name, def) {
  const i = ARGS.indexOf(name);
  return i >= 0 && i + 1 < ARGS.length ? ARGS[i + 1] : def;
}
const DURATION_S = Number(flag("--duration", "600"));
const BASE = (flag("--base-url", "http://127.0.0.1:17860") || "").replace(/\/+$/, "");
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const OUT =
  flag("--out", "") ||
  join(dirname(resolve(process.argv[1])), "harness-bench", "results", `pressure-${stamp}.jsonl`);

function resolveKey() {
  const env = (process.env.LM_KEY || "").trim();
  if (env) return env;
  try {
    const p = join(process.env.APPDATA || "", "LocalMind", "gateway.key");
    if (existsSync(p)) {
      const k = readFileSync(p, "utf8").trim();
      if (k) return k;
    }
  } catch {}
  return null;
}
const KEY = resolveKey();

// Collect Windows telemetry in one PowerShell process. The previous sampler
// spawned separate shells for CPU, memory, GPU engine, and VRAM on every tick;
// their startup overhead made the nominal 5 s sampling interval inaccurate.
function windowsSample() {
  const script = [
    "$os=Get-CimInstance Win32_OperatingSystem",
    "$corePaths=@('\\Memory\\% Committed Bytes In Use','\\Paging File(_Total)\\% Usage','\\Processor(_Total)\\% Processor Time')",
    "$core=(Get-Counter -Counter $corePaths -SampleInterval 1 -MaxSamples 1).CounterSamples",
    "$gpu=@(); try{$gpuPaths=@('\\GPU Engine(*)\\Utilization Percentage','\\GPU Adapter Memory(*)\\Dedicated Usage','\\GPU Adapter Memory(*)\\Shared Usage','\\GPU Process Memory(*)\\Dedicated Usage','\\GPU Process Memory(*)\\Shared Usage');$gpu=(Get-Counter -Counter $gpuPaths -MaxSamples 1).CounterSamples}catch{}",
    "$proc=Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue | Select-Object -First 1",
    "$comm=$core | Where-Object {$_.Path -like '*% Committed Bytes In Use'} | Select-Object -First 1",
    "$page=$core | Where-Object {$_.Path -like '*% Usage'} | Select-Object -First 1",
    "$cpu=$core | Where-Object {$_.Path -like '*% Processor Time'} | Select-Object -First 1",
    "$mem=@('FreeMB='+[math]::Round($os.FreePhysicalMemory/1KB)+' TotalMB='+[math]::Round($os.TotalVisibleMemorySize/1KB),'CommittedPct='+[math]::Round($comm.CookedValue,1),'PagePct='+[math]::Round($page.CookedValue,1),'CpuPct='+[math]::Round($cpu.CookedValue,1))",
    "$gpuTop=@($gpu | Where-Object {$_.Path -like '*GPU Engine*'} | Sort-Object CookedValue -Descending | Select-Object -First 3 | ForEach-Object {$_.InstanceName+'='+[math]::Round($_.CookedValue,1)})",
    "$adapters=@{};$engineMemory=@{};$prefix='';$enginePid=$null;if($proc){$prefix='pid_'+$proc.Id+'_';$enginePid=$proc.Id}",
    "foreach($s in $gpu){$scope=$null;if($s.Path -like '*GPU Adapter Memory*'){$scope=$adapters}elseif($s.Path -like '*GPU Process Memory*' -and $proc -and $s.InstanceName.StartsWith($prefix)){$scope=$engineMemory}else{continue};$kind=if($s.Path -like '*Dedicated Usage'){'dedicated'}else{'shared'};if(!$scope.ContainsKey($s.InstanceName)){$scope[$s.InstanceName]=@{luid=$s.InstanceName}};$scope[$s.InstanceName][$kind+'_mb']=[math]::Round([double]$s.CookedValue/1MB,1)}",
    "$gpuMemory=[ordered]@{engine_pid=$enginePid;adapters=@($adapters.Values);engine=@($engineMemory.Values)}",
    "$llama=$null;if($proc){$llama=[ordered]@{pid=$proc.Id;cpu_s=$proc.CPU;working_set_mb=[math]::Round($proc.WorkingSet64/1MB,0)}}",
    "[ordered]@{mem=$mem;gpu_top3=$gpuTop;gpu_memory=$gpuMemory;llama_server=$llama} | ConvertTo-Json -Compress -Depth 5",
  ].join(";");
  try {
    const out = execSync(`powershell.exe -NoProfile -Command "${script}"`, {
      timeout: 20000,
      windowsHide: true,
    }).toString("utf8");
    return JSON.parse(out.trim());
  } catch {
    return null;
  }
}

async function metrics() {
  if (!KEY) return null;
  try {
    const ctl = new AbortController();
    const t = setTimeout(() => ctl.abort(), 8000);
    const r = await fetch(`${BASE}/api/metrics`, {
      headers: { Authorization: `Bearer ${KEY}` },
      signal: ctl.signal,
    });
    clearTimeout(t);
    if (r.status !== 200) return null;
    return await r.json();
  } catch {
    return null;
  }
}

mkdirSync(dirname(resolve(OUT)), { recursive: true });
console.log(`pressure: ${DURATION_S}s @5s -> ${OUT}`);
const end = Date.now() + DURATION_S * 1000;
let n = 0;
while (Date.now() < end) {
  const t0 = Date.now();
  const rec = { ts: new Date().toISOString(), n: n++ };
  const win = windowsSample();
  rec.mem = win?.mem ?? [];
  rec.gpu_top3 = win?.gpu_top3 ?? [];
  rec.gpu_memory = win?.gpu_memory ?? null;
  rec.llama_server = win?.llama_server ?? null;
  const m = await metrics();
  rec.gen_tps = m && typeof m.gen_tps === "number" ? m.gen_tps : null;
  rec.cached = m && typeof m.prompt_tokens_cached === "number" ? m.prompt_tokens_cached : null;
  rec.requests_processing = m && typeof m.requests_processing === "number" ? m.requests_processing : null;
  appendFileSync(OUT, JSON.stringify(rec) + "\n");
  const spent = Date.now() - t0;
  const wait = 5000 - spent;
  if (wait > 0 && Date.now() + wait < end + 1000) await new Promise((r) => setTimeout(r, Math.min(wait, Math.max(0, end - Date.now()))));
}
console.log(`pressure: wrote ${n} samples`);
