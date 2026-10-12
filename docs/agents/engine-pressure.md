# Protocolo de mitigación de jank (LM-NF-3)
> **CORRECCIÓN 2026-10-11 — `--cache-ram` no controla el KV activo.** El
> build 10743 lo define como límite de caché de prompts en RAM. Las mediciones
> y propuestas de este documento pueden conservar sus tiempos observados, pero
> quedan retractadas las atribuciones de memoria DDR5, offload KV o cuello de
> bus que se basaron en ese flag. Ver `docs/engine-audit-2026-10.md`.

Estado: plan + diffs preparados, SIN medir. Ventana de medición pendiente
(la máquina está en uso por el dueño; otro worker tiene la ventana del motor).
Nada de este documento requiere haber arrancado el motor para leerse.

## 1. Inventario de palancas medido (fuente: `config.rs`, `process.rs` 849-925, TOML vivo)

Modelo Qwen3.8-27B IQ4_XS (~13 GB) en RX 6800 XT 16 GB (Vulkan), CPU
Ryzen 5 7600 6C/12T. Argv común: `-ngl 99 -ctk q4_0 -ctv q4_0 -a localmind
--reuse-port -t <threads> -tb <threads_batch> --prio 2 -b 1024 -ub 512
--device Vulkan0 --split-mode none --host 127.0.0.1 --port <dyn> -np 1
--poll 0 --prio-batch 0 -fa on --metrics` + spec draft-mtp (n=2, p-split 0,1)
+ `--reasoning-preserve`. El TOML conserva `cache_reuse = 256`, pero OMNI no
lo agrega al argv. El build 10683 lo rechazaba en este contexto; la ayuda del
10743 ya ofrece la opción, pendiente de prueba viva con Qwen3.8.

| Palanca | 32K `velocidad` | 64K `multi_doc` | 128K `libros` | 262K `max_contexto` | Costo en escritorio |
|---|---|---|---|---|---|
| `-c` contexto | 32768 | 65536 | 131072 | 262144 | KV lineal con `-c`; a 128K+ cruza el bus |
| `--cache-ram` (MiB de caché de prompts) | 0 | 4096 | **6144** | 6144 (+`-kvu`) | Valores históricos: limitan caché de prompts; no colocan el KV en DDR5. Los perfiles integrados actuales migran a 0 en v6. |
| `-t` threads (default TOML `threads=None` → `num_cpus`=6) | 6 | 6 | 6 | 6 | **6 = todos los físicos**: prefill satura el CPU, nada libre para el desktop |
| `-tb` threads_batch (`limit_threads_batch=true` → `min(threads,4)`) | 4 | 4 | 4 | 4 | Pico de prefill algo acotado; 4+2 hilos siguen siendo casi todo el CPU |
| `-b 1024` / `-ub 512` | fijos | fijos | fijos | fijos | **PSU-LOCKED: no tocar** (picos de potencia/VRAM) |
| spec draft-mtp n=2 | activo | activo | activo | activo | **PSU-LOCKED: no tocar** |
| `-fa on` | on | on | on | on | Bien: menos memoria de atención |
| `process_priority below_normal` | sí | sí | sí | sí | Solo prioridad OS; no limita CPU/VRAM/bus |
| mmap | default (activado, sin flag) | — | — | — | Pesos en page-cache; con commit alto puede hacer thrash |
| `idle_timeout_secs` | 1500 (25 min, global) | — | — | — | Ventana de jank larga tras cada uso |

Números ya medidos (aceptación, máquina con carga de workers): 128K 32,7 t/s;
contexto corto caliente 94,6 t/s; 262K 14,16 t/s. En reposo sin motor:
~7,6 GB libres de 15,5 GB, commit ~57 %, CPU 4-8 %.

## 2. Hipótesis H1-H4 (qué las confirma o las mata)

- **H1 — bus/RAM:** el jank es el KV de 6 GB en DDR5. Confirma: `cache_ram`
  más bajo → commit% baja y `gen_tps` igual o mejor. Mata: t/s cae >5 %
  al bajar `cache_ram` (el KV no entra en VRAM sin que el modelo desborde).
