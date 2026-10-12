# Auditoría inicial del motor LLM local

Fecha: 2026-10-11. Alcance: ciclo de vida de `llama-server`, selección de
dispositivo, gate de arranque, estimación de VRAM, proxy OpenAI, perfiles,
instrumentación y evidencia de rendimiento/calidad. Al inicio había un
gateway en 17860, sin proceso de motor; no se descargó ningún modelo.

## Estado comprobado

- OMNI apunta a `Qwen3.8-27B-IQ4_XS_4BPW.gguf`, perfil `max_contexto`, contexto
  262144 y `cache_ram = 6144` en `%APPDATA%/LocalMind/localmind.toml`.
- El Qwen3.8 configurado sí está instalado en
  `%LOCALAPPDATA%/Programs/OMNI/models/Qwen3.8-27B-IQ4_XS_4BPW.gguf` y ocupa
  13,905,649,312 bytes. El usuario preguntó por Qwen3.7; no apareció en el
  perfil local ni en D: o E:. La versión local disponible es Qwen3.8.
- `llama-server --list-devices` reporta 15,569 MiB libres en RX 6800 XT
  (`Vulkan0`) y 7,589 MiB en la integrada (`Vulkan1`). El GGUF trae claves de
  arquitectura `qwen35.*`; el estimador KV actual no cubre esa arquitectura y
  debe dar `unknown`, no extrapolar la fórmula de Qwen3 uniforme.
- La RX 6800 XT y la GPU integrada aparecen como `Vulkan0` y `Vulkan1` en esta
  máquina. El `--device Vulkan0` fijo apunta aquí a la discreta, pero la app no
  selecciona por memoria ni permite escoger dispositivo.
- En una copia aislada, el motor pasó la puerta de aceptación a 32K. El
  estimador devolvió `unknown` para el GGUF `qwen35.*`, como corresponde. El
  proceso llegó a 14.36 GiB de VRAM dedicada, pero la RAM física libre bajó a
  226 MiB durante la carga. Se detuvo la campaña por estabilidad.
- Una primera muestra exploratoria con prompt de relleno de 514 tokens y dos
  respuestas de 64 tokens dio prefill de 191.13/34.26 tok/s, decode de
  29.54/33.19 tok/s y primer output a 3629/1045 ms. No hubo texto visible antes
  de alcanzar el límite de tokens; esa muestra no sirve para comparar latencia
  visible ni calidad. Evidencia: `tests/bench-results/engine-audit-20261011/A-cache0/bench-20261011-113911.json`.
  El harness ya añade una tarea visible fija y la guarda en los metadatos, pero
  todavía no se ha medido esa versión en vivo.
- La campaña A/B y los cinco checks de calidad siguen pendientes. Con el GGUF
  actual no se seguirá en esta máquina hasta disponer de RAM suficiente: el
  prechequeo vivo rechazó 6717 MiB libres frente a una reserva calculada de
  8679 MiB, sin abrir el puerto del motor. Los resultados de septiembre siguen
  siendo evidencia histórica, no una línea base reproducible del árbol actual.
- El gateway que estaba en 17860 dejó de escuchar durante la campaña. No se
  encontró un evento de Application Error/WER que determine la causa. El log
  local registra «Servidor detenido» cerca de ese intervalo; no se atribuye
  causalidad a la campaña. No se reinició porque el log de la instancia indica
  que estaba escuchando en `0.0.0.0` (modo LAN).

## Medición aislada y control de memoria

- El muestreador guardó 121 muestras en diez minutos: RAM libre mínima
  **226 MiB**, 13 muestras bajo 1 GiB y máximo **14,358.5 MiB** de memoria
  dedicada reportada para el proceso del motor. Tras detener el motor, la RAM
  libre se recuperó a más de 8 GiB.
- En el primer arranque válido, el modelo alcanzó `acceptance_ok=true` a 32K,
  con carga de 193.6 s. Es una observación de carga y compatibilidad, no una
  recomendación de perfil: el margen de RAM fue insuficiente.
- Se añadió un prechequeo de RAM física antes de `stop()` y `spawn()`: reserva
  `ceil(GGUF MiB / 2) + cache_ram + 2048 MiB`; si hay flags extra se reserva el
  GGUF completo. RAM insuficiente devuelve 400 y conserva el motor anterior;
  si Windows no entrega el dato, el log lo declara y continúa sin esa garantía.
