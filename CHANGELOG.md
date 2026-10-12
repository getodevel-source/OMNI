# Changelog OMNI

Todos los cambios publicables de la app. El formato sigue
[Keep a Changelog](https://keepachangelog.com/es-ES/1.0.0/) y el versionado
[SemVer](https://semver.org/lang/es/).

## [Unreleased]

## [2.1.3] - 2026-10-12

### Mejorado
- El icono de OMNI queda incrustado en el ejecutable de Windows para que el
  Explorador, la barra de tareas y los accesos directos muestren el logo.

## [2.1.2] - 2026-10-12

### Corregido
- El prechequeo de VRAM deja de tratar `cache_ram` como offload del KV y calcula
  KV Q4_0 sólo cuando GGUF y dispositivo aportan datos compatibles. El cálculo
  avisa en vez de bloquear y se declara desconocido cuando hay flags extra.
- `--cache-ram` se describe como límite de caché de prompts; los overrides del
  tipo KV quedan bloqueados para mantener q4_0.
- Los textos de perfiles ya no prometen spill a DDR5 ni una penalización de
  velocidad sin una medición que lo respalde.
- La migración v7 actualiza nombres y descripciones heredados de perfiles
  integrados cuando coinciden exactamente con textos anteriores conocidos;
  conserva los textos personalizados.
- La puerta de arranque separa su tasa de request completa de decode nativo,
  termina el cronómetro tras leer la respuesta y ya no se presenta como chequeo
  de GPU. La señal de lentitud sólo usa decode informado por llama.cpp; si
  faltan timings, la interfaz lo indica como una medición no disponible.
- El cooldown y el tope horario de arranques se persisten; un historial dañado
  o que no puede escribirse bloquea nuevas cargas. Los timestamps futuros se
  normalizan al momento actual sin perder un cooldown completo.
- La escritura del historial persistente ya no retiene su mutex durante la
  operación de disco.
- Antes de detener el motor existente, el arranque comprueba la RAM física libre
  frente al GGUF, el límite de caché de prompts y el margen de Windows; la falta
  de RAM medida devuelve 400 sin lanzar otro proceso.
- El benchmark ahora incluye una tarea visible fija después del relleno, registra
  si cada respuesta llega al marcador `READY` (sin guardar el texto generado) y
  rechaza métricas A/B si alguna variante se queda en razonamiento sin respuesta
  visible. La tarea y el marcador quedan en los metadatos del resultado.
- El benchmark añade `ram-cache-churn`: precalienta el prompt medido, desplaza el
  slot activo con otro prompt y mide la reutilización del caché en host RAM. Esto
  permite aislar `--cache-ram` de la reutilización del prefijo en el slot activo;
  las comparaciones también muestran `cached_prompt_tokens`.
- `GET /api/config` expone una huella SHA-256 de los ajustes efectivos del motor
  y sólo el conteo de flags extra; el comparador la exige para impedir A/B con
  cambios inadvertidos en polling, batch, dispositivo, energía o flags ocultas.
- La huella runtime también cubre métricas, configuración de `mmproj` y perfiles
  ordenados por id, para que el comparador detecte todos los cambios efectivos.
- El arranque resuelve perfil, modelo, contexto y `mmproj` desde una sola captura
  de configuración y comprueba la RAM antes de detener el motor que ya funciona.
- Las consultas de RAM y dispositivos al motor tienen tiempo límite. La
  estimación de VRAM queda como desconocida si hay un `mmproj` activo o flags
  extra que pueden cambiar la asignación de memoria.
- El parser del benchmark distingue la respuesta JSON completa de los eventos
  SSE y tolera etiquetas futuras en las líneas de memoria de dispositivos.
- Los analizadores de flags de carga y caché no consumen la opción siguiente
  cuando falta un valor; los bloqueos de seguridad ocurren antes de detener el
  motor activo y las muestras SSE conservan el último decode válido.
- Los controles de idioma y emparejamiento conservan sus etiquetas e iconos; el
  orden del foco sigue el orden visual en escritorio y móvil.
- El benchmark acepta perfiles A/B personalizados con etiqueta de contexto
  estable; verifica su configuración y rechaza comparaciones con modelo, build,
  contexto, profundidad de entrada o flags de perfil distintos.
- El benchmark resume las corridas repetidas con medianas y conserva cada
  muestra cruda. El comparador valida profundidad y marcador `READY` en todas
  las muestras y comprueba que cada mediana guardada coincide con sus datos.

## [Unreleased]

## [2.1.1] - 2026-10-11

### Corregido
- El icono de la ventana y el de la bandeja ahora también están disponibles
  desde el binario cuando faltan los archivos de imagen junto al ejecutable.
- La carga de iconos prueba las ubicaciones disponibles antes de usar el
  recurso embebido como respaldo.

## [2.1.0] - 2026-10-10

### Añadido
- Topes anti-OOM: 25 MB en `/v1/*` (413 si excede) y 64 KiB en los JSON
  pequeños (`/api/unlock`, `/api/import_model`, `/api/config`…).
- Watchdogs de `busy` (10 min + latido por chunk) en update y descargas de
  modelos: un worker muerto ya no deja 409 para siempre.
- SSE con heartbeat `: ping` cada 25 s: un cliente ido ya no fuga el hilo.
- `POST /api/open_data_dir` (allowlist `appdata|temp`) + tarjeta «Datos y
  desinstalación» en Ajustes (rutas, Abrir carpeta, comando `Copy-Item` de
  backup, flags de desinstalación). Claves es/en.
- Botón **Reintentar** en la tarjeta de update; `GET /api/models/download`
  traduce la falta de espacio a 507 temprano.
- Pre-chequeo de espacio en descargas de modelos (regla 2×, mensaje 507 con GB).
- `localmind.toml` ilegible deja `localmind.toml.bak` antes de usar defaults.
- `OMNI_UPDATE_API` solo acepta `https://` con host real (un env `http://`
  cae al default seguro).
- Recuperación de update revalida tamaños (EXE > 1 MB, HTML > 1 KB) y purga
  stagings huérfanos de >7 días; el swap fallido restaura `.prev-<ver>` y
  deja la orden manual en el log.
- Import unificado (misma resolución + tmp+rename atómico + `import_lock`):
  409 por variante `Exists`, `.GGUF` en mayúsculas aceptado.
- UI: pairing oculto con Mostrar temporal, PID/Activo y Gen t/s traducidos,
  «Decidir después» con aviso persistente en Conexión, tablas con scroll
  horizontal, tabs con `tablist` + flechas.
- Packaging/CI: `cargo fmt --check` + `clippy -D warnings` en la release,
  firma Authenticode opcional desde secretos, SHA256+tamaño pineados del
  runtime, `install.ps1` con opcionales (`version.txt`/`LEEME.txt`),
  `uninstall.ps1` con guardarraíl de TargetDir y sin autodestruirse.

### Seguridad
- La clave del motor viaja por env `LLAMA_API_KEY` del hijo, nunca en argv
  (el argv lo lee cualquier proceso local: Task Manager, wmic, ETW).
- Puerta D-7: todo lo no público exige clave, incluido `GET /api/version`;
  `hf.token` y `remote.key` con ACL solo-usuario (aviso si `icacls` falla,
  sin filtrar la clave).
- Flags `--log-file`/`--out-file` bloqueadas (escritura arbitraria fuera del
  log rotado); el candado también rige en persistencia.
- `POST /api/start` serializado (`start_lock`): dos arranques concurrentes
  no lanzan dos motores.
- ZipSlip por prefijo real en `stage_zip` y descargas; swap `.ps1` con
  comillas simples + `-LiteralPath` (sin expansión `$`).

### Corregido
- `catch_unwind` por request y en workers: un pánico ya no mata gateway+UI
  ni deja `busy` clavado.
- `update_set` sobrevive a Mutex envenenado; `snippet` corta por chars (sin
  pánicos UTF-8).
- `POST /api/profiles/import` propaga el error de `save` (500) en vez de un
  falso «actualizado».
- Tests con SKIP honesto (sin PASS inflados) y favicons `omni.*` cubiertos.

## [2.0.12] - 2026-10-10

### Corregido (canal invisible: cero consola, reapertura garantizada)
- Swap en PowerShell `.ps1` lanzado oculto (`-WindowStyle Hidden` +
  `CREATE_NO_WINDOW`): nunca más una terminal visible ni colgada.
- Espera activa al lock (15×200 ms) en vez de `ping` fijo; `Start-Process`
  desacoplado relanza la app; autoborrado + traza en todas las salidas.
- UI: estado `installing` visible con botones bloqueados (claves es/en).

## [2.0.11] - 2026-10-10

### Corregido (canal perfecto: sin consola, sin estados rotos, sin 409 eternos)
- Swap silencioso: `cmd` con `CREATE_NO_WINDOW` + stdio a null (nada visible).
  El script vuelca traza a `%TEMP%\omni-swap-<ver>.log` (OK/FAIL por paso) y
  se autoborra en TODAS las salidas. El fallo de spawn queda en el log.
- Invariante `ready` ⇒ staging válido: `POST /restart` valida el dir en disco
  y auto-repara a `error` honesto (`mark_staging_lost`) si se perdió.
- Watchdog de `busy` (10 min): un worker muerto ya no bloquea el canal con
  409 para siempre (`take_busy_or_recover` + test).
- Harness `update-flow.mjs` con check 2b (ready con staging en disco): 7/7.

## [2.0.10] - 2026-10-09

### Añadido (auditoría: que la suite pesque lo que se vio en vivo)
- `renderUpdate` exportada en el factory de `tests/ui-render.mjs` + barrido
  de los 7 estados (cualquier excepción futura rompe la suite). Probado en
  negativo contra el `ui.html` de v2.0.8: lanza `banner is not defined` y
  devuelve la clave cruda, como en la captura del dueño.

## [2.0.9] - 2026-10-09

### Corregido (tu captura: tarjeta de update rota al buscar)
- `renderUpdate` usaba `banner` sin declararlo → `ReferenceError` visible
  como «banner is not defined» en el error-banner. Ahora resuelve
  `#update-banner` como el resto.
- Faltaba `settings.updateChecking` (y asimetrías `updateRestart`/
  `updateError`/`logFailed` entre dicts): la tarjeta pintaba la clave cruda.
  Dicts simétricos 300/300.
- Barrido permanente en `tests/ui-render.mjs`: cada literal `T('…')` MUST
  existir en ambos dicts (el bug no puede volver sin romper la suite).

## [2.0.8] - 2026-10-09

### Añadido (asesor canirun honesto: antes de descargar, no después de fallar)
- `POST /api/models/advise {repo, revision?, files?}`: resuelve el árbol HF,
  estima fit contra VRAM+RAM del host y devuelve `{verdict, file, size_mb,
  profile?, detail, hint_quant?}` (`fits`/`tight`/`no_fit`/`unknown`/
  `no_gguf`/`no_file`). Sin descargas, sin estado. Test `asesor_veredictos`.
- Botón «Evaluar» junto a Descargar + veredicto con perfil sugerido y guía
  de cuantización si no cabe (claves es/en). La descarga sigue igual.

## [2.0.7] - 2026-10-09

### Corregido (P0: el drill real rompió el canal en vivo)
- El swap operaba sobre el `base_dir` de datos en vez del dir del exe: en la
  máquina de desarrollo intentó mover `C:\PROYECTOS\OMNI` y la app quedó
  pidiendo reinicio para siempre. Ahora `do_quit` usa `app_dir()` (dir del
  exe) y `prepare_install_on_exit` rechaza dirs sin `OMNI.exe` o con cara de
  repo dev (`check_app_dir` + test).
- Nuevo `POST /api/update/restart` (409 sin `ready`) + botón «Reiniciar e
  instalar» en la tarjeta (claves es/en): salida ordenada por la vía oficial
  sin taskkill. El tick del loop ejecuta stop+swap+Exit en ≤5 s.
- Harness `tests/update-flow.mjs` (6 checks, sin red ni swap): forma del
  canal, restart-409, botón en UI, scripts sin rutas dev, version.update.

## [2.0.6] - 2026-10-09

### Añadido (adaptativo: la app se ajusta al usuario, no al revés)
- Capacidades por modelo en `GET /api/models` (`capabilities: {family,
  ctx_native, thinking}`, heurística honesta por nombre, sin parser GGUF).
- Hardware completo en `GET /api/hardware` (`cpu_physical`, `ram_total_mb`,
  `vram_total_mb` numéricos para la UI y el guard).
- UI adaptativa: contexto acotado al nativo del modelo (opciones de más
  desactivadas), aviso si el modelo no trae thinking, hilos reescritos con
  los físicos reales (recomendado/mitad/todos), backend visible solo-lectura,
  sugerencia de modelo pequeño en estado vacío. Claves es/en.
- Puerta lenta relativa al tamaño (`slow_threshold`: 2B→25, 27B→20, 70B→12;
  cfg como techo) y sampler por familia (Llama/Mistral propios).
- Warning en Logs si se pide effort a modelo sin thinking (no se rechaza).

## [2.0.5] - 2026-10-09

### Corregido (canal auto-update: el drill v2.0.4 lo rompió en vivo)
- `stage_zip` ya no falla si el ZIP trae `models/`: la omite (ni puntero ni
  pesos pisan lo del usuario) y rechaza `.gguf` fuera de `models/`. Tests:
  `staging_omite_models_y_acepta_puntero_viejo`,
  `staging_rechaza_gguf_fuera_de_models`.
- El ZIP portable ya no trae `models/` (ni el puntero): `get_base_dir` cae
  al dir del exe igual (rama 3).
- El swap repone `models/` desde el respaldo `.prev-<ver>` (robocopy) y al
  arrancar hay healing (`heal_models`: copia lo ausente sin pisar). Test
  `heal_models_repone_desde_respaldo_sin_pisar`.

## [2.0.4] - 2026-10-09

### Probado
- Drill del canal auto-update: release mínima para verificar detección,
  descarga verificada e instalación al reiniciar desde la propia app.

## [2.0.3] - 2026-10-09

### Añadido (portabilidad: pensado para otro hardware, no solo esta máquina)
- Guard de VRAM pre-arranque: `POST /api/start` rechaza con 400 y GB
  concretos si `modelo + KV(contexto)` no cabe en la VRAM (margen 0.9,
  `cache_ram` descuenta). Sin dato de VRAM nunca bloquea. Test
  `vram_fit_avisa_con_gb_y_no_bloquea_sin_dato`.
- Onboarding: estado vacío de Modelos con sugerencia + botón que rellena un
  repo pequeño real (`Qwen/Qwen2.5-1.5B-Instruct-GGUF`, ~2 GB). Claves
  `models.starterHint/starterFill` es/en. No descarga solo.
- Puerta lenta relativa al tamaño: `slow_threshold` (≤9 GB→25, ≤17 GB→20,
  >17 GB→12 t/s; cfg como techo). Un 2B en integrada y un 70B ya no comparten
  el umbral calibrado para 27B. Test `slow_threshold_escala_con_tamano_y_cfg_es_techo`.
- Sampler por familia (`Qwen|Llama|Mistral|Generic` por nombre de archivo):
  Llama (0.6/40/0.9) y Mistral (0.7/40/1.0) dejan de recibir el sampler Qwen;
  Qwen/Bonsai/desconocido = Qwen (medido). Solo rellena ausentes; lo
  explícito gana. Test `sampler_por_familia_qwen_llama_mistral_generico`.
- Backend GPU visible y honesto: fila solo-lectura en Ajustes → Motor con el
  `device` efectivo (`Vulkan0`…); `engine.device` sigue solo-lectura en API
  (se cambia en el TOML). Clave `settings.backendNote` es/en.

## [2.0.2] - 2026-10-09

### Corregido
- Canal de auto-update para terceros: el feed default apuntaba al nombre
  viejo del repo (`getodevel-source/LocalMind`, mudado a `OMNI`). Ahora
  apunta a `getodevel-source/OMNI` con test (`canal_default_apunta_al_repo_omni`).
  Sin esto, una config limpia chequeaba contra la redirección en vez del repo real.

## [2.0.1] - 2026-10-09

### Corregido
- VRAM falsa en AMD: drivers que saturan `AdapterRAM` con techos falsos
  (medido: 4095 MB en una RX 6800 XT de 16 GB) ya no ensucian el dato real
  del motor: el WMI solo se usa sin dato de `--list-devices` y con umbral
  ≥8 GB (D-21). `GET /api/hardware` muestra lo real del motor.
- El chat servía Qwen3.8 siempre en su modo más lento y propenso a fallar
  (`xhigh` por defecto de la plantilla + sampler ajeno): ahora el gateway
  aplica `reasoning_effort: medium` y el sampler oficial cuando el cliente
  no fija los valores, y el chat trae selector de razonamiento
  (Medio por defecto, Bajo/Alto/Max/Off) más los valores de Ajustes →
  Generación. Lo explícito (agentes, benches) no se toca.
- Single-instance sin carrera (D-14): el lock ya no se borra a ciegas antes
  de crear; solo un huérfano confirmado (sonda de lectura con `share_mode(0)`)
  se reclama. Con dueño vivo se enfoca la ventana y se sale.
- El fallback de perfiles ya es bilingüe (D-48): `label`/`hint` por `T()`
  en es/en en vez de español hardcodeado.
- Contratos de docs fijados a la UI real de 4 tabs (Panel/Chat/Conexión/
  Ajustes): SRS §§1.2/3.7/D-27 + `TAB_ALIASES` para deep-links viejos.
### Seguridad
- `gateway.key` con ACL solo-usuario (`icacls /inheritance:r`, best-effort
  al crear/cargar/rotar): otro usuario local ya no la lee por defecto.
### Robustez
- Update que sobrevive al apagón duro: al arrancar se recuperan stagings
  válidos olvidados en `%TEMP%\omni-update` como pendientes; la descarga
  exige 2× espacio libre medido (507 `Sin espacio…` si no cabe, sin bloquear
  si no se puede medir).
- VRAM visible (D-21 parcial): `GET /api/hardware` anota uso real vía
  `nvidia-smi` cuando existe (en AMD sin contador estable no se inventa);
  el chip «Motor lento» trae botón «Reiniciar motor» (gesto manual único,
  respeta cooldown).
- Avisos que evitan sorpresas: toast único al ocultar a la bandeja (la X no
  cierra, la VRAM sigue retenida) y pre-aviso 5 min antes del auto-stop
  (cualquier actividad lo cancela).
- Empaquetado de un solo runtime: `build-portable.ps1` rechaza `bin-hip/` y
  `bin.prev*/` en staging y en el ZIP (el canal es Vulkan en `bin/`).
- Firma opcional (D-24 parcial): `build-portable.ps1 -Sign` firma `OMNI.exe`
  con Authenticode (pfx o almacén, con timestamp); sin secreto avisa y sigue
  sin firmar. Sin cert no hay historia SmartScreen.
- LAN honesta sin TLS: el gateway sigue en HTTP plano (`tiny_http` sin stack
  TLS; cambiarlo exige otro servidor) y ahora lo declara (`GET /api/lan`
  con `plaintext_http: true`) y lo avisa en la tarjeta (es/en). P7 decidida
  por el dueño (D-29 cerrada): sin visión hasta revisar P7, con test.
- VRAM AMD honesta (D-21 parcial-honesta): techo instalado vía WMI
  `AdapterRAM` cuando no hay `nvidia-smi`; el uso en AMD sigue sin contador
  y no se inventa.
- Estructura: `mod sse` → `sse.rs` y puerta pura del motor → `engine_gate.rs`
  (7 fns + 5 tests movidos); `mod proxy` se queda (acoplado a `server.rs`).
- DeepSeek versionado (D-28 mitigada): el patch declara `formato v1`;
  D-32 cerrada como harness-side (doble evidencia del timeout 600 s).
### Añadido
- Actualización automática dentro de la app: chequeo silencioso contra
  GitHub Releases al arrancar (cada 6 h), tarjeta «Actualización» en
  Ajustes (buscar / descargar / cancelar + aviso al reiniciar) y banner en
  el Panel. Descarga con resume, verificación obligatoria por tamaño +
  `SHA256 <asset> <hex>` del body de la release (sin checksum no se
  instala), staging validado y swap de directorios al salir con respaldo
  `.prev-<ver>`. `models/` y `%APPDATA%\LocalMind` jamás se tocan.
  Endpoints: `GET /api/update`, `POST /api/update/check`,
  `POST /api/update/download`, `POST /api/update/cancel`
  (`GET /api/version` expone además `update:{state,latest,pending}`).
  Config `[update]`: `feed` (vacío = repo por defecto), `check_on_startup`
  (default true), `auto_download` (default false).
- Pipeline de release: workflow `release.yml` (tag `vX.Y.Z` → build →
  `SHA256SUMS.txt` → GitHub Release con el ZIP portable + body con línea
  `SHA256 <asset> <hex>`), `CHANGELOG.md` y `version.txt` trazable
  (versión + commit + fecha UTC).

## [2.0.0] - 2026-10-03

### Añadido
- ZIP portable (`packaging/build-portable.ps1`), instalador
  (`packaging/install.ps1`) y desinstalador (`packaging/uninstall.ps1`)
  con preservación de `models/`.
- Gateway local + motor `llama-server`, 4 tabs (Panel, Chat, Conexión,
  Ajustes), modo Oráculo/Guest, pairing LAN por QR, descarga verificada
  de modelos desde Hugging Face.