- **H2 — CPU:** el jank es prefill saturando los 6 físicos. Confirma:
  `threads 4` baja el pico por núcleo con `gen_tps` (decode, GPU-bound)
  casi intacto. Mata: `prompt_tps` se hunde o el pico no baja.
- **H3 — confusor workers:** los 11-18 t/s de la campaña de 35 min eran
  contención (node/python/cargo ajenos), no el motor. Confirma: en máquina
  quieta 128K vuelve a ~29-33 t/s. Mata: el número bajo se reproduce quieto.
- **H4 — page-cache (mmap):** solo si H1/H2 no explican. Confirma:
  `--no-mmap` baja pagefile%/fallos duros sin costo de RAM privada
  inaceptable (~13 GB privados extra). Mata: commit% se dispara u OOM.

## 3. Protocolo de la ventana libre (~8-10 min, una sola carga por ronda)

Pre-chequeo cada ronda: `tasklist` sin `node`/`python`/`cargo` ajenos;
si hay workers, esperar, no medir. Motor <10 min por ronda, stop entre rondas.

Ronda 0 (baseline 128K):
1. `POST /api/start {"profile":"libros","context":131072}` → esperar
   `running` + `acceptance_ok:true`; anotar `load s` y `decode_tps` de la puerta.
2. Muestreo 3 min cada 5 s (36 muestras) con `tests/pressure-sampler.mjs`
   (worker ajeno; si no existe aún, el bucle manual de la §4 vale):
   `Available MBytes`, `% Committed Bytes In Use`, `Paging File % Usage`,
   CPU-tiempo + working set de `llama-server.exe`, `gen_tps` de `/api/metrics`
   por muestra; GPU solo si el contador responde en AMD (si no, no-medible).
3. Probe de 300 tokens (streaming directo al puerto del motor y vía
   `/v1/chat/completions`, `max_tokens 300`, temperature 0) → `decode t/s`
   en máquina quieta = re-medición honesta de los 11-18 de la campaña.
4. `POST /api/stop` + confirmar sin `llama-server.exe` en `tasklist`.

Ronda A (`cache_ram` 128K → 0): aplicar diff A, repetir 1-4.
Ronda B (`cache_ram` 128K → 4096): aplicar diff B, repetir 1-4.
Ronda C (`threads` default → 4): aplicar diff C, repetir 1-4.
Ronda D (opcional, solo si H1/H2 no explican): `--no-mmap` en `extra_flags`
del perfil `libros`, repetir 1-4.

Criterio de aceptación (por cambio): presión del sistema mediblemente menor
(commit% pico −2 puntos o pico CPU/núcleo −15 %) Y `decode t/s` sin regresión
mayor al ~5 % frente a la ronda 0. Si no cumple ambas, rollback al valor previo.
`-ub`/`-b`/spec-decode no se tocan nunca (PSU). El contexto nunca se reduce
en silencio (D2): cada ronda pide 131072 explícito.

## 4. Bucle manual de respaldo (si el sampler ajeno no está listo)

```powershell
# Una muestra cada 5 s: RAM + commit + pagefile + motor + gen_tps aparte
powershell -NoProfile -Command "(Get-Counter '\Memory\Available MBytes','\Memory\% Committed Bytes In Use','\Paging File(_Total)\% Usage' -SampleInterval 1 -MaxSamples 1 | Select-Object -ExpandProperty CounterSamples | ForEach-Object { [math]::Round($_.CookedValue,1) }) -join '/'"
powershell -NoProfile -Command "$p=Get-Process llama-server -ErrorAction SilentlyContinue | Select-Object -First 1; if($p){$p.CPU.ToString('F0')+'/'+[math]::Round($p.WorkingSet64/1MB,0)}else{'dead'}"
# gen_tps por muestra: GET /api/metrics con la gateway key (ver tests/bench.mjs)
```

## 5. Resultados medidos 2026-09-25 (rondas B y C, máquina quieta)

Serie pareada 128K (`libros`, sampler 200 s @5 s + probe 300 tok directo):