- En la copia aislada, `POST /api/start` devolvió 400 dos veces: con 6717 y
  8338 MiB libres frente a 8679 requeridos. En ambos casos el puerto 8082 quedó
  cerrado; esas comprobaciones validan el bloqueo, no un rendimiento A/B.

## Hallazgos priorizados

| Prioridad | Hallazgo | Consecuencia | Siguiente corrección |
|---|---|---|---|
| P1 | Corregido en esta revisión: el cálculo anterior trataba `cache_ram` como KV en RAM y aproximaba el KV como `context × 32 B`. `--help` del build 10743 confirma que `--cache-ram` es caché de prompts; WMI además satura en esta RX 6800 XT. | El estado anterior podía declarar un fit falso o subestimar mucho la memoria activa. | Ahora se calcula KV Q4_0 desde metadatos GGUF admitidos y VRAM libre del dispositivo; se avisa, no se bloquea, si excede porque llama.cpp puede repartir capas en CPU. Si faltan datos o hay `extra_flags`, el resultado es desconocido. En vivo, `qwen35.*` queda correctamente como desconocido; hace falta soporte explícito de esa arquitectura para estimar su KV. |
| P1 | Los `extra_flags` se anexan después de las opciones administradas por OMNI y podrían cambiar dispositivo, contexto u offload, por lo que el prechequeo no puede afirmar `fits` cuando existen. | Evita el falso positivo de la estimación, pero deja que una flag avanzada cambie el arranque real; el gate de contexto sí comprueba `/props`. | La estimación pasa a `unknown` cuando hay flags extra; se bloquean específicamente los overrides del tipo KV para preservar la decisión `q4_0`. Revisar en una fase posterior los demás parámetros administrados y el dispositivo efectivo. |
| P2 | La migración deja `--cache-ram=0` en los perfiles integrados por seguridad. Eso desactiva la caché de prompts y podría aumentar el prefill de turnos repetidos frente a un límite mayor; no hay medición actual que decida la mejor capacidad. | Una configuración más conservadora de RAM puede perder una optimización de reutilización de prefijos. | Comparar 0 frente a límites acotados con el mismo historial y la misma profundidad real cuando haya un GGUF local; medir prefill, RAM y calidad. |
| P1 | Corregido en esta revisión: la tasa de la puerta estaba publicada como `decode_tps`; además, el cronómetro se detenía al recibir cabeceras, antes de leer el cuerpo. | Los clientes podían confundir la tasa de aceptación con decode y la medición no cubría la respuesta completa. La puerta sigue aceptando una de hasta dos muestras válidas y no prueba la GPU. | `request_tps` ahora mide completion tokens sobre request completa; `decode_tps` sólo sale de `timings.predicted_per_second`, o queda vacío. `engine_slow` compara únicamente decode real. UI diferencia tasa, decode y puerta. La GPU activa sigue sin verificarse. |
| P1 | Corregido en esta revisión: el tope de arranques por hora y el cooldown sólo vivían en la memoria del proceso. | Cerrar y relanzar OMNI reiniciaba el historial y permitía repetir cargas de 13 GB sin el tope anunciado. | `start-history.json` se carga al construir el manager y se escribe atómicamente antes de `spawn`; historial corrupto o no escribible bloquea el arranque. Tests cubren recrear el manager, reemplazo en Windows y fail-closed. |
| P1 | Corregido en esta revisión: el gate de arranque no miraba la RAM física libre. En la prueba aislada la carga dejó 226 MiB libres y el proceso usó hasta 14.36 GiB de VRAM dedicada. | La carga podía dejar Windows y el gateway sin margen; el motor llegó a aceptar una configuración con el sistema bajo presión severa. | Antes de detener el motor activo se reserva la mitad del tamaño del GGUF, el tope de caché de prompts y 2 GiB para el sistema; con flags extra se reserva el tamaño completo. Un valor medido insuficiente rechaza el arranque; dato no disponible se declara en el log. |
| P1 | `build_engine_cmd` fija `--device Vulkan0`; el gate tampoco verifica el adaptador elegido. | En equipos donde Vulkan ordene primero una GPU integrada, OMNI puede cargar en ella aunque exista una discreta. | Añadir selección visible del adaptador y comprobar el dispositivo realmente cargado antes de declarar el perfil validado. |
| P2 | La documentación SRS decía tres probes de gate, pero el código hace dos y acepta una válida. También fijaba `cargo test` en 129 casos aunque la ejecución actual obtuvo 222/0. | Evidencia de operación difícil de confiar cuando el diseño y el runtime divergen. Se actualizó la fila del gate y la cuenta observada. | Mantener requisitos y pruebas junto al comportamiento real; identificar con fecha las cifras históricas. |
| P2 | `panic = "abort"` en el perfil release coexiste con varios `catch_unwind`. | En release, esos bloques no recuperan un pánico como en builds con unwind; un fallo puede cerrar el proceso completo. No es una medición de rendimiento, pero afecta la estabilidad del motor/gateway. | Decidir entre quitar `catch_unwind` como falsa protección o compilar unwind donde la recuperación sea requisito; validar impacto y manejo de fallos. |
| P2 | Había benchmarks con profundidad aproximada y mezcla de latencia visible/razonamiento; además, el prompt sólo era relleno y el primer resultado vivo nunca llegó a texto visible. | No permitían distinguir el coste de prefill del tiempo de razonamiento ni comparar entradas de igual tamaño; `first_text_ms` quedaba vacío. | `tests/bench.mjs` calibra el prompt completo con `/apply-template` + `/tokenize`, añade una tarea visible fija y la guarda en los metadatos, usa el mismo `reasoning_effort: medium`, separa primer output/razonamiento/texto visible y rechaza comparaciones con tokens reales divergentes. Cada repetición debe emitir `READY`; si no, las métricas A/B quedan como no comparables. La tarea aún no tiene medición viva. |
| P2 | No había regresión compacta de calidad local asociada a la configuración medida. | Una subida de t/s podía ocultar peor seguimiento de instrucciones, extracción o recuperación en contexto. | Se añadió `tests/engine-quality.mjs` con cinco checks repetibles y comparación por resultado. No se ejecutó contra el GGUF actual por el margen insuficiente de RAM. |

