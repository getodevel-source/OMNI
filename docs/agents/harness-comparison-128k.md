# Comparativa de harnesses a 128K (OpenCode, DeepSeek, Pi, OMP)
> **CORRECCIÓN 2026-10-11 — `--cache-ram` no controla el KV activo.** El
> build 10743 lo define como límite de caché de prompts en RAM. Las mediciones
> y propuestas de este documento pueden conservar sus tiempos observados, pero
> quedan retractadas las atribuciones de memoria DDR5, offload KV o cuello de
> bus que se basaron en ese flag. Ver `docs/engine-audit-2026-10.md`.

## Estado del documento

- Final: tabla de campaña a 128K (8 runs, máquina contenida), tabla fair en
  quietud, tabla después de los arreglos (§ "Después": load-mode none + caché +
  effort low; omp 16,8 / pi 10,5 / opencode 1,0 / deepseek 5,7; ninguno >30),
  desglose de costo por harness (§ "Desglose": tablas antes/después por pata,
  top-3 del wall y qué cambiar para 30 t/s), metodología, gate-vs-control, TTFT
  vs nivel de razonamiento, truncamiento en 127, presión de RAM (§ "Presión"),
  integración por ruta de la app (§ "Integración"), disclaimers de contención,
  y curva t/s vs profundidad (§ "Curva": solo el punto ~512 es válido; 8k colgó
  el prefill ~300 s, 32k omitido por inestabilidad).
- Pendiente: puerta de aceptación histórica (32,7 / 32,47 / 14,46 / 15,13 / 14,83 —
  ver § "Varianza entre cargas" para la explicación por rama lenta/rápida), y el
  timeout de reacción de opencode (se repitió también con la caché puesta).
  Medido el 2026-09-28 (§ "Deriva térmica y `finish_reason` (ventana 2026-09-28)"):
  sin droop térmico en ~3 min sostenidos a 128K; `finish_reason:length` directo
  (2000 tokens, pasa del 127) — el corte en 127 de los harnesses sigue abierto
  por lado harness (falta `finish_reason` por pata).

Carga única compartida: perfil `libros`, contexto 131072, modelo Qwen3.8-27B-IQ4_XS.
Arranque: `POST /api/start {"profile":"libros","context":131072}` aceptado de inmediato
(`{"status":"starting","pid":4140}`); `running` + `acceptance_ok:true` a los ~60 s
(listo a las 12:21:09 tras pedirlo a las 12:20:09). El `decode_tps` visible entonces
(13,35) era el valor de la carga ANTERIOR (el status conserva el último gate; ver
§ "Gate vs control: resuelto").
Orden secuencial: pi-reacción, control, omp-reacción, omp-throughput, pi-throughput,
opencode-reacción, opencode-throughput, deepseek-reacción, deepseek-throughput.
Motor parado al final (`POST /api/stop` → `{"status":"stopped"}`).
Evidencia bruta: `tests/harness-bench/results/*.json` (8 ficheros).
Script reutilizable: `tests/harness-bench.mjs` (Node ESM, sin dependencias).

## Tabla medida (contendido: campaña original, thinking max/default)

Campaña original con la máquina contenida (workers + sesiones del dueño activos).
Definición de "¿supera 30 t/s?": `effective_tps = completion_tokens / gateway_ms`
agregado por run (INCLUYE prefill — el `ms` del gateway cubre la request completa).
`gen_tps` = decode puro del motor (`GET /api/metrics`) justo después del run.
NINGÚN harness supera 30 t/s en `effective_tps`; el control directo SÍ (42,6 t/s decode).

| Harness (versión) | Reacción 1er byte | Reacción wall / tok | Throughput wall / 1er byte | Prompt+compl (tput) | eff t/s (prefill incl.) | gen_tps (decode) | reqs | ¿>30 t/s? |
|---|---|---|---|---|---|---|---|---|
| pi 0.87.0 | 235,5 s | 235,5 s / 20747+467 (4 req) | 277,2 s / 277,2 s | 132153+2844 (19 req) | 10,3 | 10,7 | 19 | NO |
| omp 18.3.1 | 1,4 s | 335,4 s / 7732+20 (1 req) | 67,4 s / 0,6 s | 7749+1128 (1 req) | 16,9 | 18,2 | 1 | NO |
| opencode 1.18.10 | 1,9 s | 287,3 s / 12870+251 (2 req) | 386,3 s / 2,2 s | 12906+1583 (2 req) | 3,7 | 11,8 | 2 | NO |
| deepseek 0.1.5-rc.3 | 199,4 s | 199,4 s / 9869+95 (2 req) | 301,4 s / 198,1 s | 9900+1253 (2 req) | 4,1 | 11,2 | 2 | NO |
| **control directo** (gateway, stream) | TTFT 74,4 s | 131,1 s / 73+2418 (1 req) | — | 73+2418 | — (decode 42,6) | — | 1 | SÍ (decode) |

Detalle por pata (gateway `ms` → t/s por request):

- pi reacción (4 req, eff agregado 2,0): 4907+159/204973 ms (0,8 t/s), 5148+119/12486 (9,5),
  5281+118/9867 (12,0), 5411+71/6790 (10,5). Primera pata con el prefill frío de ~5k.
- omp reacción (1 req, eff 0,06): 7732+20/333813 ms. Prefill frío de 7,7k domina.
- omp throughput (1 req, eff 16,9, gen 18,2): 7749+1128/66592 ms. Respuesta 1–127, corte a
  mitad (el modelo paró en 127 de 300; salida 1092 B).
- pi throughput (19 req, eff agregado 10,3, gen 10,7): NO escribió los números a stdout;
  creó `numeros.txt` con los 300 vía herramientas y resumió en texto (stdout 219 B).
  Patas de ~5k→8,3k prompt, 60–305 completion cada una.
- opencode reacción (2 req, eff 0,8): 592+221/38941 (5,7 t/s), 12278+30/281946 (0,1).
  Segunda pata = prefill frío de 12,3k.
- opencode throughput (2 req, eff 3,7, gen 11,8): 608+394/43790 (9,0), 12298+1189/381156 (3,1).
  Respuesta 1–127, corte igual que omp.
- deepseek reacción (2 req, eff 0,5): 166+64/9831 (6,5), 9703+31/196392 (0,2). Prefill frío 9,7k.
- deepseek throughput (2 req, eff 4,1, gen 11,2): 181+64/7221 (8,9), 9719+1189/299189 (4,0).
  Respuesta 1–127, mismo corte.
- control (1 req): prompt 73, completion 2418, TTFT 74,4 s, decode 42,6 t/s, `ms` gateway 131105.


## Tabla fair (quietud, `--fair --thinking low --warmup`, orden inverso)

Ventana de integración+medición en máquina quieta (CPU media 5,7 % durante el run;
agentes del dueño presentes pero idle; una sola carga 128K sin reinicio, wall total
2661 s). Evidencia: `tests/harness-bench/results/2026-09-25T17-54-26-616Z.json`
(2026-09-25T17:54:26Z, `libros`, ctx 131072). Puerta de ESA carga: 14,46 t/s
(`acceptance_ok:true`) — ver nota sobre varianza de la puerta más abajo.
Orden: deepseek → opencode → omp → pi. Pi/omp con `--thinking low` explícito;
opencode/deepseek sin flag equivalente (defaults propios). `--warmup` = un request
mínimo con thinking off antes de cada harness (lo medido es TTFT caliente).

| Harness (versión) | Reacción 1er byte | Reacción wall / tok | Throughput wall / 1er byte | Prompt+compl (tput) | eff t/s | gen_tps | reqs | ¿>30 t/s? |
|---|---|---|---|---|---|---|---|---|
| omp 18.3.1 | 0,6 s | 331,6 s / 7732+17 (1 req) | 66,1 s / 0,6 s | 7749+1128 (1 req) | 17,3 | 18,8 | 1 | NO |
| pi 0.87.0 | 205,3 s | 205,3 s / 20498+356 (4 req) | 125,7 s / 125,7 s | 78932+1482 (14 req) | 11,8 | 15,3 | 14 | NO |
| opencode 1.18.10 | 1,8 s | **timeout 600 s, ok=false** (1 req 592+28) | 208,7 s / 2,1 s | 25180+2432 (3 req) | 2,4 | 18,3 | 3 | NO |
| deepseek 0.1.5-rc.3 | 482,1 s | 482,2 s / 9869+81 (2 req) | 544,2 s / 471,8 s | 9902+1374 (2 req) | 5,6 | 17,6 | 2 | NO |
| **control directo** (300 números, `max_tokens` 1500) | primer byte 159,4 s | último 207,7 s / 1208 tok | — | — | 5,8 (con prefill) | **25,0 decode** (1208 tok en 48,35 s puros) | 1 | NO (ni decode) |