| ronda | libre MB | commit % | page % | CPU % idle | decode 300tok | aceptación |
|---|---|---|---|---|---|---|
| R0 `cache_ram` 6144, threads 6 | ~1400 | 88.2 | ~9.8 | 3-5 | **16.04 t/s** | 14.88 t/s |
| RA `cache_ram` 0 (revertida) | ~650 | 88.4 | 9.3 | 3-5 | 16.23 t/s | 15.03 t/s |
| RB `threads` 4 | ~700 | 88.3 | 8.8 | 0-2 | **16.15 t/s** | 14.93 t/s |
| RC `--no-mmap` (**mantenida**) | **~6440** | 87.6 | 8.5 | 0-2 | **16.18 t/s** | 15.04 t/s |

- H1 (`cache_ram`): MUERTA — commit y t/s planos (±0,2 t/s); el commit lo
  dominan los 13 GB del modelo, no el KV.
- H2 (CPU): MUERTA como causa del jank (pico 16-18 %, media 1-5 % en todas
  las rondas), pero `threads 4` deja 2 físicos libres casi gratis
  (decode −0 %/+0,7 %): se documenta como opción, default sigue en 6
  (TOML vivo restaurado a `threads = 6`; ver diff C más abajo).
- H3 (contención): PARCIALMENTE VIVA — 16 t/s se reproduce en máquina quieta
  con prompt largo; no era solo contención.
- H4 (page-cache): **CONFIRMADA** — `--no-mmap` libera ~5 GB
  (libre 0,7→6,4 GB), page-file 9,3→8,5 %, sin regresión de decode
  (+0,9 % vs R0). El RSS privado sube (~3,8 GB estables, sin OOM).

## 6. Diffs preparados (NO aplicados)

Con 6 físicos y 16 GB VRAM, y los números de hoy:
- Escritorio fluido: `velocidad` 32K (`cache_ram` 0, todo en VRAM) — [MEDIDO:
  puerta ~34 t/s; jank no medido pero sin tráfico de bus por diseño].
- 128K/262K: aceptar tráfico de bus + commit de RAM alto; cerrar trabajo
  pesado antes; el motor tarda 25 min en soltarse solo (`idle_timeout_secs`).
- Una línea para el dueño: **"para el escritorio fluido: 32K; para 128K/262K:
  aceptá 6 GB de KV en RAM y micro-cortes, o bajá a 32K"** — [SIN MEDIR el
  tamaño real de los micro-cortes; la ventana libre lo cuantifica].
- "Modo liviano" propuesto (no implementado): preset `liviano` = 32K,
  `threads 4`, `cache_ram 0`, `idle_timeout` corto; criterio: adoptarlo si la
  ronda C muestra que 2 núcleos libres eliminan el jank sin costo >5 % en t/s.

## 7. ¿Qué hago para que no se trabe el escritorio a 128K? (respuesta honesta)

(Sección movida: ver §10 tras la corrección de seguridad — el contexto de trabajo es siempre 128K-262K.)

## 8. Cómo llega el fix al usuario (migración `profiles_version`)

El TOML vivo trae `[[profiles]]` materializados, así que cambiar el integrado
en `config.rs` no basta: `ConfigStore::load` compara `profiles_version`
(ausente = 0, actual = 3) y, si es menor, refresca cada id integrado presente
solo cuando el usuario no lo customizó — `extra_flags` se actualiza únicamente
si cada flag guardada ya estaba en ALGUNA foto previa (v0/v1/v2: una flag
añadida por el usuario bloquea el refresh de ese perfil), y `context`/
`cache_ram` solo si siguen iguales a alguna foto previa; ids desconocidos
intactos. Luego escribe la versión (tmp + rename, best-effort) y deja una línea
`[LocalMind] Perfiles actualizados a v3: <ids>` que `main.rs` emite vía
`mgr.log` (= archivo P30 + SSE). Instalación fresca lo trae directo.
Historial: v1 `--no-mmap` → v2 reversión a `[]` (veredicto A/B prematuro,
documentado abajo y superado) → v3 `--load-mode none` (grafía canónica):
cada salto conserva las fotos previas para no tocar customs.