## Lo que ya se preparó

- `tests/bench.mjs`: benchmark A/B por perfil y profundidad real, con
  `timings` del motor para prefill/decode, tiempos de razonamiento y primer
  texto, además de comparación justa por profundidad y tarea visible reproducible.
  Ahora admite selectores `<contexto>@<id-personalizado>`, valida el id con
  `/api/profiles` y captura `cache_ram` más una huella de `extra_flags`; el
  comparador exige modelo/build/ajustes iguales y no mezcla contextos asignados,
  profundidades reales ni flags distintas. `--workload ram-cache-churn` separa
  el caché de prompt en RAM del reuso del prefijo del slot activo y muestra los
  tokens reutilizados. Registra `READY` sin guardar el texto; si una muestra no
  emite el marcador, invalida las métricas A/B. También exige el fingerprint de
  la configuración de ejecución que expone `/api/config` sin revelar las flags.
  Las filas resumen las corridas completadas con medianas; el JSON conserva
  cada muestra y el comparador valida cada profundidad, marcador y mediana.
  Autoprueba actual: 50/50.
- `tests/pressure-sampler.mjs`: contador Windows consolidado en un ciclo por
  muestra; GPU Engine, memoria dedicada/compartida por adaptador y memoria del
  proceso. No deduce temperatura, clocks ni potencia.
- `tests/engine-quality.mjs`: casos de aritmética, extracción, JSON, orden y
  retrieval de una clave entre 80 registros; 8/8 autopruebas. Sólo usa un motor
  ya aceptado y nunca lo inicia/detiene. La prueba viva queda pendiente por el
  prechequeo de RAM.
- Mensaje de gate de baja tasa corregido en `process.rs`; ya no afirma que la
  GPU está apagada a partir de un umbral de velocidad.
- Los hints bilingües de los perfiles ya describen la demanda de memoria por
  contexto y no prometen almacenamiento del KV en DDR5 ni t/s garantizados.
- Salvaguardas que se conservan: enfriamiento de 120 s, máximo 4 arranques por
  hora, ausencia de reintento automático por lentitud y candado de batch/spec.

## Goal acordado

Mejorar el motor local de OMNI con una línea base reproducible de rendimiento y
calidad; corregir el cálculo de VRAM y las señales de aceptación para que
reflejen lo que miden; después aplicar una optimización escogida por evidencia
y validarla A/B a igual modelo, prompt y profundidad, con el set de calidad y
telemetría de recursos. El resultado debe mejorar latencia o throughput sin
degradar calidad ni saltarse límites de energía, estabilidad y compatibilidad
del gateway.