Lectura: **ni un harness supera 30 t/s a 128K bajo ninguna de las dos definiciones,
y el techo de decode del motor en esa carga fue ~25 t/s** (puerta 14,46; `gen_tps`
de los harnesses 15–19; control decode 25,0). La salida de "reacción" de pi no fue
el OK literal sino chit-chat (run exit 0; comportamiento del agente, no del bench).
**Hallazgo abierto: la pata de reacción de opencode falló por timeout (600 s,
ok=false, 1 req 592+28 registrado)** — no es "no medido", es un fallo a investigar
(la pata de throughput del mismo harness funcionó: 3 req, 208,7 s).

Detalle por pata (fair, gateway `ms` → t/s por request):

- omp throughput (1 req, eff 17,3, gen 18,8): 7749+1128/65351 ms.
- omp reacción (1 req, eff 0,1): 7732+17/330827 ms. Mismo prefill frío que en
  contenido, reacción dominada por prefill.
- pi throughput (14 req, eff 11,8, gen 15,3): patas 4896+110/25898 (4,2) y luego
  12,9–15,7 t/s estables (p. ej. 5628+102/7025 = 14,5; 6432+118/8114 = 14,5).
  Primera pata fría, resto calientes y parejas.
- pi reacción (4 req, eff 1,7): 4896+128/185706 (0,7), 5106+80/7262 (11,0),
  5200+84/6233 (13,5), 5296+64/5509 (11,6). Misma forma: una fría + resto ~11-13.
- opencode throughput (3 req, eff 2,4, gen 18,3): 12278+24/669179 (0,0),
  608+1163/132428 (8,8), 12294+1245/204606 (6,1). Primera pata = prefill frío 12,3k.
- deepseek throughput (2 req, eff 5,6, gen 17,6): 181+64/5656 (11,3),
  9721+1310/241394 (5,4). Segunda pata = prefill frío 9,7k.
- deepseek reacción (2 req, eff 0,5): 166+64/6842 (9,4), 9703+17/168786 (0,1).
- Nota: los `gen_tps` 15–19 de esta ventana superan los 10–12 de la campaña
  contenida — la contención sí pesaba (~5 puntos de decode).

## Después de los arreglos (load-mode none + caché de prompt + effort low)

Ventana en máquina quieta (CPU 1 % al arrancar, libre 9877/15466 MB; agentes del
dueño presentes pero idle). Una sola carga 128K `libros` (pid 6236, lista 23:54:50,
~33 s de carga; puerta `decode_tps = 14,83`, `acceptance_ok:true`). Mismo protocolo
fair (`--fair --thinking low --warmup --profile libros --context 131072 --no-control`,
orden deepseek → opencode → omp → pi; wall total 3127 s ≈ 52 min). Evidencia:
`tests/harness-bench/results/2026-09-28T03-47-01-315Z.json`.
Motor parado al final (`POST /api/stop` → `stopped`, sin `llama-server.exe`).

| Harness | Reacción 1er byte | Reacción wall / tok | Throughput wall / 1er byte | Prompt+compl (tput) | eff t/s | gen_tps | cached post | reqs | ¿>30? |
|---|---|---|---|---|---|---|---|---|---|
| omp 18.3.1 | 0,6 s | 324,0 s / 7733+22 (1 req) | 67,2 s / 0,5 s | 7751+1119 (1 req) | 16,8 | 18,2 | 27801 | 1 | NO |
| pi 0.87.0 | 188,4 s | 188,4 s / 14943+198 (3 req) | 161,5 s / 161,5 s | 86436+1688 (15 req) | 10,5 | 15,8 | 123463 | 15 | NO |
| opencode 1.18.10 | 1,6 s | **timeout 600 s, ok=false** (1 req 592+39) | 783,1 s / 2,1 s | 25181+1449 (3 req) | 1,0 | 17,4 | 20138 | 3 | NO |
| deepseek 0.1.5-rc.3 | 472,0 s | 472,1 s / 9869+99 (2 req) | 526,2 s / 458,1 s | 9906+1306 (2 req) | 5,7 | 17,8 | 19548 | 2 | NO |

Antes → después (eff t/s): omp 17,3 → 16,8 (−3 %); pi 11,8 → 10,5 (−11 %);
opencode 2,4 → 1,0 (−58 %); deepseek 5,6 → 5,7 (+2 %).

Lectura honesta: los arreglos NO movieron los efectivos de los harnesses porque el
prefill frío de cada system prompt (5–12k tokens) sigue dominando la primera pata
de cada run, y el throughput agregado promedia esa primera pata fría con el resto.
Lo que sí se movió: (a) el prefill frío por token bajó (modo none: la puerta de
esta carga no sirve de referencia intra-carga, pero el 176 tok/s medido del modo
es el cambio estructural); (b) la caché de prompt deja `cached` alto al final de
cada run (pi 123k, omp 28k, opencode 20k, deepseek 20k) — las patas tardías de pi
llegan a 12–14 t/s una tras otra (legs 11,8–13,7); (c) el effort low ya estaba en
la fair anterior, así que no hay delta de razonamiento entre tablas.
Quien más se beneficia es **pi**: 15 requests por tarea × ~5–6k de prompt cada una
con prefijos que se repiten → casi todas sus patas son hits de caché (11–14 t/s
estables); omp (1 request) no tiene de dónde rascar hits. Ningún harness supera 30
t/s en ninguna definición; el techo de decode de esta carga fueron los `gen_tps`
15–18.

Detalle por pata (después, gateway `ms` → t/s):

- omp throughput: 7751+1119/66535 = 16,8 (gen 18,2). Reacción: 7733+22/323236 = 0,07.
- pi throughput (15 req): primera 4896+76/29448 (2,6), resto 9,0–13,7 estables
  (p. ej. 5575+120/8763 = 13,7; 6668+114/8364 = 13,6). Reacción (3 req):
  4895+71/177839 (0,4), 4980+76/5786 (13,1), 5068+51/4298 (11,9).
- opencode throughput (3 req): 12279+30/656374 (0,05), 608+206/72531 (2,8),
  12294+1213/779427 (1,6). Reacción: **timeout 600 s, ok=false, 1 req 592+39/16744
  (2,3) registrado pero el hijo nunca terminó** — mismo hallazgo abierto que en la
  fair anterior, ahora también con la caché puesta: no es falta de caché, es el
  harness (o el hijo `opencode run`) quedándose colgado en esa pata.
- deepseek throughput: 181+64/5878 (10,9), 9725+1242/223975 (5,6). Reacción:
  166+64/7058 (9,1), 9703+35/157337 (0,2).

La puerta de aceptación varía entre cargas 128K: 32,7 / 32,47 / 14,46 / 15,13 /
14,83 en cinco cargas distintas — probablemente el estado del KV/VRAM al momento
de la puerta; queda como ítem abierto (no invalida las comparaciones intra-carga).

## Curva t/s vs profundidad (carga fresca 128K, vía directa)

Carga fresca `libros`/131072 (pid 15924, lista 15:41:04, ~26 s de carga; puerta
`decode_tps = 15,13`, `acceptance_ok:true`). Los puntos usan el mismo prompt base
(frase fija repetida) + sufijo fijo, `temperature: 0`, `max_tokens: 400`,
streaming por el gateway, sin harness. Sampler de 240 s en paralelo: 40 muestras
(`tests/harness-bench/results/pressure-2026-09-25T18-41-08-757Z.jsonl`).

| Punto | prompt real | cached | completion | `ms` gateway | eff (con prefill) | decode puro | `gen_tps` post |
| ~512 | 745 | 42 | 400 (`length`) | 39932 | 10,0 | 400 tok en 23,1 s (40,0−16,9): **17,3** | 16,0 |
| ~8k | — (sin línea: el request no terminó) | 271 | — | — | — | — | — |
| ~32k | no medido (omitido por inestabilidad, ver abajo) | — | — | — | — | — | — |