## 9. Veredicto A/B prefill 8k @128K: el flag SE MANTIENE (override del dueño)

Mismo prompt de 10546 tokens, misma máquina quieta, scratch APPDATA:
variante A (mmap default) → prefill **21,65 t/s** (463 s), decode **14,01 t/s**;
variante B (sin mmap) → prefill **38,18 t/s** (263 s, **+76 %**), decode
**13,92 t/s** (-0,6 % = ruido), commit 89,3→87,7 %, libre 620→991 MB.
Racional explícita: el prefill del primer turno ES el caso diario — cada primer
turno a 128K paga 5-12k tokens de system-prompt y el flag recorta ~200 s de esa
espera ("cuánto tarda en reaccionar"); el decode sostenido queda igual y el
commit igual o mejor. Por eso la v2 se supera: `libros` lleva `--load-mode
none` (grafía no deprecada; `--no-mmap` hacía lo mismo pero está deprecado en
el build 10683). Tests: `mig_v1_nommap_upgraded_to_canonical`,
`mig_v2_empty_upgraded_to_canonical`, proofs v0/v1/v2 en scratch APPDATA.

## 10. Seguridad de energía (LM-NF-3, 2026-09-28: dos apagones duros)

Análisis del incidente: la PC se apagó de golpe dos veces, sin BSOD =
firma de protección por sobrecorriente de la PSU, no crash de software.
Sospechoso dominante: el transitorio de arranque del motor (cada
`POST /api/start` lee ~13 GB a VRAM de una vez) repetido en decenas de
ciclos start/stop seguidos entre campañas y ventanas de medición; el cambio
a `--load-mode none` agravaba cada arranque (sin page-cache = relectura
total). Sospechoso secundario: GPU sostenida ~99,6 % durante 40-60 min por
campaña. El contexto de trabajo es siempre 128K-262K: la seguridad NO
restringe contexto, solo picos y frecuencia.

Guardarraíles implementados (`config.rs`/`process.rs`, defaults):
- `[engine] start_cooldown_secs = 120`: un arranque antes de 120 s se rechaza
  con los segundos restantes en español (y se loguea).
- `[engine] max_starts_per_hour = 4`: el 5º arranque en hora rodante se
  rechaza (y se loguea). Historial acotado a 64 entradas (D-8).
- `[engine] idle_timeout_secs = 5400` (90 min, antes 1500): el motor
  residente evita ciclos stop/start entre sesiones de agentes; para liberar
  VRAM usar Stop manual.
- `libros`/`max_contexto` vuelven a load mode default (mmap, arranques
  baratos); `--load-mode none` queda opt-in por perfil (migración v4, misma
  regla segura: fotos v0/v1/v2/v3 reconocidas, customs intactos).
- Candado PSU en el arranque: `psu_unsafe_flag` rechaza `-ub`/`--ubatch-size`
  > 512, `-b`/`--batch-size` > 1024 y cualquier `--spec-*` en perfiles o
  engine global, nombrando la flag en español (ni el dueño ni un worker
  futuro pueden aflojar los fusibles por config).
- Watchdog sin reintentos: si el hijo muere (incluidos los primeros ~90 s),
  `error` + log y NADA más; reintentar = Stop+Start manual (con cooldown).
- `[engine] power_safe = true`: guardarraíl + candado + sin reintentos +
  aviso en log al arrancar 262K; ningún perfil se rechaza.

Recomendación de hardware (el fix real del pico): bajar el power limit de la
GPU −10/−15 % en Radeon Software (costo típico <5 % t/s) y, a medio plazo,
una PSU de mayor margen. El software solo puede espaciar y abaratar picos,
no eliminar el transitorio de 13 GB.

## 11. A/B `-kvu` en `libros` @128K: NO gana (2026-09-28, scratch APPDATA)

Mismo prompt de ~12k (15802 tok reales vía gateway), mismo contexto 131072,
misma máquina quieta, cargas separadas por ≥3 min idle + cooldown 120 s:

| variante | gate t/s | prefill 15802 tok (motor) | prefill gateway | decode | commit med | free med |
|---|---|---|---|---|---|---|
| A 128K sin `-kvu` (`kv_unified='false'`) | 24.77 | 94020 ms (168.07 t/s) | 145.23 t/s | — | 76.6 % | 554 MB |
| B 128K con `-kvu` (`kv_unified='true'`) | 24.34 | 102145 ms (154.70 t/s) | 134.49 t/s | — | 79.5 % | 1271 MB |

Veredicto: **`-kvu` NO se aplica a `libros`**. A igualdad de prompt es −8 %
en prefill del motor y −7 % vía gateway, con commit PEOR (+3 puntos); el
decode/gate planos (±2 %). La ventaja 2× vista en la campaña (262K con
`-kvu` vs 128K sin él) no replica a igualdad de contexto: era efecto del
contextouchs y del prompt, no del flag. Sin cambio de código (la migración
sigue en v4 con `libros` en `[]`); sin inestabilidad en ninguna carga.
Nota metodológica: el primer intento de B corrió sin el flag (el TOML
scratch había sido reescrito por la migración v4 al arrancar: `["-kvu"]` →
`[]` por regla segura); se repitió con hot-import verificado (`["-kvu"]` +
`kv_unified='true'` en el log). El guardarraíl de cooldown funcionó en vivo
(un `start` a los ~70 s fue rechazado con "esperá 49 s").

## 12. Puerta con calentamiento + mediana de 3 (bimodalidad 128K)

Evidencia (campaña): mismo argv 128K, dos cargas seguidas dan prompt-eval
34,8 vs 71,9 t/s y eval 17,0 vs 27,1 t/s; gates 13-15 vs 24-25 (30,5 en 262K,
32-34 en 32K). La carga lenta arrancó ~25 s después de un stop previo; la
rápida tras un reposo largo. La primera medida en frío no representa el
estado estacionario.

Comportamiento nuevo (`process.rs`, `config.rs`): tras `/health` OK y antes
de la medida real, 1 completion desechable (16 tokens, mismo path de chat);
luego 3 muestras iguales (prompt/max_tokens idénticos) y `decode_tps` =
mediana (con 2 muestras por un fallo, la menor: no inflar). El veredicto
sigue en la mediana (≥20 tokens, ≥3 t/s: la guarda contra CPU intacta).
`decode_tps_samples: [f64; 3]` expone la dispersión; `engine_slow: bool` es
true bajo `[engine] slow_gate_tps = 20` (default) con mensaje UI
`El motor cargó lento (N t/s); puede mejorarse reiniciándolo una vez`.
Sin reintentos automáticos (el guardarraíl LM-NF-3 lo prohíbe) y sin
re-calentamiento con el motor en `running` (la puerta solo corre en
`starting`, una vez por arranque). Cooldown, tope horario, candado PSU y
watchdog intactos.

## 13. Migración `[engine]` a v5: menos ciclos stop/start (2026-09-28)

El TOML materializado trae `[engine]` con defaults viejos (`idle 1500`, sin
`start_cooldown_secs`/`max_starts_per_hour`/`power_safe`/`slow_gate_tps`) y la
migración solo refrescaba perfiles: los nuevos defaults de energía nunca
llegaban a una instalación existente (misma brecha que los perfiles en v1).
Regla v5 (igual de segura que perfiles): cada campo de `[engine]` se toca
solo si sigue igual a ALGUNA foto previa (1500 para idle; 90/3 de v4 para
cooldown/cap; ausente = foto `None`); un 900 deliberado o cualquier
`threads`/`batch`/`priority`/puerto customizado queda intacto. Efecto
esperado (no medido como t/s: es conteo de ciclos): con `idle 5400` el motor
residente sobrevive 90 min entre turnos de agentes en vez de 25, así que una
jornada de harnesses paga ~3 arranques en vez de ~10+ (cada arranque = el
transitorio PSU más grande). La nota de migración nombra
`engine: idle_timeout_secs, start_cooldown_secs, …`.

## 14. Validación del request de arranque (nunca sustituir en silencio)