La campaña A/B y la prueba de calidad en vivo quedan pendientes porque el
prechequeo de RAM bloquea la carga con el margen disponible en este equipo. El
Qwen3.8-27B está instalado y respondió a la puerta de aceptación en una prueba
aislada; no se descargaron pesos ni se reinició el gateway real.

## Seguimiento de la auditoría

- El TOML real sigue en `profiles_version = 5`. Sus descripciones conservan
  afirmaciones heredadas de offload KV a DDR5 y de velocidad fija. La migración
  v6 ya apagaba `cache_ram` en los presets, pero no actualizaba esos textos.
  `PROFILES_VERSION = 7` ahora refresca nombres/descripciones sólo si coinciden
  exactamente con valores anteriores conocidos y deja intacto el texto propio.
- `cache_reuse = 256` se conserva por compatibilidad, pero OMNI no lo envía.
  El build 10683 rechazó la opción en cuatro combinaciones; la ayuda del 10743
  ya la ofrece con default 0. Falta probarla con Qwen3.8 en vivo antes de
  convertirla en una palanca del motor.
- Verificación de esta corrección: `cargo test --bin localmind` pasó 224/224;
  `cargo clippy --bin localmind -- -D warnings`, `cargo fmt --all -- --check` y
  `git diff --check` pasaron.
- En una lectura posterior, con OMNI y el motor detenidos, Windows reportó
  8.828 MiB de RAM física libre; es apenas 152 MiB más que la reserva de
  8.676 MiB del perfil 32K sin caché. La copia aislada del gateway había
  reportado 8.338 MiB y recibió el rechazo esperado. Aún no hay margen probado
  para ejecutar el gateway más el modelo; la campaña A/B y los checks vivos de
  calidad continúan pendientes. El gateway real sigue detenido y no se cambió
  la configuración del usuario.
- Revalidación anterior: OMNI, `llama-server` y los puertos
  17860/17861/8080/8082 siguen detenidos; Windows reporta 8.438 MiB de RAM
  física libre (8.504 MiB disponibles). El build local confirma por `--help`
  que `--cache-reuse` existe en 10743, tiene default 0 y requiere prompt
  caching; OMNI sigue sin enviarlo. El candidato medible queda acotado a
  `cache_ram=0` frente a 512 MiB con prompt y perfil idénticos; el nuevo workload
  conserva cada muestra y desplaza el slot activo con un prompt distinto antes
  de medir la reutilización. No se inició una carga porque
  la RAM sigue por debajo de los 8.679 MiB que requiere el motor sin gateway.
- Validación estática actual: `cargo test --bin localmind` 224/224;
  `cargo clippy --bin localmind -- -D warnings`, `cargo fmt --all -- --check`,
  `node tests/bench.mjs --self-test` y `node tests/engine-quality.mjs
  --self-test` pasaron; la suite de calidad obtuvo 8/8. Son pruebas de código y
  harness, no resultados de rendimiento ni de calidad generada por Qwen3.8.
- Hallazgo sobre la muestra A-cache0: la segunda petición con el prompt idéntico
  reportó 510 tokens reutilizados con `cache_ram=0`. Eso prueba reuso de prefijo,
  pero no el caché en host RAM que controla `--cache-ram`; faltaba intercalar un
  prompt distinto para desplazar el slot activo. El harness ofrece ahora
  `ram-cache-churn` (target, prompt distinto corto, target) y compara
  `cached_prompt_tokens`.
- Revalidación actual: `node --check tests/bench.mjs`,
  `node tests/bench.mjs --self-test` (44/44), `--dry-run` de los workloads
  `repeat` y `ram-cache-churn` y `git diff --check` pasaron. La ayuda del build
  local 10743 confirma que `--cache-ram=0` desactiva el caché de prompt en RAM;
  el prompt caching del slot se controla aparte. Windows reportó 7.501 MiB de
  RAM libre frente a los 8.679 MiB requeridos. No hay motor/gateway activo; la
  nueva campaña A/B y la calidad en vivo siguen pendientes.
- El endpoint `GET /api/config` ahora expone un fingerprint SHA-256 de los
  ajustes que cambian el argv/comportamiento del motor, junto con el conteo de
  flags extra sin revelar sus valores. El comparador exige esa huella. Una
  prueba Rust verifica que cambiar `extra_flags` cambia el hash y que el valor
  de la flag no se serializa. `cargo test --bin localmind` pasó 225/225,
  `cargo clippy --bin localmind -- -D warnings` y `cargo fmt --all -- --check`
  pasaron.