Lectura honesta: solo el punto ~512 es válido. El primer delta llegó a los 16,9 s
y todo fueron `reasoning_content` (1730 chars, sin ni un token de contenido;
`finish: length` a los 400). El punto ~8k nunca devolvió headers
(`HeadersTimeoutError` tras ~300 s de prefill; el motor quedó con
`requests_processing: 1` y el sampler lo vio al 98–121 % de compute). El 32k no se
intentó siquiera: con el 8k colgando al motor, empujar 32k habría sido forzar la
máquina. El decode caliente de ~17 t/s a 745 tokens de contexto cuadra con los
`gen_tps` 15–19 de la ventana fair (no con los 42–94 de probes de 2–300 tokens:
la completion aquí son 400 tokens de razonamiento puro, no texto corto).
Caveat frío-vs-cálido confirmado una vez más: 314 s en frío vs 325 ms en caché
para el mismo prompt de 10786 tokens (medido en la campaña anterior).

Sampler con el perfil nuevo activo: inicio **libre 6580 MB, commit 87,6 %, page
9,0 %, llama RSS 3,29 GB, CPU 1,4 %**; mitad libre 4765 MB, commit 89,1 %, compute
98,7 %, RSS 4,88 GB; final libre 4721 MB, commit 89,1 %, CPU 9,3 %, RSS 4,93 GB.
Hay varios GB libres al inicio (mejor que los ~1,4 GB sin el perfil nuevo), pero
el commit sigue ~88–89 % y el 8k cuelga igual: el perfil nuevo alivia la RAM
visible, no el costo del prefill largo a 128K.

## Metodología
- `tests/harness-bench.mjs --harnesses pi,omp,opencode,deepseek --profile libros --context 131072`:
  reutiliza la carga `running` al contexto pedido (no la reinicia), espera una carga ajena
  `starting` en vez de duplicarla, y solo arranca si está `stopped`. Clave como
  `tests/smoke.mjs` (env `LM_KEY`, si no `%APPDATA%/LocalMind/gateway.key`); nunca la imprime.
- Env/argv espejan `src-rust/src/launcher.rs` + `agents.rs` + `server.rs`:
  pi `pi --provider localmind --model localmind/localmind --thinking max -p` (+`OPENAI_*`,
  `PI_CODING_AGENT_DIR`); omp `omp --model localmind/qwen3.8-27b --thinking max -p` (+ mismo env);
  opencode `opencode --model localmind/qwen3.8-27b run` (+ XDG aislados y
  `<home>/config/opencode/opencode.json` con clave inline, como `launch_opencode` — NO por env);
  deepseek `dsh --profile headless` (+ `DSH_HOME`, telemetría off, patch Cordis generado con
  puerto/contexto vivos). Cwds scratch bajo `%TEMP%`; configs en
  `%APPDATA%/LocalMind/agents/<agent>/`.
- Primer byte con `process.hrtime.bigint()` sobre chunks de stdout/stderr (no por líneas).
- `--dry-run` imprime comandos/env exactos (clave redactada) sin tocar nada (ni red);
  `--repeats 2` repite solo runs <180 s (ignorado bajo `--fair`: siempre 1 run por fase).
  `--fair` invierte el orden, fija pi/omp a `--thinking low|off`, añade `--warmup`
  (request mínimo con thinking off antes de cada harness) y registra patas por request
- Salida no-cero si algún harness falla; en la campaña original los 8 runs salieron
  con exit 0; en la ventana fair, 7/8 (reacción de opencode: timeout 600 s, ok=false).

## Caveats honestos

- Overhead de system prompt: pi ~4,9–8,3k, omp ~7,7k, opencode ~12,3k, deepseek ~9,7k prompt
  tokens por request frente a 73 del control. Eso hunde el `effective_tps` (prefill incluido).
- Prefill frío vs cálido: la primera pata grande de cada harness paga el prefill completo
  (0,06–0,8 t/s en reacciones); las patas siguientes reutilizan prefijo (métricas finales:
  `prompt_tokens_cached` hasta 155k). Los runs tardíos van sobre KV-cache más caliente:
  efecto a favor de deepseek (último) imposible de separar con una sola carga.
- omp/opencode/deepseek cortaron la lista en "127" (3 de 3): ver § "Truncamiento en
  127": no es un cap de los harnesses (caps 4–16k frente a ~1,19k observados) ni del
  proxy (control directo con `max_tokens: 2000` llegó a 2000 tokens / 196 líneas);
  probablemente EOS del modelo en esos contextos — marcado NO PROBADO, pendiente de
  re-run en quietud con `finish_reason` registrado. Comparar "t/s" entre una lista
  truncada y 19 turnos de herramienta es aproximado: la métrica común es
  completion/gateway_ms en ambos casos.
- `effective_tps` incluye prefill; `gen_tps` es decode puro post-run. La pregunta "¿30 t/s?"
- Tiempo total de motor ≈ 35 min (235+131+335+67+277+287+386+199+301 s de runs).

## Gate vs control: resuelto

El 13,35 t/s que mostraba `/api/status` al arrancar la campaña era el gate de la
carga ANTERIOR: el status conserva el último valor hasta que la puerta de la nueva
carga termina (observación de producto; no es un bug en sí, pero conviene saberlo).
La puerta de la carga fresca 128K (`libros`, pid 19360, lista 13:11:18) midió
`decode_tps = 32,72` con `acceptance_ok:true`, consistente con los 32,47 de la bench
anterior a 128K. Se retira la hipótesis de "cold-start": la puerta (no-stream,
`Count from 1 to 60`, `max_tokens: 200`) y el control (stream, mismo prompt,
`max_tokens: 300`) midieron en caliente 32,7 y 94,6/94,7 t/s decode respectivamente
(dos probes idénticos seguidos). La diferencia gate-vs-control es el modo de medida
y el tamaño de la completion, no varianza real del motor.

## Reacción / TTFT vs nivel de razonamiento

Probes directos al gateway, prompt trivial (`Responde exactamente: OK`), motor
caliente. Se distingue TTFT al primer delta (`reasoning_content` o `content`) del
TTFT al primer token de CONTENIDO: el modelo razona (~n chars) antes de responder.

| Ajuste | TTFT 1er delta | TTFT 1er contenido | prompt+compl | reasoning | decode t/s |
|---|---|---|---|---|---|
| default (sin campo; lo que mandan los harnesses) | 1,8 / 1,0 s (2 rep) | 1,8 / 1,0 s | 57+28 | ~99 chars | n/a (2 tok) |
| `reasoning_effort: "low"` | 1,2 / 0,9 s (2 rep) | 1,2 / 0,9 s | 45+23 | ~78 chars | n/a (2 tok) |
| thinking off (`chat_template_kwargs: {"enable_thinking": false}`) | 0,5 s | 0,5 s | 17+2 | 0 chars | n/a (2 tok) |
| `reasoning_effort: "max"` | — | — | — | — | HTTP 500: la plantilla Qwen rechaza el mapeo `max` |

Lectura: `enable_thinking:false` SÍ lo honra el motor (prompt 17 vs 57, completion 2
vs 28, `reasoning_content` vacío). El `"max"` que mandan pi/omp por defecto
(`--thinking max`) el proxy hoy lo deja pasar y el motor lo rechaza con 500 —
defecto ya derivado para corrección (el proxy normalizará `max → xhigh`); pi/omp lo
sufrirían en cada request si el gateway lo devolviera en vez de reintentar (en la
campaña los runs salieron con exit 0: el stack lo absorbió aguas arriba).

## Por qué las "reacciones" de los harnesses tardaron 199–335 s

Dos sumandos, ambos medidos:

1. Prefill frío del system prompt (5–12k tokens) con la máquina contendedora: el
   punto depth-8k lo muestra en crudo — mismo prompt de 10786 tokens, `ms` 314222
   en frío frente a 325 ms en caliente con el prefijo en caché (cached 11269).
   Mil veces de diferencia por el caché de prefijo, no por el harness.
2. Fase de razonamiento antes del primer token de contenido (~500 chars en el
   control frío con TTFT-contenido 74,4 s; ~99 chars incluso en probes triviales
   calientes).