`POST /api/start` valida CAMPOS NOMBRADOS antes del guardarraíl (un typo no
consume cooldown/tope): perfil inexistente → `Perfil desconocido: '<id>'.
Válidos: …`; contexto fuera de 1024..=1048576 en múltiplos de 1024 →
`Contexto no válido`; modelo no listado ni alias → `Modelo desconocido`.
Omitidos resuelven por la precedencia habitual (request → última sesión →
defaults). Misma familia que el bug "pedí 32K y me dio 128K".

## 15. Modelos sin MTP: reintento dirigido sin spec (2026-09-28)

Defecto: `POST /api/start {"model":"LFM2.5-2.6B-Q4_K_M.gguf"}` moria siempre
(`creating MTP draft context` -> `model doesn't contain MTP layers` ->
`failed to create MTP context`) porque la app pasa `--spec-*` siempre
(`[engine.speculation] enabled = true`), valido solo para modelos con capas
MTP (nuestro Qwen). Recuperacion dirigida (NO es el auto-reintento generico
que prohibe LM-NF-3): si el hijo muere en `starting` con la firma exacta,
UNA vez por arranque y solo con spec habilitada, se relanza el MISMO
arranque sin `--spec-*` (mismo modelo/perfil/contexto/puerto; sin cooldown,
sin tope, sin revalidar; la puerta sigue en `starting`). Linea grepeable:
`[LocalMind] El modelo no soporta decodificacion especulativa (MTP):
reintentando sin --spec-*`. Si vuelve a fallar -> `error` normal.
Desactivar spec por config: `[engine.speculation] enabled = false`
(pendiente `POST /api/config` -> `engine.speculation.enabled`; el worker de
server.rs no pudo tomarlo — derivado a Main/engine-worker con snippet).

## 16. Doble defecto MTP: unconditional + contexto stale + verificación /props

Evidencia (model worker, LFM2.5): (a) el reintento MTP disparaba aunque el
TOML tuviera spec apagada — la condición no exigía `spec_on`; (b) el
reintento reconstruía el argv desde `st.context`/`st.model` (stale: `[last]`
se persiste al final del `start()` original), así que pedir 65536/131072
relanzaba con 32768 (`n_ctx_slot = 32768` en el log del motor).
Fix: (1) `spawn_child_nospec` recibe el `ResolvedStart` guardado en
`start()` (mismo modelo/path, contexto, perfil, hilos, prioridad, puerto,
mmproj) y difiere del primer spawn SOLO en omitir `--spec-*`;
`pending_start.take()` = una sola vez; (2) condición endurecida con
`spec_on` (sin spec habilitada no hay nada que omitir: directo a `error`);
(3) cinturón `/props`: tras la puerta, `engine_n_ctx(port)` compara `n_ctx`
vs pedido; si difiere → `error` grepeable
`El motor cargó con contexto N pero se pidió M` (ausente → sin falso error).

## 16 (actualizado). Doble defecto MTP: unconditional + contexto stale + verificación /props

Evidencia (model worker, LFM2.5): (a) el reintento MTP disparaba aunque el
TOML tuviera spec apagada — la condición no exigía `spec_on`; (b) el
reintento reconstruía el argv desde `st.context`/`st.model` (stale: `[last]`
se persiste al final del `start()` original), así que pedir 65536/131072
relanzaba con 32768 (`n_ctx_slot = 32768` en el log del motor).
Fix: (1) `spawn_child_nospec` recibe el `ResolvedStart` guardado en
`start()` (mismo modelo/path, contexto, perfil, hilos, prioridad, puerto,
mmproj) y difiere del primer spawn SOLO en omitir `--spec-*`;
`pending_start.take()` = una sola vez; (2) condición endurecida con
`spec_on` (sin spec habilitada no hay nada que omitir: directo a `error`);
(3) cinturón `/props`: tras la puerta, `engine_n_ctx(port)` compara `n_ctx`
vs pedido; si difiere → `error` grepeable
`El motor cargó con contexto N pero se pidió M` (ausente → sin falso error).
Tests: `props_n_ctx_shapes` (raíz/anidado/ausente/no-numérico),
`retry_argv_equals_first_minus_spec_32k` y `_128k` (el reintento difiere
SOLO en los 6 tokens spec, mismo `-c`).