- Revalidacion del harness: `node --check tests/bench.mjs`, 50/50
  auto-pruebas, y `--dry-run` de `repeat` y `ram-cache-churn` pasaron.
  El esquema 4 guarda la mediana de cada metrica de probe, conserva las muestras
  crudas y el comparador verifica que cada corrida alcance la profundidad y el
  marcador `READY`, y que el resumen coincida con las muestras. No se ejecuta el
  modelo: OMNI, llama-server y los puertos 17860/17861/8080/8082 siguen detenidos.
  Relectura del sistema: WMI reporta 5.850 MiB libres y el contador de Windows
  `Memory\Available MBytes` 5.717 MiB disponibles; faltan 2.962 MiB para el
  gate de 8.679 MiB del perfil 32K sin cache. OMNI, llama-server y los puertos
  17860/17861/8080/8082 siguen detenidos. La campana A/B y la calidad en vivo
  siguen pendientes.
- Revalidacion Rust: `cargo test --bin localmind` pasa 225/225; `cargo clippy
  --bin localmind -- -D warnings` y `cargo fmt --all -- --check` pasan. La
  autoprueba del harness de calidad pasa 8/8; es evidencia del script, no de la
  calidad del modelo en vivo.
- Verificacion del runtime instalado, sin cargar el modelo: `llama-server`
  informa version 0.2.0-dev, build 10743, commit `adfffbe41`. Su `--help`
  diferencia `--cache-ram` (limite del prompt cache host, 0 lo desactiva),
  `--cache-prompt` (reuso del KV del slot, activado por defecto) y
  `--cache-reuse` (tamano minimo de chunk para KV shifting, default 0). La
  documentacion oficial confirma esa separacion:
  [server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md).
  Por eso los 510 tokens reutilizados en la muestra con `cache_ram=0` no prueban
  la cache host; `ram-cache-churn` es la prueba apropiada.
- Restriccion de calidad para el A/B de cache: la discusion upstream sobre
  [OOM del prompt cache](https://github.com/ggml-org/llama.cpp/discussions/28237)
  reporta crecimiento de RAM y fallos OOM, sin demostrar que este build/modelo
  concreto este afectado. El
  [issue #28368](https://github.com/ggml-org/llama.cpp/issues/28368) reporta
  variacion numerica por `cache_prompt` en Gemma-4 CPU build b10107; no es
  evidencia directa sobre Qwen3.8 en Vulkan. No se debe activar una variante
  globalmente: medir el cap 512 frente a 0 con telemetria y checks de calidad.
- El TOML activo de `%APPDATA%/LocalMind` sigue en `profiles_version = 5`, con
  `max_contexto` seleccionado a 262144 y `cache_ram = 6144` mas `-kvu`. La
  fuente contiene migracion v7 de perfiles heredados, pero no se inicio esa
  instancia para aplicarla: el gateway habia escuchado en LAN y el margen de
  RAM actual no alcanza el prechequeo. Para la campana se debe solicitar
  explicitamente el perfil 32K/cache 0 o el candidato A/B; no usar el perfil
  guardado de 262K como linea base.
- Revalidacion posterior (2026-10-11): se cargo el GGUF en forma aislada por
  loopback, sin gateway, con build 10743, Vulkan0, 32K, KV q4_0, `cache_ram=0`
  y `--load-mode none`. Con 5.43 GiB disponibles antes de arrancar, `/health`
  respondio OK y una peticion corta devolvio `READY`; durante la prueba quedaron
  4.43 GiB disponibles y el proceso uso 13,481.5 MiB de memoria dedicada GPU
  mas 578.8 MiB compartidos/no locales. Al detenerlo, Windows reporto 5.71 GiB
  disponibles. Esto demuestra que 8,679 MiB no es un requisito fisico universal:
  es el umbral conservador del prechequeo actual y no modela `load-mode none`.
  La corrida confirma arranque y generacion del servidor aislado a 32K, no una
  medicion de calidad ni una validacion del gateway OMNI.
- La puerta de RAM actual trata cualquier `extra_flags` como si reservara el
  GGUF completo y suma todo el limite `cache_ram`; eso incluye `-kvu` y una
  cache de prompts vacia al arranque. Revisar la estimacion por efecto real de
  cada flag y modo de carga antes de bloquear; conservar la reserva de sistema
  y los limites de frecuencia/energia de arranque. El contexto 128K habia sido
  aceptado historicamente, pero esos logs no registran el split real KV/VRAM.