Conclusión práctica: en caliente y en quietud estos prompts responden en ~1 s (0,5 s
con thinking off). Lo que hace lenta la primera respuesta es el system prompt propio
de cada harness pagando prefill, y el thinking quita la mayor parte del resto.

## Truncamiento en 127

No es un cap de salida de los harnesses: omp `maxTokens` 16384 (`agents.rs`), opencode
`limit.output` 8192 (`launcher.rs:434`), deepseek `maxTokens` 4096 (patch) — todos muy
por encima de los ~1,19k completion observados. Tampoco es un cap del proxy: el mismo
prompt por vía directa con `max_tokens: 2000` llegó a 2000 tokens / 196 líneas
(`finish: length`, cortado a mitad del "195"), y con `max_tokens: 900` a 900 tokens.
Causa más probable: EOS del modelo en esos contextos de harness (system prompts de
5–12k que desplazan la trayectoria). Re-run directo en quietud 2026-09-28
(ventana `libros`/131072, gate 16,67, `stream:true`, `max_tokens:2000`):
`finish_reason:"length"` verbatim con completion 2000 / prompt 73, llegando al
"195" (cola SSE en `tests/harness-bench/results/finish-reason-128k-1790637136563.txt`).
Conclusión: el motor NO corta en 127 por sí solo; el corte 1–127 de los 6 runs
one-shot (omp/opencode/deepseek) es EOS condicionado por sus system prompts
(o stop del harness), no un cap del proxy/motor.

## Presión durante la ventana fair (medido, no narrativa)

Sampler de 900 s, 142 muestras (`tests/harness-bench/results/pressure-2026-09-25T17-09-30-820Z.jsonl`):
al inicio **RAM libre 1376 MB, commit 87,6 %, page-file 9,1 %, llama RSS 8,19 GB,
CPU 2,2 %**; a mitad commit 88,9 %, GPU compute (pid del motor) 99,6 %;
CPU min/media/max 1,5/5,7/16,8 %. Conclusión medida: **el jank a 128K es presión
de RAM/commit, no CPU** (el CPU casi no se usa). Detalle y palancas en
`docs/agents/engine-pressure.md`.

## Integración por ruta de la app (128K, `POST /api/launch`, uno por uno)

Ventana de integración en máquina quieta, por la ruta real de la app (no el bench
directo): los cuatro harnesses devolvieron 200 con la carga 128K viva.
`agents/pi/models.json` con `apiKey` + gateway `:17860` + ctx 131072 (pi 200, spawn
`wt pid 4776`); `agents/omp/models.yml` con apiKey/gateway/ctx (omp 200);
`agents/opencode/config/opencode/opencode.json` con
`baseURL http://127.0.0.1:17860/v1` y ctx 131072 (opencode 200);
`agents/deepseek/profiles/headless/cordis.patch.yml` con `baseURL :17860/v1` y
`contextWindow 131072` ya generado con valores vivos, no hardcodeado (deepseek 200).
Para los interactivos (pi/omp) la prueba es config+spawn+200 (la tarea se ignora por
diseño); para los one-shot (opencode/deepseek) el tráfico por el gateway quedó
probado por las patas del bench fair. Binario shippeado `db1a1b8f…` (3551232 B) con
el fix de `reasoning_effort` vivo: triple `max`/`minimal`/`medium` sobre el binario
shippeado devolvió 200 con `reasoning_content` y prompts 56/44/14 (el fix funciona).

## Desglose de costo por harness (análisis de los dos JSON fair, sin motor)

Método: cada pata (`legs[]`, una línea `usage.jsonl`) aporta prompt/completion/ms
reales. Dentro de una pata NO se puede separar prefill de decode sin timings del
motor (no están en los records): se acota por los dos extremos. Decode puro por
pata caliente ≈ 12–19 t/s (patas tardías de pi, estables en ambas campañas);
prefill frío ≈ 0,05–0,7 t/s en la primera pata grande de cada run. El "resto"
(patas 2..n) se reporta como decode aproximado completion/ms — es una COTA
INFERIOR del decode real porque incluye su propio prefill del Delta de historial.
`wall − gateway_ms` = tiempo fuera del motor (hijo CLI, herramientas, reintentos).
Ojo: en opencode el wall sale MENOR que el `ms` agregado (wall−ms = −725/−798 s):
sus requests corren en paralelo/solapados dentro del hijo, así que el `ms`
agregado NO es tiempo serial — el efectivo agregado subestima su decode real.
Efectivo necesario para 30 t/s: completion_total / 30 = presupuesto total en ms.

### omp (1 request por tarea; prompt ~7,7k fijo)

| Campaña | reqs | prompt+compl | wall | gateway ms | eff | gen | 1ª pata (fría) | resto |
|---|---|---|---|---|---|---|---|---|
| antes | 1 | 7749+1128 | 66,1 s | 65351 | 17,3 | 18,8 | 100 % (única) | — |
| después | 1 | 7751+1119 | 67,2 s | 66535 | 16,8 | 18,2 | 100 % (única) | — |

Desglose (después): prefill frío 7,7k en ~60 s (la pata entera a 16,8 t/s con solo
1119 de completion es casi todo prefill) + decode ~1119/18 ≈ 62 s solapados dentro
del mismo `ms`; wall−ms = 0,6 s (cero overhead de hijo). Caché post: 27801
(antes 40060; el número baja porque el run de después tocó menos prefijo repetido,
no porque la caché empeore — ver límites).
Top-3 del wall: (1) prefill frío 7,7k ≈ 55–60 s; (2) decode ~60 s entrelazado;
(3) overhead de hijo ≈ 1 s. Para 30 t/s efectivos necesitaría 1119/30 = 37,3 s de
presupuesto total: imposible con 7,7k de prefill frío (~60 s él solo). Solo un hit
de caché del system prompt (prefill ~1 s) lo pondría en ~20–25 t/s; a 30 solo
llega con prompt ≤ ~2k o decode ≥ 40 t/s.

### pi (14–15 requests; prompt 4,9k→6,7k creciente por historial acumulado)

| Campaña | reqs | prompt+compl | wall | gateway ms | eff | gen | 1ª pata (fría) | resto decode aprox |
|---|---|---|---|---|---|---|---|---|
| antes | 14 | 78932+1482 | 125,7 s | 125068 | 11,8 | 15,3 | 25898 (21 %) | 13,8 |
| después | 15 | 86436+1688 | 161,5 s | 160849 | 10,5 | 15,8 | 29448 (18 %) | 12,3 |

Desglose (después): primera pata 4896+76/29448 (2,6 t/s: prefill frío de ~29 s) +
resto 14 patas 5–29 s cada una a 9,0–13,7 estables (p. ej. 5575+120/8763 = 13,7);
wall−ms = 0,6 s (cero overhead: el hijo pisa el acelerador sin parar). Caché post
123463 (antes 133814): ~12–14 t/s en patas 2..15 = hits de caché del prefijo
repetido; la pata 0 fría cuesta ~25 s extra. Caché antes→después: la primera pata
pasó de 25898 a 29448 ms (peor prensa fría en la segunda carga, varianza
intra-carga, no regresión del fix).
Top-3 del wall: (1) 14 pre-fills de 5–6,7k ≈ 130 s agregados (cada uno barato por
caché, pero son 14); (2) decode ~1612/12,3 ≈ 131 s entrelazado; (3) primera pata
fría ~25 s extra. Para 30 t/s efectivos necesitaría 1688/30 = 56 s totales:
imposible con 86k prompt agregados. Camino realista: la mitad de turnos (7–8 en
vez de 15) + prompts de ~3k → ~25k prompt totales ≈ 60–80 s con caché → ~20 t/s;
a 30 solo con turnos ≤5 o decode ≥ 35 t/s.

### opencode (3 requests; prompts 0,6k / 12,3k / 12,3k — sándwich frío)

| Campaña | reqs | prompt+compl | wall | gateway ms | eff | gen | 1ª pata (fría) | resto decode aprox |
|---|---|---|---|---|---|---|---|---|
| antes | 3 | 25180+2432 | 208,7 s | 1006213 | 2,4 | 18,3 | 669179 (67 %) | 7,1 |
| después | 3 | 25181+1449 | 783,1 s | 1508332 | 1,0 | 17,4 | 656374 (44 %) | 1,7 |

