# Manual de usuario OMNI

## Requisitos

- Windows 10/11 de 64 bits con WebView2 (lo trae Edge; sin WebView2 la
  ventana no abre).
- Modelos `.gguf` aparte (el ZIP no los trae: pesan ~14 GB). 16 GB de VRAM
  para el `Qwen3.8-27B`; con menos VRAM usa un `.gguf` más pequeño.
- Puertos libres: gateway `127.0.0.1:17860` (+10 intentos) y motor
  `llama_port` (default 8080, siguiente libre si choca).

## Instalación

No requiere administrador (todo vive en `%LOCALAPPDATA%` + `%APPDATA%`).

1. Extrae `OMNI-portable-<versión>-<fecha>.zip` donde quieras
   (p. ej. `C:\OMNI`) o ejecuta `packaging/install.ps1` para instalar en
   `%LOCALAPPDATA%\Programs\OMNI` con accesos directos.
2. Copia tus `.gguf` a `models/` junto a `OMNI.exe`.
3. Ejecuta `OMNI.exe`. En SmartScreen pulsa «Más información → Ejecutar de
   todos modos» (la app aún no firma código).

P1 producción: instalar desde el repo requiere build previo (`cargo build
--release` o `packaging/build-portable.ps1`); `version.txt`/`LEEME.txt` se
generan en el portable y el instalador los copia con aviso si faltan.

## Primer arranque

Checklist: busca tu caso y haz lo indicado.

| Caso | Acción |
|---|---|
| Sin modelos («Sin modelos — importa un .gguf») | Ajustes → Modelos → **Ir a Modelos**: importa tu `.gguf` o usa **Evaluar** + Descargar uno verificado |
| Sin motor (el Chat dice que está apagado) | Panel → elige modelo y perfil → **Iniciar Motor** |
| Sin VRAM (el arranque avisa con los GB que faltan) | usa un `.gguf` más pequeño (ver tabla de abajo) |
| Rol Guest (usas el modelo de otro) | Conexión → pega la URL del Oráculo + clave |

1. Elige el rol de esta PC (Oráculo = tiene el modelo; Guest = usa el de
   otro). Es un diálogo accesible (foco inicial, Tab contenido, Esc =
   decidir después). «Decidir después» deja un aviso en
   Conexión → Rol hasta elegir.
2. En el Panel elige modelo y perfil, pulsa **Iniciar Motor**.
3. Chatea en la tab Chat. Los agentes (Pi, OMP, OpenCode, DeepSeek) se
   lanzan desde el Panel con el motor en marcha.
4. Ajustes guarda perfiles, generación, motor/avisos y rol (Oráculo/Guest).
   La UI es bilingüe (ES/EN, selector en el encabezado): etiquetas,
   estados (Gen t/s, PID/Activo), botones y avisos cambian de idioma,
   incluido el sufijo «(copia)» al duplicar perfiles.

### ¿Qué se puede estimar antes de descargar?

El botón **Evaluar** (`POST /api/models/advise`) solo recibe el tamaño del GGUF
publicado en Hugging Face. No conoce sus metadatos de arquitectura, el contexto
que se usará ni la memoria libre del adaptador seleccionado, así que devuelve
`unknown` y explica cuánto ocupan los pesos frente a un presupuesto de
referencia. No interpreta que el modelo no pueda arrancar: llama.cpp puede dejar
parte de las capas en CPU.

Después de descargar, el arranque intenta estimar pesos + KV `q4_0` con los
metadatos GGUF y la memoria libre del dispositivo elegido. La estimación solo
cubre arquitecturas densas reconocidas; si faltan datos, lo indica. Un exceso es
una advertencia y el motor intenta cargar, porque puede repartir capas en CPU.
La puerta posterior comprueba que el servidor responda.

`cache_ram` configura la caché de prompts del servidor en RAM. El valor `0` la
desactiva. Esa caché no mueve el KV activo a RAM ni permite ampliar el contexto;
los perfiles integrados la dejan apagada mientras se valida su comportamiento.

## Actualización automática

- La app comprueba GitHub Releases al arrancar y cada 6 h (silencioso).
- Si hay novedad, el Panel muestra un banner y **Ajustes → Actualización**
  ofrece **Descargar** (con progreso y **Cancelar**).
- La instalación ocurre **al reiniciar** (botón **Reiniciar e instalar**):
  la app para el motor, un swap de directorios reemplaza el runtime y la
  app reabre sola. Tus `models/` y `%APPDATA%\LocalMind` (config, claves)
  se conservan siempre.
- Si el swap falla a mitad (staging corrupto), restaura el respaldo
  `OMNI.prev-<versión>` solo y deja la orden manual en el log del swap
  (`%TEMP%\omni-swap-<versión>.log`): copia esa carpeta sobre la app y
  reabre `OMNI.exe`.
- Si la tarjeta muestra un error, pulsa **Reintentar** (repite el chequeo).
- Opciones en `localmind.toml` (`[update]`): `feed` (repo `owner/name`,
  vacío = oficial), `check_on_startup` (default true), `auto_download`
  (default false).
- Sin red o sin releases: la tarjeta dice «Al día» o el motivo del fallo;
  nada se descarga solo.

## Datos y desinstalación

Ajustes → **Datos y desinstalación** muestra las rutas, abre cada carpeta
(`POST /api/open_data_dir`, allowlist `appdata|temp`) y ofrece el comando
de backup para copiar.