Desglose (después): pata 0 (12,3k frío, 656 s) + pata 1 (0,6k, 206 compl, 72 s) +
pata 2 (12,3k, 1213 compl, 779 s). Las dos patas de 12,3k son el 95 % del `ms`.
wall−ms = −725 s: requests solapados, el agregado miente. Caché post 20138
(antes 32397). OJO antes→después: la pata 2 pasó de 204 s a 779 s con el mismo
prompt (12,3k) — varianza de prefill entre cargas, no efecto del fix; el eff 1,0
de después no es comparable al 2,4 de antes porque el denominador lo domina una
pata fría 4× más lenta ese día.
Top-3 del wall: (1) dos pre-fills fríos de 12,3k ≈ 1435 s agregados (¡más que el
wall por el solape!); (2) decode ~1449/17 ≈ 85 s; (3) overhead de hijo ~2 s
(primer byte a 2,1 s). Para 30 t/s efectivos necesitaría 1449/30 = 48 s totales:
imposible con 25k prompt. Camino: un solo request de ~12k con hit de caché
(prefill ~5–10 s) + decode 85 s → ~95 s → ~15 t/s; a 30 solo con prompt ≤ ~3k.

### deepseek (2 requests; 0,2k + 9,7k — todo-o-nada)

| Campaña | reqs | prompt+compl | wall | gateway ms | eff | gen | 1ª pata (fría) | resto decode aprox |
|---|---|---|---|---|---|---|---|---|
| antes | 2 | 9902+1374 | 544,2 s | 247050 | 5,6 | 17,6 | 5656 (2 %) | 5,4 |
| después | 2 | 9906+1306 | 526,2 s | 229853 | 5,7 | 17,8 | 5878 (3 %) | 5,5 |

Desglose (después, casi idéntico a antes): pata 0 (0,2k, 64 compl, 5,9 s, 10,9
t/s) + pata 1 (9,7k frío, 1242 compl, 224 s, 5,5 t/s). La pata grande es el 97 %
del `ms`; wall−ms = 296 s (hijo dsh + orchestración Cordis entre patas, el mayor
overhead de los cuatro). Caché post 19548 (antes 19544: idéntico — el prefijo no
se reutiliza entre sus 2 requests lo bastante para que la caché muerda).
Top-3 del wall: (1) prefill frío 9,7k ≈ 200 s; (2) overhead hijo/orchestración
≈ 296 s (¡más que el prefill!); (3) decode ~1306/17 ≈ 77 s. Para 30 t/s
efectivos necesitaría 1306/30 = 44 s totales: imposible con prefill de 200 s +
overhead de 296 s. Camino: el overhead del harness es su primer problema (más de
la mitad del wall está fuera del motor); después, hit de caché del 9,7k.

### Conclusión por harness (qué cambiar para 30 t/s efectivos)

- **omp**: 1 request de 7,7k. El prefill frío (~60 s) es el 90 % del costo. Con
  hit de caché del system prompt → ~20–25 t/s. A 30: prompt ≤2k o decode ≥40.
- **pi**: 15 requests × ~5–6k. Cada pata es barata con caché (12–14), pero son
  15. A 30: ≤5–7 turnos o prompts de ~3k. Es el que más gana con la caché porque
  casi todo su costo es prefijo repetido.
- **opencode**: sándwich 0,6k/12,3k/12,3k con dos pre-fills fríos gigantes y
  requests solapados (el `ms` agregado no es serial). A 30: un solo request con
  caché o prompts ≤3k. La varianza entre cargas (204 s vs 779 s para el mismo
  12,3k) domina cualquier comparación antes/después.
- **deepseek**: 2 requests donde el 97 % del costo es una pata de 9,7k + 296 s
  de overhead del hijo (el mayor de los cuatro). A 30: primero matar el overhead
  (296 s fuera del motor), después caché del 9,7k. Sin tocar el harness no llega.

### Datos que faltan para afinar (no estimados en silencio)

- Timings de prefill vs decode dentro de cada pata (el motor no los expone por
  request; solo `gen_tps` global post-run y `prompt_tokens_cached` final).
- `prompt_tokens_cached` por pata (solo hay el valor final post-run; en reacción
  no hay `gen_tps` ni caché registrados).
- `finish_reason` por pata (para el truncamiento en 127 y el timeout de opencode).
- Contenido/timing de los 296 s de overhead de deepseek (hijo dsh vs orchestración).
- El `ms` agregado de opencode no es serial (wall < ms): su eff agregado no es
  comparable con los otros tres sin traza de solape.

## Contención y qué significan los números (campaña original)

Todo el follow-up se midió con sesiones propias del dueño activas (`omp`, `opencode`,
Orca) más la campaña en curso; la lectura GPU final en idle quedó <6% de uso. Las
cifras de throughput son cotas inferiores. Techo real a 128K de asignación con prompt
corto: probe caliente de 300 tokens a **94,6 t/s decode** (dos repeticiones idénticas).

## Qué significa esto para el dueño (actualizado con la ventana fair)

Para respuestas rápidas: contexto real corto, nivel de razonamiento bajo o thinking
off, y perfiles 32–64K cuando el documento lo permita. Tras los arreglos (ventana
fair en quietud, load-mode none + caché + effort low): omp 16,8 / pi 10,5 /
opencode 1,0 / deepseek 5,7 — **ningún harness supera 30 t/s ni en efectivo ni en
decode** (patas calientes 11–19, techo de la carga `gen_tps` 15–18). El que más se
beneficia de la caché es pi (15 requests con prefijos repetidos → 11–14 estables).
Si quiere velocidad: prompts cortos, thinking bajo/off, y aceptar que a 128K el
jank medido es RAM/commit (ver § "Presión"), no CPU.

## Protocolo de ventana libre (~10 min, copiar-pegar)

Preparado mientras la máquina está ocupada; NADA pesado ejecutado para escribirlo
(`node --check` + `--dry-run` + un sampler de 12 s contra el gateway parado).
Orden inverso (deepseek, opencode, omp, pi), `thinking` explícito `low` en pi/omp
(desvía del default `max` de la app: compara harnesses, no la config instalada),
una reacción + un throughput por harness, patas por request (cada `usage.jsonl`
con su prompt/completion/ms y su t/s de pata), más `--warmup` (un request mínimo
con thinking off antes de cada harness — lo declarado pasa a ser TTFT caliente).

```bat
node tests/harness-bench.mjs --fair --thinking low --warmup --profile libros --context 131072 --no-control
```

Ventana completa (un solo `POST /api/start`, secuencial, parar al final):

1. `POST /api/start {"profile":"libros","context":131072}` → esperar `running` +
   `acceptance_ok:true`; anotar `decode_tps` de la puerta y segundos de carga (~1 min).
2. Arrancar el sampler en paralelo: `node tests/pressure-sampler.mjs --duration 600`
   (cada 5 s: RAM libre/comprometida, page-file, CPU total, `llama-server.exe`,
   top-3 GPU engines, `gen_tps` si la app responde; JSONL a
   `tests/harness-bench/results/pressure-<ts>.jsonl`). No arranca nada por sí mismo.
3. `node tests/harness-bench.mjs --fair --thinking low --warmup --profile libros --context 131072 --no-control`
   (~8 min: 8 runs con sus patas por request).
4. Opcional, puntos de profundidad con thinking off (mismo prompt + sufijo fijo):
   ~512 / ~8k / ~32k tokens (omitir 96k si tarda >5 min o algo se ve inestable).
5. `POST /api/stop` → confirmar `stopped` y `llama-server.exe` ausente.

Variante documentada: `--thinking off` es válido (pi/omp aceptan `off`; opencode y
deepseek no tienen flag equivalente y mantienen sus defaults).

## 64K vs 128K por harness (pendiente de medir — campaña 64K abortada)

Hechos ya medidos (sin motor, de la triage de opencode y las ventanas fair):
para el mismo prompt de 12,3k, prompt-eval **196 tok/s a 32K vs 39,6 tok/s a 128K**
(el KV a 128K desborda a DDR5 — por eso la primera pata de cada harness a 128K
ronda 0,05–0,7 t/s efectivos y la primera pata de opencode necesita 6–11 min y
roza el timeout de 600 s del bench). Decode medido en harnesses: ~30 t/s a
32K/64K frente a 15–18 t/s (`gen_tps`) a 128K. La historia de velocidad honesta
por harness tiene que incluir el perfil 64K (`multi_doc`, ctx 65536), no solo 128K.
Tabla 64K vs 128K: PENDIENTE (la campaña 64K se abortó por el apagado de la
máquina antes de producir runs; no se inventa ningún número).

## Máximo t/s a 128K y 262K (una carga por ventana)

Carga estrictamente secuencial: una sola carga por ventana, los cuatro harnesses
en serie dentro de cada carga, `POST /api/stop` entre ventanas con idle
≥10 min. Protocolo fair idéntico en las 8 runs
(`--fair --thinking low --warmup --reaction-timeout 900 --throughput-timeout 1200 --no-control`;
timeouts subidos frente a los 600/900 anteriores porque la reacción de opencode
había timed-out dos veces a 600 s — esta vez completó las 8 runs con ok=true).
Nota de versión: omp sale ahora como **18.4.1** (antes 18.3.1).

Ventana 1 — 128K: `POST /api/start {"profile":"libros","context":131072}` →
`starting` pid 8032; `running` + `acceptance_ok:true` a los ~30 s con puerta
`decode_tps = 14,73`. CPU pre-ventana 7 %, sin `llama-server.exe` previo.
Runs (hora de escritura del JSON): omp 11:03:56Z, pi 11:09:45Z, opencode
11:35:16Z, deepseek 11:45:55Z (timestamps UTC del fichero).
`POST /api/stop` → `stopped` a las 11:46:28Z, sin `llama-server.exe`.
Evidencia: `tests/harness-bench/results/2026-09-28T11-{03-56-943Z,09-45-299Z,35-16-248Z,45-55-574Z}.json`.

Idle entre ventanas: motor parado 11:46:28Z → `POST /api/start` ventana 2 a las
11:57:09Z = **10 min 41 s** (idle medido 11:46:53Z–11:56:53Z, 600 s, más 25 s
post-stop y 16 s pre-start).

Ventana 2 — 262K: `POST /api/start {"profile":"max_contexto","context":262144}` →
`starting` pid 13516; `running` + `acceptance_ok:true` a los ~30 s con puerta
`decode_tps = 30,54`. CPU pre-ventana 11 %, sin `llama-server.exe` previo. La
carga y el primer harness (omp 33,0 t/s) se vieron sanos, así que se completó
la ventana: omp 11:59:10Z, pi 12:01:35Z, opencode 12:07:12Z, deepseek 12:11:18Z.
`POST /api/stop` → `stopped` a las 12:11:48Z, sin `llama-server.exe`;
`LocalMind.exe` (pid 3692) sigue vivo.
Evidencia: `tests/harness-bench/results/2026-09-28T11-59-10-896Z.json`,
`2026-09-28T12-{01-35-427Z,07-12-581Z,11-18-021Z}.json`.

### Tabla por harness, ambos contextos (throughput; eff = completion/gateway_ms)

| Harness | ctx | Reacción 1er byte | Reacción wall / tok | Tput wall / 1er byte | Prompt+compl (tput) | eff t/s | gen_tps | reqs | cached post | ¿>30? |
|---|---|---|---|---|---|---|---|---|---|---|
| omp 18.4.1 | 128K | 0,9 s | 329,6 s / 7953+37 (1) | 83,4 s / 0,6 s | 7970+1381 (1) | 16,7 | 0,0* | 1 | 7871 | NO |
| pi 0.87.0 | 128K | 197,2 s | 197,2 s / 20255+356 (4) | 145,7 s / 145,7 s | 80912+1739 (14) | 12,0 | 17,3 | 14 | 103426 | NO |
| opencode 1.18.10 | 128K | 1,7 s | 735,1 s / 12871+914 (2) | 791,9 s / 2,0 s | 12902+2173 (2) | 2,6 | 16,7 | 2 | 104016 | NO |
| deepseek 0.1.5-rc.3 | 128K | 389,7 s | 390,6 s / 9867+110 (2) | 239,2 s / 173,9 s | 9900+1261 (2) | 5,2 | 17,8 | 2 | 113854 | NO |
| omp 18.4.1 | 262K | 1,3 s | 43,8 s / 7954+16 (1) | 38,3 s / 0,6 s | 7971+1238 (1) | 33,0 | 34,0 | 1 | 7871 | **SÍ** |
| pi 0.87.0 | 262K | 43,9 s | 43,9 s / 20871+525 (4) | 97,2 s / 97,1 s | 88586+2797 (14) | 28,9 | 28,3 | 14 | 111622 | NO (roza) |
| opencode 1.18.10 | 262K | 1,7 s | 155,0 s / 12871+338 (2) | 178,8 s / 1,8 s | 12902+1314 (2) | 7,2 | 32,6 | 2 | 112212 | NO |
| deepseek 0.1.5-rc.3 | 262K | 102,6 s | 104,0 s / 9873+131 (2) | 137,5 s / 101,9 s | 9904+1297 (2) | 9,3 | 33,8 | 2 | 112353 | NO |

\* `gen_tps = 0,0` en el throughput de omp a 128K es artefacto de muestreo
(`/api/metrics` en reposo entre requests: `prompt_tokens_cached 7871,
tokens_predicted 1620`), no decode real; el eff 16,7 de la pata sí es válido.

Deltas vs la fair anterior (§ "Después": omp 16,8 / pi 10,5 / opencode 1,0 /
deepseek 5,7): a 128K, omp 16,7 (−1 %), pi 12,0 (+14 %), opencode 2,6 (+160 % —
pero con timeouts de 900/1200 s frente a 600 s: la subida es tiempo permitido,
no velocidad; la pata grande tardó 788 s), deepseek 5,2 (−8 %).
De 128K a 262K (misma semana, distinto perfil): omp ×1,98, pi ×2,41, opencode
×2,77, deepseek ×1,79 en eff; `gen_tps` 15–18 → 28–34.

Salidas (comportamiento, no solo números): reacción omp/opencode/deepseek = `OK`
literal en ambas ventanas; pi = chit-chat en ambas (exit 0). Throughput
omp/opencode/deepseek = lista 1–127 cortada (1092–1093 B, mismo corte de §
"Truncamiento en 127", ahora en 6 de 6 runs one-shot); pi = escribe el fichero
(`numeros_1_300.txt` / `numeros.txt`) y resume en stdout (173/143 B).
Spec-decode 262K (accept/draft): omp 946/1021 (93 %), pi 3002/3557 (84 %),
opencode 4040/4791 (84 %), deepseek 4972/5780 (86 %).

### Patas por request (gateway `ms` → t/s de pata)

- omp 128K: reac 7953+37/328527 = 0,11; tput 7970+1381/82721 = 16,69.
- omp 262K: reac 7954+16/42264 = 0,38; tput 7971+1238/37477 = 33,03.
- pi 128K reac: 4897+95/175427 = 0,54; 5010+79/6144 = 12,86; 5103+130/10065 =
  12,92; 5245+52/4698 = 11,07. Tput (14): primera 4896+87/29586 = 2,94, resto
  12,15–15,64 (p. ej. 5643+112/7162 = 15,64; 6460+188/12309 = 15,27).
- pi 262K reac: 4895+213/30399 = 7,01; 5190+117/5249 = 22,29; 5321+132/5072 =
  26,03; 5465+63/2716 = 23,20. Tput (14): primera 4896+73/7826 = 9,33, resto
  22,61–36,18 (p. ej. 5382+210/6939 = 30,26; 5681+1203/33252 = 36,18 —
  la pata 1203-completion a 36,2 es el decode más rápido medido en un harness).
- opencode 128K: reac 592+882/77854 = 11,33 y 12279+32/731157 = 0,04; tput
  608+860/58372 = 14,73 y 12294+1313/788012 = 1,67.
- opencode 262K: reac 592+321/14896 = 21,55 y 12279+17/151247 = 0,11; tput
  608+190/8236 = 23,07 y 12294+1124/173642 = 6,47.
- deepseek 128K: reac 166+64/6885 = 9,30 y 9701+46/74429 = 0,62; tput 181+64/4706
  = 13,60 y 9719+1197/237349 = 5,04.
- deepseek 262K: reac 166+64/3322 = 19,27 y 9707+67/100911 = 0,66; tput 181+64/4398
  = 14,55 y 9723+1233/134564 = 9,16.

### Veredicto de paridad: NO están en el mismo rango