| Qué | Dónde |
|---|---|
| App (exe, UI, `bin/`, `models/`) | junto a `OMNI.exe` (portable o `%LOCALAPPDATA%\Programs\OMNI`) |
| Config `localmind.toml` (+`.bak` si el TOML llegó ilegible), clave `gateway.key`, token `hf.token` | `%APPDATA%\LocalMind` |
| Logs `logs/localmind.log` (1 MiB ×3) | `%APPDATA%\LocalMind\logs` |
| Lock single-instance | `%TEMP%\localmind.lock` |
| Staging de update | `%TEMP%\omni-update\<versión>\staging` |

Backup (PowerShell, el mismo que muestra la tarjeta):

```powershell
Copy-Item "$env:APPDATA\LocalMind" "$env:USERPROFILE\Desktop\LocalMind-backup" -Recurse
```

Los `.gguf` se respaldan aparte. Para migrar de PC: instala, pega `models/`
y restaura `localmind.toml` + `gateway.key`.

Desinstalar:

```powershell
powershell -ExecutionPolicy Bypass -File packaging\uninstall.ps1
```

Flags: `-RemoveModels` borra también `models/` (~14 GB); `-RemoveData`
borra `%APPDATA%\LocalMind` (config, claves, tokens, logs: úsalo antes de
vender o regalar el PC); `-Force` detiene un `OMNI.exe` en marcha en vez
de negarse. Sin flags conserva modelos y datos. El script exige huella
(`OMNI.exe` o `version.txt`) y nunca borra raíces ni carpetas protegidas.
Si se lanza desde el acceso directo (el script vive en el propio dir
instalado), el resto `uninstall.ps1` se conserva a propósito: no se
autodestruye en caliente.

Para verificar que no queda resto: comprueba que no existen el dir instalado
(`%LOCALAPPDATA%\Programs\OMNI`), `%APPDATA%\LocalMind` (si usaste
`-RemoveData`), `%TEMP%\omni-update` y los accesos del Menú Inicio.

## Conexión / LAN

- Default: solo `127.0.0.1` (esta PC). Con `[lan].enabled = true` (Ajustes
  → Rol → Oráculo → Exponer, **rige al reiniciar**) el gateway escucha en
  `0.0.0.0` **en HTTP plano**: la clave y los prompts viajan sin TLS en tu
  LAN. Úsalo solo en redes de confianza (la tarjeta lo avisa; `GET /api/lan`
  declara `plaintext_http: true`).
- Empareja el Guest con el QR o copiando `omni://<ip>:<puerto>#k=<clave>`
  (Conexión). Tras **Regenerar clave**, reinicia para que tome efecto.

## Problemas comunes

| Síntoma | Causa y arreglo |
|---|---|
| No abre / se cierra al instante | WebView2 ausente (instala Edge); mira `%APPDATA%\LocalMind\logs\localmind.log` |
| SmartScreen avisa al abrir `OMNI.exe` | normal sin firma de código (la firma es opcional): «Más información → Ejecutar de todos modos» |
| «Puerto ocupado» | otro OMNI o app en 17860–17869 (`http_port` + 10 intentos): cierra el otro o cambia `http_port` |
| El motor no arranca | sin `.gguf` en `models/`; VRAM insuficiente; mira el log |
| Segunda instancia no abre | normal: el lock `%TEMP%\localmind.lock` enfoca la ventana viva |
| Update dice «sin paquete verificado» | la release no publica `SHA256 <asset> <hex>`: espera a la release corregida |
| Update «tamaño inesperado / SHA no coincide» | descarga corrupta: reintenta Descargar |
| Reinstalar encima falla | cierra OMNI; si hay update pendiente (`%TEMP%\omni-update`), reinicia la app para aplicarlo primero |
| Importar modelo no responde | el diálogo es nativo: si lo cancelas, la UI dice «cancelado»; si lo dejas 120 s abierto, expira solo con el mismo aviso |

## English (quick start)

The UI is fully bilingual (ES/EN switch in the header).

1. Extract `OMNI-portable-<version>-<date>.zip` anywhere or run
   `packaging/install.ps1` (installs to `%LOCALAPPDATA%\Programs\OMNI`).
2. Copy your `.gguf` weights into `models/` next to `OMNI.exe` (they never
   ship in the ZIP, ~14 GB).
3. Run `OMNI.exe`. SmartScreen warns on first run (no code signature yet):
   «More info → Run anyway». Needs WebView2 (comes with Edge) and free
   ports: gateway `127.0.0.1:17860` (+10 tries) and engine `llama_port`
   (default 8080, next free on clash).
4. First run: pick this PC's role (Oracle = holds the model; Guest = uses
   someone else's). No models → Settings → Models → **Go to Models**
   (import a `.gguf` or **Evaluate** + download a verified one). No engine
   → Dashboard → pick model + profile → **Start Engine**. Low VRAM → use a
   smaller `.gguf` (Evaluate tells you `fits`/`tight`/`no_fit`).
5. Update: the app polls GitHub Releases on start and every 6 h; download
   from **Settings → Update**, install happens on restart (button
   **Restart & install**) with `.prev-<version>` rollback. `models/` and
   `%APPDATA%\LocalMind` are never touched.
6. Data & uninstall: **Settings → Data & uninstall** shows the paths, opens
   each folder, and offers the backup command
   (`Copy-Item "$env:APPDATA\LocalMind" "$env:USERPROFILE\Desktop\LocalMind-backup" -Recurse`).
   Uninstall with `packaging\uninstall.ps1` (`-RemoveModels` wipes weights,
   `-RemoveData` wipes config/keys/logs, `-Force` stops a running instance).