En decode puro (`gen_tps`) SÍ: 16,7–17,8 a 128K y 28,3–34,0 a 262K en los
cuatro — el motor da lo mismo a todos. En efectivo (lo que el dueño siente),
NO: a 128K opencode 2,6 queda ~6× por debajo de omp 16,7 y ~5× de pi 12,0;
deepseek 5,2 queda ~3× bajo omp y ~2,3× bajo pi. A 262K opencode 7,2 queda ~4,6×
bajo omp 33,0 y ~4× bajo pi 28,9; deepseek 9,3 ~3,5× bajo omp y ~3× bajo pi.
Razón medida (no hipótesis): el efectivo lo domina el prefill frío del system
prompt propio de cada harness — opencode paga DOS pre-fills de ~12,3k por tarea
(patas a 0,04–1,67 t/s a 128K; 0,11–6,47 a 262K) y deepseek UNO de ~9,7k (0,62
y 5,04–9,16), mientras omp paga uno solo de ~8k y pi amortiza el suyo en 14
patas con caché (12–16 t/s a 128K, 22–36 a 262K). El wall−ms de deepseek en
throughput sale ≈ −3/−1,5 s (requests solapados dentro del hijo, como opencode),
así que su overhead de orchestración de § "Desglose" no se repitió esta vez.

Sorpresa honesta: **262K fue MÁS rápido que 128K en los cuatro harnesses**
(puerta 30,54 vs 14,73; gen 28–34 vs 15–18). Causa NO verificada: el perfil
`max_contexto` trae flags distintos (conserva `-kvu`) y la carga es otra; el
undervolt aplicado hoy también puede mover relojes. No se afirma que 262K sea
siempre más rápido — es lo medido en estas dos cargas concretas.

### Qué palanca movería el número (HIPÓTESIS a medir, no medido)

- `--slot-save-path` (persistir el KV del system prompt entre arranques):
  convertiría el prefill frío de 8–12k (40–790 s) en hit de caché (~segundos).
  Es la palanca mayor para opencode (dos pre-fills por tarea) y deepseek.
- Spec-decode `n-max`: accept rates 84–93 % a 262K ya medidos; subir el draft
  podría acercar el decode 28–34 hacia 40+. Medir aceptados/wall, no solo t/s.
- Layout/cuant del KV (`-kvu` y vecindario): el 262K con `-kvu` decodeó al doble
  que el 128K mmap — entender por qué antes de tocar nada.
- Recortar system prompt por harness (opencode 12,3k, deepseek 9,7k): cada 1k
  de prompt frío cuesta ~25–60 s a 128K; nada rinde más por token ahorrado.
- Pi ya demuestra el camino barato: muchos turnos cortos con prefijo repetido
  (caché 103–112k) rinden 22–36 t/s por pata a 262K sin cambiar el motor.
- Thinking off en harnesses (pi/omp lo aceptan): quitaría el TTFT de
  razonamiento de la primera pata (197 s de pi-reacción a 128K son casi todo
  prefill+razonamiento, no hijo).

## Varianza entre cargas (el gate manda; `-kvu` descartado)

Hipótesis `-kvu` MUERTA por A/B del dueño a 128K (mismo prompt 12k):
prefill 168,07 → 154,70 t/s (neutro-peor), decode plano, commit 76,6 → 79,5 %.
La ventaja del 262K NO era el flag. Queda **varianza carga-a-carga**.

### Tabla de gates observados (misma sesión, `/api/status` o log)

| Carga | Perfil/ctx | Gate `decode_tps` | Fuente |
|---|---|---|---|
| campaña 128K | libros/131072, pid 8032 | **14,73** | status polling (~30 s) |
| re-medición 128K | libros/131072, pid 6800 | **24,16** | status polling (~30 s); log `Verificación OK: 24 t/s` (línea 4756) |
| A/B pre-campaña | libros/131072 | 24,77 (log `OK: 25 t/s`, línea 3785) y 24,34 (mencionado por el dueño) | `localmind.log` líneas 3777–3785 |
| A/B otro punto | libros/131072 | 24,80 (stale gate visto en status pre-campaña) | status |
| 262K | max_contexto/262144, pid 13516 | **30,54** | status; log `OK: 31 t/s` (línea 4333) |
| 32K típico | velocidad/32768 | 32–34 (una docena de cargas: 30/33/34/34/34/34/34/32/32/32/32/33) | `localmind.log` |
| 128K lentas | libros/131072 | 13–15 (líneas 883, 1637, 1673, 2247, 2308, 2356, 2405, 2583, 2813, 3830) | `localmind.log` |

Lectura: el 128K es BIMODAL — o bien ~13–15, o bien ~24–25 — con el MISMO
perfil, mismos hilos (6), mismo uBatch (512), mismo `kv_unified='false'`.
La campaña (14,73) fue la rama lenta; la re-medición (24,16) la rápida.
El 262K (30,54) y el 32K (32–34) son consistentemente rápidos.

### Líneas de arranque recuperadas (`%APPDATA%\LocalMind\logs\localmind.log`)

El log NO trae líneas de `KV self size` / `CUDA0 KV buffer` / `VMM` /
`cache_ram` / `model buffer` / `compute buffer` / offload (verbosity 3 del
`llama-server` no las emite; 0 hits para VMM/buffer-size/cache_ram/offload).
Lo que SÍ trae por carga, lado a lado:

| Carga | `n_ctx_slot` / `kv_unified` | threadpool→loaded | Gate probe (64 tok prompt + 200 tok gen) | Veredicto log |
|---|---|---|---|---|
| campaña 128K `[1790592977]` (líneas 3799–3830) | 131072 / `false` | 19,2 s → 20,8 s | prompt 1838,74 ms (**34,81 t/s**); eval 11708,78 ms (**17,00 t/s**); total 13547,52 ms | `OK: 15 t/s` |
| re-med 128K `[1790599220]` (líneas 4725–4756) | 131072 / `false` | 20,4 s → 21,6 s | prompt 890,45 ms (**71,87 t/s**); eval 7353,43 ms (**27,06 t/s**); total 8243,88 ms | `OK: 24 t/s` |
| A/B 128K `[1790592559]` (líneas 3755–3785) | 131072 / `false` | 17,8 s → 20,1 s | prompt 906,97 ms (**70,56 t/s**); eval 7125,49 ms (**27,93 t/s**); total 8032,46 ms | `OK: 25 t/s` |
| 262K `[1790596630]` (líneas 4302–4333) | 262144 / `true` | 21,3 s → 24,1 s | prompt 837,63 ms (**76,41 t/s**); eval 5566,27 ms (**35,75 t/s**); total 6403,90 ms | `OK: 31 t/s` |

Mejor explicación soportada: **el decode del gate a 128K cae a ~17 t/s en la
rama lenta frente a ~27–28 en la rápida, y el prompt-eval a ~35 frente a
~71–72** — con argv idéntico (mismos hilos/ubatch/split-mode/device; el log
no registra la línea de comando, pero `src-rust/src/process.rs:886-973`
construye el MISMO argv para ambas: `-ngl 99 -c <ctx> -ctk/-ctv q4_0 -t 6
-ub 512 --split-mode none -np 1 --cache-ram 6144`, más `-kvu` solo en
`max_contexto`). No hay split KV/VRAM distinto visible porque el log no lo
emite; la diferencia 2× en prompt-eval con el mismo ctx apunta a estado de
máquina (relojes/VRAM fragmentada tras cargas previas, throttling térmico o
frecuencia GDDR) más que a configuración. La campaña 128K arrancó tras solo
~25 s post-stop de una carga 64K (líneas 3793–3798: stops 1790592675/2813 y
start 1790592977); la re-medición arrancó tras >2 h de idle relativo con la
máquina quieta (CPU 4 %). Sin traza de relojes/VRAM por carga no se puede
cerrar; el siguiente paso es loguearlas (ver hipótesis abajo), no más A/B.

### Re-medición 128K: el gate bueno confirma el outlier (pid 6800)

Ventana: `POST /api/start {"profile":"libros","context":131072}` 12:40:20Z →
`starting` pid 6800; `running` + `acceptance_ok:true` a los ~30 s con puerta
`decode_tps = 24,16`. CPU pre-ventana 4 %, sin `llama-server.exe` previo.
`POST /api/stop` → `stopped` 12:58:53Z, sin `llama-server.exe`; GUI viva
(pid 3692). Evidencia:
`tests/harness-bench/results/2026-09-28T12-{42-33-179Z,46-55-442Z,52-45-107Z,58-39-712Z}.json`.
Solo se consumió UNA carga de motor en este turno (presupuesto: 2).

| Harness | Reacción 1er byte | Reacción wall / tok | Tput wall / 1er byte | Prompt+compl (tput) | eff t/s | gen_tps | reqs | cached post | ¿>30? |
|---|---|---|---|---|---|---|---|---|---|---|
| omp 18.4.1 | 1,2 s | 43,0 s / 7953+20 (1) | 41,0 s / 0,6 s | 7971+1118 (1) | 27,8 | 28,6 | 1 | 7871 | NO |
| pi 0.87.0 | 59,2 s | 59,3 s / 20481+360 (4) | 190,5 s / 190,5 s | 104039+1988 (16) | 10,5 | 12,3 | 16 | 125556 | NO |
| opencode 1.18.10 | 1,9 s | 210,8 s / 12870+534 (2) | 100,1 s / 1,9 s | 12902+1376 (2) | 12,4 | 0,0* | 2 | 138409 | NO |
| deepseek 0.1.5-rc.3 | 125,7 s | 125,7 s / 9867+98 (2) | 205,5 s / 122,5 s | 9904+1336 (2) | 6,4 | 14,9 | 2 | 138550 | NO |

\* `gen_tps = 0,0` en opencode-tput = artefacto de muestreo en reposo (igual
que omp-campaña); el eff 12,4 de sus patas sí es válido.

Corregido vs campaña (misma tabla § anterior, gate 14,73 → 24,16):
omp 16,7 → **27,8** (+66 %); opencode 2,6 → **12,4** (×4,8 — su pata grande
pasó de 788 s a 96 s: 1,67 → 12,49 t/s); deepseek 5,2 → **6,4** (+23 %);
pi 12,0 → 10,5 (−13 %, con 16 reqs esta vez y una pata 7080+92/20132 = 4,57
en mitad del run — varianza intra-carga, no regresión).
Patas: omp-tput 7971+1118/40193 = 27,82; pi-tput primera 4896+91/14094 = 6,46
resto 10,56–12,83; opencode-tput 608+173/14842 = 11,66 y 12294+1203/96323 =
12,49; deepseek-tput 181+64/5305 = 12,06 y 9723+1272/203636 = 6,25.
Salidas: omp/deepseek `OK` literal; pi chit-chat; tput omp/opencode/deepseek =
lista 1–127 (1093 B, mismo corte); pi = fichero + resumen.
Veredicto de paridad CON gate bueno: decode 12–29 según harness, pero el
efectivo sigue separado por el prefill del system prompt (opencode 12,4 y
deepseek 6,4 vs omp 27,8) — la paridad en efectivo NO se alcanza ni en carga
rápida; en decode puro casi (28,6 / 12,3 / ~12,5 / 14,9 — pi y deepseek
midieron bajo por patas con prefill entremezclado, no por motor).

### Hipótesis para la próxima ventana (SOLO si el 128K hubiera repetido ~14)

NO APLICA esta vez (el 128K dio 24,16: fue outlier, no perfil lento). Se
registran como hipótesis a instrumentar, no conclusiones:
- Relojes/frecuencia GPU y VRAM libre/fragmentada en el momento del arranque
  (la rama lenta 2× en prompt-eval con argv idéntico huele a estado, no a flag).
- `cache_ram` 6144 y colocación del KV (RAM vs VRAM) por carga: el log actual
  no emite el split — subir verbosity o volcar `n_gpu_layers` efectivas y
  `CUDA0 ... buffer` por carga cerraría la pregunta.
- Tiempo desde el stop previo (cooldown real vs 120 s del guard) y throttling
  térmico: la campaña arrancó a ~25 s de un stop; la re-medición con horas de
  margen. Registrar `starting_for_secs`, temperatura y relojes por carga.

## Deriva térmica y `finish_reason` (ventana 2026-09-28, una sola carga 128K)

Ventana: `POST /api/start {"profile":"libros","context":131072}` 23:09:18Z →
`starting` pid 17144; `running` + `acceptance_ok:true` a los ~60 s con puerta
`decode_tps = 16,67`, samples `[16,74, 16,50, 16,67]`, `engine_slow:true`
(rama lenta del 128K, ver § "Varianza entre cargas"). Log: `n_ctx_slot =
131072, kv_unified = 'false'` (línea 5935); veredicto `OK: 17 t/s (mediana de
3)` (línea 5978). CPU pre-ventana 0 %, sin `llama-server.exe` previo.
`POST /api/stop` → `stopped` 23:19:30Z, sin `llama-server.exe`; GUI viva
(pid 11540). Tiempo de motor ≈ 10 min (60 s carga + 108 s finish_reason +
327 s droop). Sin inestabilidad (sin stutter/fan-ramp/crash).
Evidencia: `tests/harness-bench/results/finish-reason-128k-1790637136563.txt`
(cola SSE) y `tests/harness-bench/results/droop-128k-2026-09-28T23-13-50-617Z.json`
(timeline; el runner `droop-run-128k.mjs` era throwaway y se eliminó).

### `finish_reason` del corte en 300 números: `length`, el motor pasa del 127

Prompt determinista por gateway (`temperature:0`, `max_tokens:2000`,
`stream:true`): HTTP 200 `text/event-stream`, 2002 chunks SSE, wall 107792 ms.
Último chunk con contenido útil, verbatim:
`data: {"choices":[{"finish_reason":"length","index":0,"delta":{}}], ... "usage":{"completion_tokens":2000,"prompt_tokens":73,...} ...}`
seguido de `data: [DONE]`. Timings del motor: prompt 713,79 ms (43,43 t/s),
decode 107044,5 ms (18,67 t/s), draft 1314/1370 (96 %). La cola llega al "195"
(`...94, 195, 19` + corte por `length`, igual que la campaña original).
Conclusión: **pregunta cerrada por lado motor** — NO hay EOS en 127 sin system
prompt de harness; el corte 1–127 de los runs omp/opencode/deepseek es EOS
condicionado por sus prompts (o stop del harness). Queda abierto solo el lado
harness (`finish_reason` por pata — el bench no lo registra).

### Deriva térmica: NO hay droop en ~3 min sostenidos (rama lenta, peor caso)

Protocolo: probe 512-token-prompt/300-completion, 10 slices back-to-back
(max 1200 cada uno, ~15,6 s por slice) y 3 re-probes con 30 s de idle.
Nota: el prompt pedido de "512" resultó en prompt_n real 758 (2720 chars);
los slices siguientes hitan caché (prompt_n 4, completion 252 fijos) — lo que
miden es decode sostenido puro, que es lo que importa para droop.

| # | tag | ts (UTC) | wall | compl | decode (timings) | gen_tps (métricas) |
|---|---|---|---|---|---|---|
| 0 | probe0-baseline | 23:14:23 | 33,1 s | 252 | 16,19 | 16,13 |
| 1–10 | slice1…slice10 | 23:14:39 → 23:17:00 | 15,6–15,7 s c/u | 252 c/u | 16,32–16,35 | 16,26–16,29 |
| 11 | reprobe1 (+30 s idle) | 23:17:46 | 15,7 s | 252 | 16,33 | 16,27 |
| 12 | reprobe2 (+30 s idle) | 23:18:31 | 15,7 s | 252 | 16,32 | 16,25 |
| 13 | reprobe3 (+30 s idle) | 23:19:17 | 15,7 s | 252 | 16,33 | 16,27 |

Veredicto: **sin droop medible** — decode 16,19 → 16,32–16,35 sostenido
(rango ±0,1 t/s, deriva +0,9 % hacia ARRIBA por calentamiento de caché, no
caída) y los 3 re-probes idénticos (16,32–16,33). GPU (muestra barata
post-ventana): 3D ~0,17–0,21 %, copy 0,11 % — motor ya parado, solo confirma
que no quedó carga colgada. Inconcluso: NADA pendiente aquí salvo repetir en
rama rápida si alguien quiere el par; la ausencia de droop en la rama lenta
(peor caso térmico) es evidencia suficiente para descartar la deriva como
explicación de la varianza entre cargas.
