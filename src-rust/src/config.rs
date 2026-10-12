use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HardwareProfile {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_context")]
    pub context: usize,
    #[serde(default)]
    pub cache_ram: usize,
    #[serde(default)]
    pub extra_flags: Vec<String>,
}

fn default_context() -> usize {
    32768
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeculationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_spec_type")]
    pub ty: String,
    #[serde(default = "default_spec_n")]
    pub draft_n_max: usize,
    #[serde(default = "default_spec_p")]
    pub draft_p_split: f32,
}

impl Default for SpeculationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ty: "draft-mtp".to_string(),
            draft_n_max: 2,
            draft_p_split: 0.1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default = "default_device")]
    pub device: String,
    /// None → default 6 (ajustar a núcleos físicos de la CPU)
    #[serde(default)]
    pub threads: Option<usize>,
    #[serde(default)]
    pub threads_batch: Option<usize>,
    #[serde(default = "default_priority")]
    pub priority: String,
    /// Prioridad de hilos batch (--prio-batch). Desdoblada de --prio: batch en alta
    /// prioridad eleva hilos de cómputo por encima de foreground y satura la GPU
    /// (tirones); "0" = hilos batch sin elevar, --prio interactivo intacto.
    #[serde(default = "default_priority_batch")]
    pub priority_batch: String,
    #[serde(default = "default_batch")]
    pub batch: usize,
    /// Prioridad de proceso (Windows): 0=bajo, 1=abajo-normal, 2=normal. "below_normal"
    /// protege la interactividad del sistema en sesiones largas.
    #[serde(default = "default_process_priority")]
    pub process_priority: String,
    /// Nivel de polling (llama-server --poll). 0 = sin spin-wait (CPU/PSU descansan);
    /// 50 = default de llama.cpp (latencia mínima pero CPU 100% entre tokens).
    #[serde(default = "default_poll")]
    pub poll: u32,
    /// threads_batch separado: batch de prompt es el pico máximo de CPU/VRAM.
    #[serde(default = "default_true")]
    pub limit_threads_batch: bool,
    /// Auto-stop del motor tras N segundos sin actividad de generación.
    /// Default 5400 (90 min): el motor residente evita ciclos stop/start
    /// entre sesiones de agentes; para liberar VRAM usar Stop manual.
    /// 0 = desactivado.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Pausa mínima entre arranques del motor (seguridad de energía LM-NF-3).
    /// Cada arranque lee ~13 GB a VRAM: es el transitorio más grande del
    /// sistema y los arranques seguidos disparan la protección de la PSU.
    #[serde(default = "default_start_cooldown")]
    pub start_cooldown_secs: u64,
    /// Tope de arranques por hora rodante (seguridad de energía LM-NF-3).
    #[serde(default = "default_max_starts_per_hour")]
    pub max_starts_per_hour: u32,
    /// Umbral de puerta lenta (t/s): bajo este valor `engine_slow = true`
    /// (solo informativo, sin reintentos: LM-NF-3 lo prohíbe).
    #[serde(default = "default_slow_gate_tps")]
    pub slow_gate_tps: f64,
    /// Modo seguro de energía (default true, SIN restringir contexto):
    /// guardarraíl + flags bloqueadas + sin auto-reintentos + aviso en
    /// 262K. Ningún perfil se rechaza por este flag.
    #[serde(default = "default_true")]
    pub power_safe: bool,
    #[serde(default)]
    pub extra_flags: Vec<String>,
    #[serde(default = "default_aliases")]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub speculation: Option<SpeculationConfig>,
    #[serde(default = "default_true")]
    pub flash_attention: bool,
    #[serde(default = "default_true")]
    pub reasoning_preserve: bool,
    #[serde(default = "default_true")]
    pub metrics: bool,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// Puerto preferido del motor (llama-server). Si está ocupado se busca el siguiente libre.
    #[serde(default = "default_llama_port")]
    pub llama_port: u16,
    /// Reutilización de prefijos de prompt en KV cache (legado, inerte en OMNI).
    /// El build 10683 rechazó `--cache-reuse` en este contexto en cuatro
    /// combinaciones (medido 2026-09-27). El build 10743 sí incluye la opción,
    /// pero su efecto en Qwen3.8 y este tamaño de contexto sigue sin validarse en vivo;
    /// `build_engine_cmd` no la envía hasta medirla. Se conserva el campo para
    /// no romper TOMLs/APIs que lo lean.
    #[serde(default = "default_cache_reuse")]
    pub cache_reuse: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            device: "Vulkan0".to_string(),
            threads: None,
            threads_batch: None,
            priority: "2".to_string(),
            priority_batch: "0".to_string(),
            batch: 1024,
            process_priority: "below_normal".to_string(),
            poll: 0,
            limit_threads_batch: true,
            idle_timeout_secs: 5400,
            start_cooldown_secs: 120,
            max_starts_per_hour: 4,
            slow_gate_tps: 20.0,
            power_safe: true,
            extra_flags: Vec::new(),
            aliases: default_aliases(),
            speculation: None,
            flash_attention: true,
            reasoning_preserve: true,
            metrics: true,
            http_port: 17860,
            llama_port: 8080,
            cache_reuse: default_cache_reuse(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MmprojConfig {
    /// true → detectar automáticamente el mmproj más afín al modelo cargado
    #[serde(default = "default_false")]
    pub auto: bool,
    /// Lista de candidatos (orden de prioridad) en models/
    #[serde(default = "default_mmproj_files")]
    pub files: Vec<String>,
}

impl Default for MmprojConfig {
    fn default() -> Self {
        Self {
            auto: false,
            files: default_mmproj_files(),
        }
    }
}
/// Id de perfil por defecto: siempre uno real de `built_in_profiles()` (D-1:
/// el fantasma `turbo` nunca debe asomar en el estado ni en `last.profile`).
pub const DEFAULT_PROFILE_ID: &str = "velocidad";

/// Versión de config materializada. Historial:
/// v1 = `--no-mmap` en `libros`; v2 = vuelta a `[]`; v3 = `--load-mode none`
/// en `libros`; v4 = vuelta a `[]` (SEGURIDAD LM-NF-3); v5 = `[engine]`
/// (idle 1500→5400 + cooldown 120 + tope 4/h + power_safe + slow_gate_tps 20);
/// v6 = caché de prompts `--cache-ram` desactivada por defecto: no es offload KV;
/// v7 = actualizar nombres y descripciones antiguas de perfiles integrados.
/// La migración refresca no-customizados (fotos previas reconocidas).
pub const PROFILES_VERSION: u32 = 7;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    /// Interruptor general de avisos de escritorio (P20).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Avisar cuando el motor pasa la puerta de aceptación.
    #[serde(default = "default_true")]
    pub on_ready: bool,
    /// Avisar cuando el motor falla (puerta o crash).
    #[serde(default = "default_true")]
    pub on_failure: bool,
    /// Avisar cuando el auto-stop apaga el motor.
    #[serde(default = "default_true")]
    pub on_autostop: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_ready: true,
            on_failure: true,
            on_autostop: true,
        }
    }
}

/// Red local (Fase B, diseño B1/B2): opt-in explícito para exponer el gateway
/// más allá de loopback. Default `false` = comportamiento histórico (solo
/// `127.0.0.1`, sin ningún cambio). Se lee al arrancar: cambiarlo exige
/// reiniciar la app (el socket se liga una vez, sin hot-swap en v1).
/// `default` para TOMLs viejos sin sección.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LanConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// Modo Cliente (Fase B3, diseño B1/B6): esta PC no computa, consume el
/// Servidor remoto. Con `enabled`, `/api/start` se bloquea (409) y el
/// chat `/v1/*` se proxyea al remoto (B3b). Default `false` = Todo-aquí.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// Canal de actualización automática (Fase Prod): chequeo silencioso contra
/// GitHub Releases + descarga verificada + instalación al reiniciar.
/// `feed` vacío = repo por defecto (`update::DEFAULT_FEED_REPO`).
/// `check_on_startup` (default true) y `auto_download` (default false):
/// por defecto solo AVISA; la descarga la pide el dueño.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    #[serde(default)]
    pub feed: String,
    #[serde(default = "default_true")]
    pub check_on_startup: bool,
    #[serde(default)]
    pub auto_download: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            feed: String::new(),
            check_on_startup: true,
            auto_download: false,
        }
    }
}

/// Servidor remoto del modo Cliente (diseño B5/B6): `url` base (con o sin
/// `/v1`, se normaliza al usar) y `key` Bearer. La UI jamás la devuelve
/// completa (enmascarada) y nunca viaja en logs ni URLs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationConfig {
    /// Temperatura de muestreo (0..=2). Base para exponer en la API (P16).
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    /// Top-p (0..=1).
    #[serde(default = "default_top_p")]
    pub top_p: f64,
    /// Máximo de tokens a generar (1..=32768).
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// Semilla (>= 0).
    #[serde(default)]
    pub seed: i64,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: 1.0,
            max_tokens: 2048,
            seed: 0,
        }
    }
}

/// Validadores de la API de ajustes (`GET/POST /api/config`, `POST
/// `/api/profiles/save|delete`). Sin pánicos: la UI envía parciales y el
/// servidor responde `400 {"error":"<mensaje español nombrando el campo>"}`.
/// (Antes `allow(dead_code)`: ahora los consume `server.rs`.)
pub fn gen_temperature_ok(v: f64) -> bool {
    v.is_finite() && (0.0..=2.0).contains(&v)
}
pub fn gen_top_p_ok(v: f64) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}
pub fn gen_max_tokens_ok(v: usize) -> bool {
    (1..=32768).contains(&v)
}
pub fn gen_seed_ok(v: i64) -> bool {
    v >= 0
}
/// `idle_timeout_secs`: 0 = desactivado; si no, 60 s..7 días.
pub fn engine_idle_timeout_ok(v: u64) -> bool {
    v == 0 || (60..=604800).contains(&v)
}
/// `threads`: None = auto (núcleos físicos); si se fija, 1..=256.
pub fn engine_threads_ok(v: usize) -> bool {
    (1..=256).contains(&v)
}
/// Prioridad de hilos del motor (`--prio`): la UI ofrece `2` (alta) y `0`.
pub fn engine_priority_ok(v: &str) -> bool {
    v == "2" || v == "0"
}
/// Id de perfil: `^[a-z0-9_-]{1,32}$` (lo que la UI usa como clave).
pub fn profile_id_ok(id: &str) -> bool {
    if id.is_empty() || id.len() > 32 {
        return false;
    }
    id.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
/// Contexto de perfil: 1024..=1048576 en pasos de 1024.
pub fn profile_context_ok(v: usize) -> bool {
    (1024..=1048576).contains(&v) && v.is_multiple_of(1024)
}
/// `cache_ram` (MB): 0..=65536.
pub fn profile_cache_ram_ok(v: usize) -> bool {
    v <= 65536
}
/// Una flag extra: ≤64 chars, charset `[A-Za-z0-9 _.=:/-]`.
pub fn profile_flag_ok(f: &str) -> bool {
    if f.is_empty() || f.len() > 64 {
        return false;
    }
    f.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b' ' | b'_' | b'.' | b'=' | b':' | b'/' | b'-')
    })
}
/// La lista completa de flags: ≤8 entradas.
pub fn profile_flags_ok(flags: &[String]) -> bool {
    flags.len() <= 8
        && flags.iter().all(|f| profile_flag_ok(f))
        && cache_type_override_flag(flags).is_none()
}

/// El tipo de KV cache es una decisión de memoria del motor y se fija en q4_0.
/// Evita que `extra_flags` tenga más precedencia que los argumentos gestionados.
pub fn cache_type_override_flag(flags: &[String]) -> Option<&str> {
    flags.iter().map(String::as_str).find(|flag| {
        ["-ctk", "-ctv", "--cache-type-k", "--cache-type-v"]
            .iter()
            .any(|name| {
                *flag == *name
                    || flag.starts_with(&format!("{}=", name))
                    || flag.starts_with(&format!("{} ", name))
            })
    })
}

/// Candado PSU + escritura en disco (LM-NF-3, P1-IO): flags que suben el consumo
/// más allá del set seguro medido o que permiten escribir ficheros arbitrarios.
/// Rechaza `-ub`/`--ubatch-size` > 512, `-b`/`--batch-size` > 1024, cualquier
/// `--spec-*` (el draft especulativo mueve el pico de potencia) y cualquier
/// `--log-file`/`--out-file` (el motor escribiría en disco fuera de su log
/// rotado). Vale TAMBIÉN en persistencia (`extra_flags_persist_ok`): lo guardado
/// ya pasa este candado, no solo el arranque.
/// Devuelve la flag ofensora en español. Sin pánicos.
pub fn psu_unsafe_flag(flags: &[String]) -> Option<String> {
    let mut i = 0;
    while i < flags.len() {
        let f = flags[i].as_str();
        // Pares valorados: -ub N / -b N (y formas largas/iguales).
        let val_of = |j: usize| -> Option<usize> {
            let raw = flags.get(j)?.as_str();
            let num = raw.strip_prefix('=').unwrap_or(raw);
            num.parse::<usize>().ok()
        };
        let takes_val = |name: &str| -> bool {
            f == name
                || f.starts_with(&format!("{}=", name))
                || f.starts_with(&format!("{} ", name))
        };
        if f == "-ub" || f == "--ubatch-size" || f.starts_with("--ubatch-size=") {
            let v = if f.contains('=') {
                f.rsplit('=').next().and_then(|n| n.parse::<usize>().ok())
            } else {
                val_of(i + 1)
            };
            if v.is_some_and(|n| n > 512) {
                return Some(format!("Flag bloqueada por seguridad de energía (PSU): «{}» supera el máximo seguro 512.", flags[i]));
            }
        } else if f == "-b" || f == "--batch-size" || f.starts_with("--batch-size=") {
            let v = if f.contains('=') {
                f.rsplit('=').next().and_then(|n| n.parse::<usize>().ok())
            } else {
                val_of(i + 1)
            };
            if v.is_some_and(|n| n > 1024) {
                return Some(format!("Flag bloqueada por seguridad de energía (PSU): «{}» supera el máximo seguro 1024.", flags[i]));
            }
        } else if f == "--spec-type" || f.starts_with("--spec-") {
            return Some(format!("Flag bloqueada por seguridad de energía (PSU): «{}» altera el draft especulativo medido.", flags[i]));
        } else if f == "--log-file"
            || f.starts_with("--log-file=")
            || f.starts_with("--log-file ")
            || f == "--out-file"
            || f.starts_with("--out-file=")
            || f.starts_with("--out-file ")
        {
            // P1-IO: el motor escribiría ficheros arbitrarios fuera de su log
            // rotado. Bloqueo por presencia (cualquier valor es una ruta).
            return Some(format!("Flag bloqueada por seguridad de escritura: «{}» permite escribir ficheros fuera del log rotado.", flags[i]));
        } else if takes_val("--ubatch-size") || takes_val("--batch-size") {
            // Formas con espacio ya cubiertas arriba por prefijo; sin valor
            // explícito no se puede juzgar: se deja pasar (el default manda).
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LastSettings {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub context: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub engine: EngineConfig,
    #[serde(default)]
    pub mmproj: MmprojConfig,
    /// Vacío → perfiles integrados
    #[serde(default)]
    pub profiles: Vec<HardwareProfile>,
    #[serde(default)]
    pub last: LastSettings,
    /// Avisos de escritorio (P20). `default` para TOMLs viejos sin sección.
    #[serde(default)]
    pub notifications: NotificationsConfig,
    /// Parámetros de generación (P16). La puerta usa temperature 0 fijo.
    #[serde(default)]
    pub generation: GenerationConfig,
    /// Versión de los perfiles integrados materializados (migración tuning).
    /// Ausente en TOMLs viejos → 0. Actual: `PROFILES_VERSION`.
    #[serde(default)]
    pub profiles_version: u32,
    /// Red local (Fase B): ausente en TOMLs viejos → `false` (todo igual).
    #[serde(default)]
    pub lan: LanConfig,
    /// Modo Cliente (Fase B3): ausente → `false`.
    #[serde(default)]
    pub client: ClientConfig,
    /// Servidor remoto (Fase B3): ausente → vacío.
    #[serde(default)]
    pub remote: RemoteConfig,
    /// Canal de actualización (Fase Prod): ausente → defaults (avisar, no auto-descargar).
    #[serde(default)]
    pub update: UpdateConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            mmproj: MmprojConfig::default(),
            profiles: built_in_profiles(),
            last: LastSettings::default(),
            notifications: NotificationsConfig::default(),
            generation: GenerationConfig::default(),
            profiles_version: PROFILES_VERSION,
            lan: LanConfig::default(),
            client: ClientConfig::default(),
            remote: RemoteConfig::default(),
            update: UpdateConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Defaults helpers
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}
fn default_device() -> String {
    "Vulkan0".to_string()
}
fn default_priority() -> String {
    "2".to_string()
}
fn default_priority_batch() -> String {
    "0".to_string()
}
fn default_batch() -> usize {
    1024
}
fn default_process_priority() -> String {
    "below_normal".to_string()
}
fn default_poll() -> u32 {
    0
}
fn default_idle_timeout() -> u64 {
    5400 // 90 min: el motor residente evita ciclos stop/start entre sesiones
         // (cada arranque = transitorio PSU); para liberar VRAM usar Stop manual.
}
fn default_start_cooldown() -> u64 {
    120
}
fn default_max_starts_per_hour() -> u32 {
    4
}
fn default_slow_gate_tps() -> f64 {
    20.0
}
fn default_false() -> bool {
    false
}
fn default_http_port() -> u16 {
    17860
}
fn default_llama_port() -> u16 {
    8080
}
fn default_cache_reuse() -> usize {
    256
}
fn default_spec_type() -> String {
    "draft-mtp".to_string()
}
fn default_spec_n() -> usize {
    2
}
fn default_spec_p() -> f32 {
    0.1
}
fn default_temperature() -> f64 {
    0.7
}
fn default_top_p() -> f64 {
    1.0
}
fn default_max_tokens() -> usize {
    2048
}
fn default_aliases() -> Vec<String> {
    vec!["localmind".to_string(), "qwen3.8-27b".to_string()]
}
fn default_mmproj_files() -> Vec<String> {
    vec![
        "mmproj-BF16.gguf".to_string(),
        "mmproj-F16.gguf".to_string(),
    ]
}

/// Perfiles integrados universales. Sobrescribibles vía [[profiles]] en el TOML.
pub fn built_in_profiles() -> Vec<HardwareProfile> {
    vec![
        HardwareProfile {
            id: "velocidad".to_string(),
            // Fase C: el id NO cambia (compat TOML/migración/tests); el nombre
            // visible sí: este es el perfil por defecto ("Recomendado").
            name: "Recomendado · 32K (el más rápido)".to_string(),
            description: "Contexto de 32K. La caché de prompts en RAM está desactivada; esto no cambia dónde se aloja el KV activo. Ideal para chat, preguntas y código corto (<30k palabras).".to_string(),
            context: 32768,
            cache_ram: 0,
            extra_flags: Vec::new(),
        },
        HardwareProfile {
            id: "multi_doc".to_string(),
            name: "Multi-documento · 64K (velocidad levemente menor)".to_string(),
            description: "Contexto de 64K para varios documentos o repositorios. La caché de prompts en RAM está desactivada; esto no mueve el KV activo fuera de la VRAM.".to_string(),
            context: 65536,
            cache_ram: 0,
            extra_flags: Vec::new(),
        },
        HardwareProfile {
            id: "libros".to_string(),
            name: "Libros largos · 128K contexto extendido".to_string(),
            description: "Contexto de 128K para libros o proyectos grandes. La caché de prompts en RAM está desactivada; esto no mueve el KV activo fuera de la VRAM.".to_string(),
            context: 131072,
            // `cache_ram` limita la caché de prompts del servidor, no la
            // ubicación del KV activo. La caché queda apagada por defecto
            // mientras se valida corrección y beneficio en este build. Las
            // mediciones históricas no prueban offload de KV.
            // A/B prefill 8k @128K (mismo prompt 10546 tok, misma máquina):
            //   mmap default → prefill 21,65 t/s · decode 14,01 t/s
            //   sin mmap     → prefill 38,18 t/s · decode 13,92 t/s
            // SEGURIDAD (LM-NF-3, 2026-09-28, dos apagones duros): se vuelve al
            // default con mmap para que los arranques repetidos sean baratos
            // (cada arranque re-lee ~13 GB; sin page-cache el transitorio es
            // mayor y más frecuente = riesgo PSU). `--load-mode none` queda
            // como OPT-IN por perfil (medido: RAM libre +5 GB, prefill +76 %,
            // decode igual): añadir `extra_flags = ["--load-mode", "none"]`.
            // No tocar -ub/-b/hilos/spec en este perfil (PSU).
            cache_ram: 0,
            extra_flags: Vec::new(),
        },
        HardwareProfile {
            id: "max_contexto".to_string(),
            name: "Máximo contexto · 262K (límite físico del modelo)".to_string(),
            description: "Alcanza los 262,144 tokens que el modelo soporta nativamente (~200k palabras: bases de código masivas). La memoria para el KV activo depende del modelo, contexto y hardware; `cache_ram` no lo desborda a RAM.".to_string(),
            context: 262144,
            // El barrido histórico comparó tamaños de la caché de prompts,
            // no el offload del KV activo; no sirve para dimensionar contexto.
            // Se desactiva por defecto mientras se valida su corrección y
            // beneficio. SEGURIDAD (LM-NF-3): load mode mmap por defecto;
            // `--load-mode none` solo opt-in por perfil.
            // No tocar -ub/-b/hilos/spec en este perfil (PSU).
            cache_ram: 0,
            extra_flags: vec!["-kvu".to_string()],
        },
    ]
}

/// Foto de los integrados en una versión dada. Al subir `PROFILES_VERSION`
/// se conservan TODAS las fotos previas que la migración deba reconocer:
/// v0/v2 = `libros` sin flags; v1 = `libros` con `--no-mmap`;
/// v3 = `libros` con `--load-mode none`; v4 = vuelta a `[]` (seguridad);
/// v5 = perfiles previos a desactivar la caché de prompts por defecto.
/// El resto idéntico en todas. Solo campos migrables. Sin pánicos.
pub fn previous_built_in_field(id: &str, version: u32) -> Option<(usize, usize, Vec<String>)> {
    match (id, version) {
        ("velocidad", _) => Some((32768, 0, Vec::new())),
        ("multi_doc", 6) => Some((65536, 0, Vec::new())),
        ("multi_doc", _) => Some((65536, 4096, Vec::new())),
        ("libros", 0) | ("libros", 2) | ("libros", 4) => Some((131072, 6144, Vec::new())),
        ("libros", 5) => Some((131072, 6144, Vec::new())),
        ("libros", 6) => Some((131072, 0, Vec::new())),
        ("libros", 1) => Some((131072, 6144, vec!["--no-mmap".to_string()])),
        ("libros", 3) => Some((
            131072,
            6144,
            vec!["--load-mode".to_string(), "none".to_string()],
        )),
        ("max_contexto", 6) => Some((262144, 0, vec!["-kvu".to_string()])),
        ("max_contexto", _) => Some((262144, 6144, vec!["-kvu".to_string()])),
        _ => None,
    }
}

/// Foto de los defaults de `[engine]` que la migración puede refrescar.
/// Solo campos con default cambiado o nuevo: `idle_timeout_secs`
/// (1500 → 5400), `start_cooldown_secs` (ausente/90 → 120),
/// `max_starts_per_hour` (ausente/3 → 4), `power_safe` (ausente → true),
/// `slow_gate_tps` (ausente → 20.0). `None` = el campo no existía en esa
/// versión (ausente en el TOML cuenta como no customizado). Sin pánicos.
pub struct EnginePrevDefaults {
    pub idle_timeout_secs: Option<u64>,
    pub start_cooldown_secs: Option<u64>,
    pub max_starts_per_hour: Option<u32>,
    pub power_safe: Option<bool>,
    pub slow_gate_tps: Option<f64>,
}

/// Foto anterior de `[engine]` según la versión del TOML. v0-v3 no tenían
/// estos campos versionados (el TOML materializado trae 1500 o lo que el
/// dueño puso); v4 trae 90/3 pero sin `slow_gate_tps`; v5 es la actual.
pub fn previous_engine_defaults(version: u32) -> EnginePrevDefaults {
    match version {
        0..=3 => EnginePrevDefaults {
            idle_timeout_secs: Some(1500),
            start_cooldown_secs: None,
            max_starts_per_hour: None,
            power_safe: None,
            slow_gate_tps: None,
        },
        4 => EnginePrevDefaults {
            idle_timeout_secs: Some(1500),
            start_cooldown_secs: Some(90),
            max_starts_per_hour: Some(3),
            power_safe: Some(true),
            slow_gate_tps: None,
        },
        _ => EnginePrevDefaults {
            idle_timeout_secs: Some(5400),
            start_cooldown_secs: Some(120),
            max_starts_per_hour: Some(4),
            power_safe: Some(true),
            slow_gate_tps: Some(20.0),
        },
    }
}

/// Refrescar `[engine]` materializado no customizado (LM-NF-3): cada campo se
/// actualiza solo si sigue igual a ALGUNA foto previa (o estaba ausente y la
/// foto dice `None`, que también cuenta como no customizado). El resto de
/// `[engine]` (`threads`, `batch`, `priority`, puertos…) jamás se toca.
/// Devuelve los nombres tocados (para el log).
pub fn migrate_engine_defaults(cfg: &mut AppConfig, stored_version: u32) -> Vec<String> {
    // Regla: un campo se toca solo si su valor coincide con ALGUNA foto
    // previa (incluida la ausencia = `None` en la foto, que cuenta como no
    // customizado) y difiere del actual. Como serde ya materializó defaults
    // al parsear, "ausente" no se distingue del default actual por el valor:
    // por eso la ausencia se detecta por VERSIÓN (foto `None`), no por valor.
    // Campos que el TOML viejo no traía (cooldown/cap/power/slow en v0-v3, o
    // slow en v4) se escriben siempre: el dueño no pudo customizar lo que no
    // existía como default documentado… SALVO que el valor difiera del actual
    // (entonces sí lo puso a mano y se respeta).
    let cur = EngineConfig::default();
    let mut prevs = Vec::new();
    for v in stored_version..PROFILES_VERSION {
        prevs.push(previous_engine_defaults(v));
    }
    if prevs.is_empty() {
        return Vec::new();
    }
    let mut touched = Vec::new();
    // `idle_timeout_secs`: existe desde v0 con foto 1500 → 900 intacto.
    if prevs
        .iter()
        .any(|p| p.idle_timeout_secs == Some(cfg.engine.idle_timeout_secs))
        && cfg.engine.idle_timeout_secs != cur.idle_timeout_secs
    {
        cfg.engine.idle_timeout_secs = cur.idle_timeout_secs;
        touched.push("idle_timeout_secs".to_string());
    }
    // Campos nuevos: si alguna foto dice `None` (no existía), un valor igual
    // al actual es "ausente por serde" → nada que hacer; un valor DISTINTO
    // del actual con foto `None`… también pudo ser custom en v4 (cooldown 90
    // era default v4). Solo migrar si coincide con foto `Some` previa.
    macro_rules! mig_new {
        ($field:ident, $touched:literal) => {
            if prevs.iter().any(|p| p.$field == Some(cfg.engine.$field))
                && cfg.engine.$field != cur.$field
            {
                cfg.engine.$field = cur.$field;
                touched.push($touched.to_string());
            }
        };
    }
    mig_new!(start_cooldown_secs, "start_cooldown_secs");
    mig_new!(max_starts_per_hour, "max_starts_per_hour");
    mig_new!(power_safe, "power_safe");
    mig_new!(slow_gate_tps, "slow_gate_tps");
    touched
}
/// Refrescar perfiles materializados no customizados hacia los integrados
/// actuales, aceptando como "no customizado" el valor de CUALQUIER versión
/// previa (`stored_version..PROFILES_VERSION`): el usuario solo customiza si
/// su valor no coincide con ninguna foto previa. Regla por id conocido:
/// `extra_flags` se actualiza solo si cada flag guardada ya estaba en alguna
/// foto previa (el usuario no añadió nada); `cache_ram`/`context` solo si
/// siguen iguales a alguna foto previa. Nombre y descripción solo se refrescan
/// cuando coinciden exactamente con una presentación anterior conocida. Ids
/// desconocidos intactos. Devuelve los ids tocados (para el log).
pub fn migrate_builtin_profiles_from(
    stored: &mut [HardwareProfile],
    stored_version: u32,
) -> Vec<String> {
    let current = built_in_profiles();
    let mut touched = Vec::new();
    for p in stored.iter_mut() {
        let cur = match current.iter().find(|c| c.id == p.id) {
            Some(c) => c,
            None => continue,
        };
        let mut prevs = Vec::new();
        for v in stored_version..PROFILES_VERSION {
            if let Some(prev) = previous_built_in_field(&p.id, v) {
                prevs.push(prev);
            }
        }
        if prevs.is_empty() {
            continue;
        }
        let flags_untouched = prevs
            .iter()
            .any(|prev| p.extra_flags.iter().all(|f| prev.2.contains(f)));
        let mut changed = false;
        if flags_untouched && p.extra_flags != cur.extra_flags {
            p.extra_flags = cur.extra_flags.clone();
            changed = true;
        }
        if prevs.iter().any(|prev| p.context == prev.0) && p.context != cur.context {
            p.context = cur.context;
            changed = true;
        }
        if prevs.iter().any(|prev| p.cache_ram == prev.1) && p.cache_ram != cur.cache_ram {
            p.cache_ram = cur.cache_ram;
            changed = true;
        }
        if let Some((old_names, old_descriptions)) = previous_built_in_presentation(&p.id) {
            if old_names.contains(&p.name.as_str()) && p.name != cur.name {
                p.name = cur.name.clone();
                changed = true;
            }
            if old_descriptions.contains(&p.description.as_str())
                && p.description != cur.description
            {
                p.description = cur.description.clone();
                changed = true;
            }
        }
        if changed {
            touched.push(p.id.clone());
        }
    }
    touched
}

/// Textos de versiones anteriores de perfiles integrados. Solo se usan para
/// refrescar presentación si el valor guardado coincide exactamente con un
/// texto que OMNI publicó antes; nombres y descripciones personalizados siguen
/// intactos.
pub fn previous_built_in_presentation(
    id: &str,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match id {
        "velocidad" => Some((
            &[
                "Velocidad máxima · 32K contextos cortos",
                "Recomendado · 32K (el más rápido)",
            ],
            &[
                "Todo el modelo y su memoria de conversación (KV cache) viven en los 16 GB de la GPU. Es el modo más rápido: ~17 t/s. Ideal para chat, preguntas y código corto (sesiones de menos de ~30k palabras).",
                "Todo el modelo y su memoria de conversación (KV cache) viven íntegramente en la VRAM de la GPU. Es el modo más rápido con mínima latencia. Ideal para chat, preguntas y código corto (<30k palabras).",
            ],
        )),
        "multi_doc" => Some((
            &["Multi-documento · 64K (velocidad levemente menor)"],
            &[
                "El modelo vive en VRAM y parte del KV cache pasa a RAM DDR5. Renuncia ~5% de velocidad para sostener ~60k palabras de contexto: varios documentos o un repo de código mediano en la misma conversación.",
                "El modelo reside en VRAM y parte del KV cache pasa a la memoria RAM del sistema. Cede ~5% de velocidad para sostener ~60k palabras de contexto: múltiples documentos o repositorios completos.",
            ],
        )),
        "libros" => Some((
            &["Libros largos · 128K contexto extendido"],
            &[
                "Modelo intacto en VRAM + KV cache ampliado en RAM DDR5: sostiene ~120k palabras (un libro entero, un repo mediano). La velocidad de respuesta baja ~15-20% frente a Velocidad máxima porque parte de la memoria del prompt viaja por DDR5.",
                "Modelo en VRAM + KV cache ampliado en RAM del sistema: sostiene ~120k palabras (un libro entero o proyecto grande). Velocidad moderada (~15-20% menor que Velocidad máxima) al cursar tráfico por el bus de memoria.",
            ],
        )),
        "max_contexto" => Some((
            &["Máximo contexto · 262K (límite físico del modelo)"],
            &[
                "Alcanza los 262,144 tokens que el modelo soporta nativamente (~200k palabras: biblia, code bases enormes). El más lento y el que más tensiona CPU/PSU: úsalo solo cuando necesites sujetar la obra completa en una sola conversación.",
                "Alcanza los 262,144 tokens que el modelo soporta nativamente (~200k palabras: bases de código masivas). Requiere memoria RAM compartida considerable; ideal para cargas analíticas profundas.",
            ],
        )),
        _ => None,
    }
}

/// Atajo para tests: migrar como si el TOML estuviera en v0.
#[cfg(test)]
pub fn migrate_builtin_profiles(stored: &mut [HardwareProfile]) -> Vec<String> {
    migrate_builtin_profiles_from(stored, 0)
}

// ---------------------------------------------------------------------------
// ConfigStore: carga/escritura del TOML en %APPDATA%\LocalMind\localmind.toml
// ---------------------------------------------------------------------------

pub struct ConfigStore {
    path: PathBuf,
    inner: Arc<RwLock<AppConfig>>,
    /// Nota de migración pendiente de loguear (la sirve `take_migration_note`).
    pending_note: Arc<RwLock<Option<String>>>,
}

impl ConfigStore {
    pub fn load(base_dir: &Path) -> Self {
        Self::load_from_path(&Self::config_path(base_dir))
    }

    /// Núcleo testeable sin env global (los tests en paralelo comparten
    /// `APPDATA` y `set_var` es data race): carga desde un path explícito.
    pub fn load_from_path(path: &Path) -> Self {
        let path = path.to_path_buf();
        let raw = std::fs::read_to_string(&path).ok();
        // Un TOML ILEGIBLE no es lo mismo que un archivo ausente. El ausente es
        // el primer arranque: se calla. El ilegible deja defaults en memoria y
        // sin esta nota el siguiente `save()` sobrescribe la config del usuario
        // con defaults y nadie se entera nunca.
        let mut ilegible_note: Option<String> = None;
        // P2 producción: el TOML ilegible también deja `.bak` (antes solo la
        // migración v5 lo escribía). Sin copia, un `save()` posterior pisa la
        // config rota del usuario con defaults y no hay forma de rescatarla.
        let mut cfg = match raw.as_ref() {
            Some(text) => match toml::from_str::<AppConfig>(text) {
                Ok(c) => c,
                Err(e) => {
                    let bak = path.with_extension("toml.bak");
                    // P2 producción: best-effort igual que la migración v5; el
                    // test exige el `.bak`, no el `is_ok` intermedio.
                    let _ = std::fs::write(&bak, text);
                    ilegible_note = Some(format!(
                        "[LocalMind] No se pudo leer la configuración de {}: {}. Se usan los valores por defecto (copia de seguridad en {}); corregir o borrar el archivo restaura la config.",
                        path.display(),
                        e,
                        bak.display()
                    ));
                    AppConfig::default()
                }
            },
            None => AppConfig::default(),
        };
        // Migración: ids de perfiles de la era anterior → nombres actuales.
        let legacy = ["turbo", "balanced", "deep", "ultra"];
        if cfg.profiles.iter().any(|p| legacy.contains(&p.id.as_str())) {
            let last = cfg.last.clone();
            cfg = AppConfig::default();
            cfg.last = last;
        }
        // D-1: si no hay perfiles, reponer los integrados; si `last.profile`
        // apunta a un id inexistente (p. ej. `turbo` heredado), caer al default
        // real para que el estado nunca exponga un perfil fantasma.
        if cfg.profiles.is_empty() {
            cfg.profiles = built_in_profiles();
        }
        let known = |id: &str| cfg.profiles.iter().any(|p| p.id == id);
        if cfg.last.profile.as_deref().is_some_and(|id| !known(id)) {
            cfg.last.profile = Some(DEFAULT_PROFILE_ID.to_string());
            cfg.last.context = None;
        }
        // Migración v5: perfiles (misma regla segura de siempre) + `[engine]`
        // (solo valores que siguen en defaults previos: idle 1500→5400 y los
        // campos nuevos; customs como threads/batch/puertos jamás se tocan).
        // Atómica best-effort; la nota nombra perfiles y campos tocados.
        let mut tuned_note: Option<String> = None;
        if raw.is_some() && cfg.profiles_version < PROFILES_VERSION {
            // Backup del TOML previo (irreversible sin copia: un bug de
            // migración no puede costar la config del dueño).
            if let Some(text) = raw.as_ref() {
                let bak = path.with_extension("toml.bak");
                let _ = std::fs::write(&bak, text);
            }
            let from = cfg.profiles_version;
            let touched_p = migrate_builtin_profiles_from(&mut cfg.profiles, from);
            let touched_e = migrate_engine_defaults(&mut cfg, from);
            cfg.profiles_version = PROFILES_VERSION;
            let mut parts = Vec::new();
            if !touched_p.is_empty() {
                parts.push(format!("perfiles: {}", touched_p.join(", ")));
            }
            if !touched_e.is_empty() {
                parts.push(format!("engine: {}", touched_e.join(", ")));
            }
            if !parts.is_empty() {
                tuned_note = Some(format!(
                    "[LocalMind] Config actualizada a v{}: {}",
                    PROFILES_VERSION,
                    parts.join("; ")
                ));
            }
            if let Ok(text) = toml::to_string_pretty(&cfg) {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let tmp = path.with_extension("toml.tmp");
                if std::fs::write(&tmp, text).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }
        Self {
            path,
            inner: Arc::new(RwLock::new(cfg)),
            // La nota de ilegibilidad pisa a la de migración: solo una de las
            // dos puede ocurrir, y perder la config es lo grave.
            pending_note: Arc::new(RwLock::new(ilegible_note.or(tuned_note))),
        }
    }

    fn config_path(base_dir: &Path) -> PathBuf {
        std::env::var("APPDATA")
            .map(|p| Path::new(&p).join("LocalMind").join("localmind.toml"))
            .unwrap_or_else(|_| base_dir.join("localmind.toml"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self) -> AppConfig {
        self.inner.read().clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut AppConfig)) {
        f(&mut self.inner.write());
    }

    /// Servir y consumir la nota de migración (una sola vez, para `mgr.log`).
    pub fn take_migration_note(&self) -> Option<String> {
        self.pending_note.write().take()
    }

    /// Persistir en disco (best-effort).
    pub fn save(&self) -> Result<(), String> {
        let snapshot = self.inner.read().clone();
        let text = toml::to_string_pretty(&snapshot).map_err(|e| e.to_string())?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.path, text).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Perfiles (Fase A3, vuelta a raíces): antes vivían en `profiles.rs` como
// indirección de un módulo de 43 líneas. Ahora son parte de `config.rs`.
// `profiles.rs` queda como shim de compatibilidad (`pub use`) hasta que se
// actualicen los ~21 puntos de uso en `server.rs`/`process.rs`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareProfileDto {
    pub id: String,
    pub name: String,
    pub description: String,
    pub context: usize,
    pub cache_ram: usize,
    pub extra_flags: Vec<String>,
}

impl From<HardwareProfile> for HardwareProfileDto {
    fn from(p: HardwareProfile) -> Self {
        Self {
            id: p.id,
            name: p.name,
            description: p.description,
            context: p.context,
            cache_ram: p.cache_ram,
            extra_flags: p.extra_flags,
        }
    }
}

/// Perfiles expuestos a la UI con datos completos desde el TOML.
pub fn get_hardware_profiles(cfg: &AppConfig) -> Vec<HardwareProfileDto> {
    cfg.profiles.iter().cloned().map(Into::into).collect()
}

/// Resolver un perfil por id; fallback al primero disponible. Sin pánicos: con
/// `panic = "abort"` un `expect` acá mataría el proceso entero, y el caso
/// "lista vacía" solo se alcanza si el TOML se editó a mano.
pub fn resolve_profile(profiles: &[HardwareProfile], id: &str) -> HardwareProfile {
    profiles
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .unwrap_or_else(|| profiles.first().cloned().unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Parche de configuración (Fase A4, vuelta a raíces): la validación del
// `POST /api/config` vivía inline en el handler de `server.rs` (~190 líneas).
// Ahora es pura y testeable sin socket: aplica un parche parcial sobre una
// foto de `AppConfig` y devuelve la config resultante o el mensaje de error
// con el mismo texto que el gateway expone en `{"error": ...}`.
// Solo las tres secciones editables; `ubatch`/`device`/`llama_port`/
// `http_port` son de solo lectura y se rechazan.
// ---------------------------------------------------------------------------

/// Aplica un cuerpo de `POST /api/config` sobre `current`.
///
/// `patch` debe ser un objeto con un subconjunto de las claves `engine`,
/// `generation`, `notifications`. Todo lo demás es `Err` con el mensaje que
/// el gateway expone en `{"error": ...}` (mismo texto que servía el handler).
pub fn apply_config_patch(
    current: &AppConfig,
    patch: &serde_json::Value,
) -> Result<AppConfig, String> {
    if !patch.is_object() {
        return Err("cuerpo inválido: se esperaba un objeto".to_string());
    }
    let obj = patch.as_object().cloned().unwrap_or_default();
    for k in obj.keys() {
        if k != "engine"
            && k != "generation"
            && k != "notifications"
            && k != "lan"
            && k != "client"
            && k != "update"
        {
            return Err(format!("clave desconocida: {}", k));
        }
    }
    let bad = |campo: &str| format!("campo inválido: {}", campo);
    let mut next = current.clone();
    // --- engine (parcial; el resto es de solo lectura) ---
    if let Some(eng) = obj.get("engine") {
        let em = match eng.as_object() {
            Some(m) => m,
            None => return Err(bad("engine")),
        };
        for k in em.keys() {
            if k != "idle_timeout_secs" && k != "threads" && k != "priority" && k != "speculation" {
                return Err(format!(
                    "campo inválido: engine.{} (solo lectura o desconocido)",
                    k
                ));
            }
        }
        if let Some(j) = em.get("idle_timeout_secs") {
            match j.as_u64() {
                Some(n) if engine_idle_timeout_ok(n) => next.engine.idle_timeout_secs = n,
                _ => return Err(bad("engine.idle_timeout_secs")),
            }
        }
        if let Some(j) = em.get("threads") {
            if j.is_null() {
                next.engine.threads = None;
            } else if let Some(n) = j.as_u64().and_then(|n| usize::try_from(n).ok()) {
                if !engine_threads_ok(n) {
                    return Err(bad("engine.threads"));
                }
                next.engine.threads = Some(n);
            } else {
                return Err(bad("engine.threads"));
            }
        }
        if let Some(j) = em.get("priority") {
            match j.as_str() {
                Some(s) if engine_priority_ok(s) => next.engine.priority = s.to_string(),
                _ => return Err(bad("engine.priority")),
            }
        }
        if let Some(j) = em.get("speculation") {
            match j.as_object() {
                Some(sm) => {
                    for k in sm.keys() {
                        if k != "enabled" {
                            return Err(format!(
                                "campo inválido: engine.speculation.{} (solo se acepta enabled)",
                                k
                            ));
                        }
                    }
                    match sm.get("enabled") {
                        Some(b) if b.is_boolean() => {
                            let en = b.as_bool().unwrap_or(true);
                            if next.engine.speculation.is_none() {
                                next.engine.speculation = Some(SpeculationConfig::default());
                            }
                            if let Some(s) = next.engine.speculation.as_mut() {
                                s.enabled = en;
                            }
                        }
                        _ => return Err(bad("engine.speculation.enabled")),
                    }
                }
                _ => return Err(bad("engine.speculation")),
            }
        }
    }
    // --- generation (parcial; rangos de este módulo) ---
    if let Some(gen) = obj.get("generation") {
        let gm = match gen.as_object() {
            Some(m) => m,
            None => return Err(bad("generation")),
        };
        for k in gm.keys() {
            if k != "temperature" && k != "top_p" && k != "max_tokens" && k != "seed" {
                return Err(format!("clave desconocida: generation.{}", k));
            }
        }
        if let Some(j) = gm.get("temperature") {
            match j.as_f64() {
                Some(t) if gen_temperature_ok(t) => next.generation.temperature = t,
                _ => return Err(bad("generation.temperature")),
            }
        }
        if let Some(j) = gm.get("top_p") {
            match j.as_f64() {
                Some(p) if gen_top_p_ok(p) => next.generation.top_p = p,
                _ => return Err(bad("generation.top_p")),
            }
        }
        if let Some(j) = gm.get("max_tokens") {
            match j.as_u64().and_then(|n| usize::try_from(n).ok()) {
                Some(m) if gen_max_tokens_ok(m) => next.generation.max_tokens = m,
                _ => return Err(bad("generation.max_tokens")),
            }
        }
        if let Some(j) = gm.get("seed") {
            match j.as_i64() {
                Some(s) if gen_seed_ok(s) => next.generation.seed = s,
                _ => return Err(bad("generation.seed")),
            }
        }
    }
    // --- notifications (parcial; todo booleanos) ---
    if let Some(not) = obj.get("notifications") {
        let nm = match not.as_object() {
            Some(m) => m,
            None => return Err(bad("notifications")),
        };
        for k in nm.keys() {
            if k != "enabled" && k != "on_ready" && k != "on_failure" && k != "on_autostop" {
                return Err(format!("clave desconocida: notifications.{}", k));
            }
        }
        let flag = |key: &str, slot: &mut bool| -> bool {
            match nm.get(key) {
                None => true,
                Some(j) if j.is_boolean() => {
                    *slot = j.as_bool().unwrap_or(*slot);
                    true
                }
                _ => false,
            }
        };
        if !flag("enabled", &mut next.notifications.enabled) {
            return Err(bad("notifications.enabled"));
        }
        if !flag("on_ready", &mut next.notifications.on_ready) {
            return Err(bad("notifications.on_ready"));
        }
        if !flag("on_failure", &mut next.notifications.on_failure) {
            return Err(bad("notifications.on_failure"));
        }
        if !flag("on_autostop", &mut next.notifications.on_autostop) {
            return Err(bad("notifications.on_autostop"));
        }
    }
    // --- lan (Fase C): solo `enabled`. Rige el bind del próximo arranque;
    // el gateway en curso no se re-liga (diseño B11).
    if let Some(lan) = obj.get("lan") {
        let lm = match lan.as_object() {
            Some(m) => m,
            None => return Err(bad("lan")),
        };
        for k in lm.keys() {
            if k != "enabled" {
                return Err(format!("clave desconocida: lan.{}", k));
            }
        }
        match lm.get("enabled") {
            Some(b) if b.is_boolean() => {
                next.lan.enabled = b.as_bool().unwrap_or(false);
            }
            _ => return Err(bad("lan.enabled")),
        }
    }
    // --- client (Fase C): solo `enabled`. Toma efecto al instante: el
    // dispatch de `/api/start` y `/v1/*` lee la config viva por request.
    // El `[remote]` NO entra por acá (tiene su ruta con manejo de clave).
    if let Some(cli) = obj.get("client") {
        let cm = match cli.as_object() {
            Some(m) => m,
            None => return Err(bad("client")),
        };
        for k in cm.keys() {
            if k != "enabled" {
                return Err(format!("clave desconocida: client.{}", k));
            }
        }
        match cm.get("enabled") {
            Some(b) if b.is_boolean() => {
                next.client.enabled = b.as_bool().unwrap_or(false);
            }
            _ => return Err(bad("client.enabled")),
        }
    }
    // --- update (Fase Prod): `feed` (repo owner/name o vacío = defecto),
    // `check_on_startup` y `auto_download` (booleanos). Efecto al próximo
    // chequeo; el scheduler lee la config viva.
    if let Some(up) = obj.get("update") {
        let um = match up.as_object() {
            Some(m) => m,
            None => return Err(bad("update")),
        };
        for k in um.keys() {
            if k != "feed" && k != "check_on_startup" && k != "auto_download" {
                return Err(format!("clave desconocida: update.{}", k));
            }
        }
        if let Some(j) = um.get("feed") {
            match j.as_str() {
                Some(s) if s.len() <= 120 && (s.is_empty() || valid_update_feed(s)) => {
                    next.update.feed = s.trim().to_string();
                }
                _ => return Err(bad("update.feed")),
            }
        }
        if let Some(j) = um.get("check_on_startup") {
            match j.as_bool() {
                Some(b) => next.update.check_on_startup = b,
                None => return Err(bad("update.check_on_startup")),
            }
        }
        if let Some(j) = um.get("auto_download") {
            match j.as_bool() {
                Some(b) => next.update.auto_download = b,
                None => return Err(bad("update.auto_download")),
            }
        }
    }
    Ok(next)
}

// ---------------------------------------------------------------------------
// URL remota del modo Cliente (Fase B1, diseño B6): pura y testeable.
// Sin cablear todavía: ningún handler la llama en esta ronda.
// ---------------------------------------------------------------------------

/// ¿URL remota aceptable para el modo Cliente?
///
/// - Esquema `http`/`https` (insensible a mayúsculas), sin credenciales (`@`).
/// - Host IP: loopback, RFC1918 o CGNAT (red del dueño, misma regla que el peer).
/// - `localhost`: ambos esquemas.
/// - Dominio no-IP: solo `https` con punto (dominio del túnel, cuyo operador
///   termina TLS). Dominios en `http` plano se rechazan: el DNS puede apuntar
///   a cualquier lado y el gateway no valida nada más en v1.
pub fn remote_url_ok(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    let (rest, secure) = if let Some(r) = lower.strip_prefix("http://") {
        (r, false)
    } else if let Some(r) = lower.strip_prefix("https://") {
        (r, true)
    } else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    // Autoridad hasta el primer `/`, `?` o `#`.
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let auth = &rest[..auth_end];
    if auth.is_empty() || auth.contains('@') {
        return false;
    }
    // Host sin puerto (IPv6 entre corchetes; sin corchetes se rechaza).
    let host = if let Some(s) = auth.strip_prefix('[') {
        let end = match s.find(']') {
            Some(i) => i,
            None => return false,
        };
        let after = &s[end + 1..];
        if !after.is_empty() {
            let p = match after.strip_prefix(':') {
                Some(p) => p,
                None => return false,
            };
            if p.is_empty() || p.len() > 5 || !p.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
        }
        &s[..end]
    } else {
        let mut parts = auth.split(':');
        let h = parts.next().unwrap_or("");
        match parts.next() {
            None => h,
            Some(p) => {
                if parts.next().is_some() {
                    return false;
                }
                if p.is_empty() || p.len() > 5 || !p.chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                h
            }
        }
    };
    if host.is_empty() {
        return false;
    }
    if host == "localhost" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback() || crate::meta::ip_red_privada(ip),
        Err(_) => secure && host.contains('.') && !host.contains([' ', '/', '\\']),
    }
}

/// ¿Base de la API de actualización aceptable (`OMNI_UPDATE_API`)?
/// Sec producción: exige `https://` + host con punto y rechaza `http://`
/// y credenciales (`@`). Modelo: `remote_url_ok` (dominio no-IP solo por TLS).
/// Sin esto, un env local redirige el chequeo/descarga del update a un
/// `http://` interno (MITM en la LAN: el binario sustituto llegaría igual,
/// solo lo frenaría el checksum del body). `None`/vacío = defecto seguro
/// (`https://api.github.com`); el llamador lo aplica antes de usar.
pub fn update_api_base_ok(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https://") else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    // Autoridad hasta el primer `/`, `?` o `#` (igual que `remote_url_ok`).
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let auth = &rest[..auth_end];
    if auth.is_empty() || auth.contains('@') {
        return false;
    }
    // Host sin puerto (IPv6 entre corchetes; sin corchetes se rechaza).
    let host = if let Some(s) = auth.strip_prefix('[') {
        let end = match s.find(']') {
            Some(i) => i,
            None => return false,
        };
        let after = &s[end + 1..];
        if !after.is_empty() {
            let p = match after.strip_prefix(':') {
                Some(p) => p,
                None => return false,
            };
            if p.is_empty() || p.len() > 5 || !p.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
        }
        &s[..end]
    } else {
        let mut parts = auth.split(':');
        let h = parts.next().unwrap_or("");
        match parts.next() {
            None => h,
            Some(p) => {
                if parts.next().is_some() {
                    return false;
                }
                if p.is_empty() || p.len() > 5 || !p.chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                h
            }
        }
    };
    // Host con punto (dominio/API real); sin punto sería un nombre LAN al que
    // un env local redirigiría el update. Sin espacios ni barras invertidas.
    !host.is_empty() && host.contains('.') && !host.contains([' ', '/', '\\'])
}

/// ¿Feed de actualización aceptable? `owner/name` GitHub (vacío = defecto).
/// Misma gramática estricta que `models::check_repo`: sin `..`, sin espacios,
/// sin backslash; cada lado `owner`/`name` no vacío, charset acotado.
pub fn valid_update_feed(feed: &str) -> bool {
    let r = feed.trim();
    if r.is_empty() {
        return false;
    }
    let mut parts = r.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(n), None) => {
            !o.is_empty()
                && !n.is_empty()
                && !r.contains("..")
                && !r.contains(' ')
                && !r.contains('\\')
                && r.chars().all(|c| {
                    c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_' || c == '.'
                })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validador_id_perfil() {
        assert!(profile_id_ok("perfil_1"));
        assert!(profile_id_ok("a-b-c_123"));
        assert!(profile_id_ok("velocidad"));
        assert!(profile_id_ok("x"));
        // Longitud máxima 32 chars
        let exact32 = "a".repeat(32);
        assert!(profile_id_ok(&exact32));
        let too_long = "a".repeat(33);
        assert!(!profile_id_ok(&too_long));
        // Caracteres inválidos
        assert!(!profile_id_ok(""));
        assert!(!profile_id_ok("Perfil")); // mayúsculas
        assert!(!profile_id_ok("perfil con espacio"));
        assert!(!profile_id_ok("perfil/sub"));
        assert!(!profile_id_ok("perfil.punto"));
        assert!(!profile_id_ok("perfil;cmd"));
    }

    #[test]
    fn validador_contexto_y_cache_ram() {
        assert!(profile_context_ok(1024));
        assert!(profile_context_ok(32768));
        assert!(profile_context_ok(1048576));
        assert!(!profile_context_ok(0));
        assert!(!profile_context_ok(1023));
        assert!(!profile_context_ok(1025)); // no es múltiplo de 1024
        assert!(!profile_context_ok(1048576 + 1024)); // fuera de rango

        assert!(profile_cache_ram_ok(0));
        assert!(profile_cache_ram_ok(4096));
        assert!(profile_cache_ram_ok(65536));
        assert!(!profile_cache_ram_ok(65537));
    }

    #[test]
    fn validador_extra_flags() {
        let ok_flags = vec![
            "-kvu".to_string(),
            "--flash-attn".to_string(),
            "foo=bar:1/2".to_string(),
        ];
        assert!(profile_flags_ok(&ok_flags));
        assert!(profile_flags_ok(&[]));
        assert!(!profile_flags_ok(&["-ctk".to_string(), "f16".to_string()]));
        assert!(!profile_flags_ok(&["-ctk f16".to_string()]));
        assert!(!profile_flags_ok(&["--cache-type-v=q8_0".to_string()]));
        assert!(profile_flags_ok(&["-kvu".to_string()]));
        // Máximo 8 flags
        let nine = (0..9).map(|i| format!("-f{}", i)).collect::<Vec<_>>();
        assert!(!profile_flags_ok(&nine));
        // Flag vacía o >64 chars
        assert!(!profile_flags_ok(&["".to_string()]));
        let long_flag = "a".repeat(65);
        assert!(!profile_flags_ok(&[long_flag]));
        // Caracteres prohibidos (inyección cmd / shell)
        assert!(!profile_flags_ok(&["-flag & calc.exe".to_string()]));
        assert!(!profile_flags_ok(&["-flag; reboot".to_string()]));
        assert!(!profile_flags_ok(&["-flag | echo".to_string()]));
        assert!(!profile_flags_ok(&["-flag\"quote".to_string()]));
    }

    #[test]
    fn validador_motor_y_generacion() {
        assert!(engine_idle_timeout_ok(0));
        assert!(engine_idle_timeout_ok(60));
        assert!(engine_idle_timeout_ok(1500));
        assert!(engine_idle_timeout_ok(604800));
        assert!(!engine_idle_timeout_ok(59)); // <60 excepto 0
        assert!(!engine_idle_timeout_ok(604801));

        assert!(engine_threads_ok(1));
        assert!(engine_threads_ok(6));
        assert!(engine_threads_ok(256));
        assert!(!engine_threads_ok(0));
        assert!(!engine_threads_ok(257));

        assert!(engine_priority_ok("2"));
        assert!(engine_priority_ok("0"));
        assert!(!engine_priority_ok("1"));
        assert!(!engine_priority_ok("high"));

        assert!(gen_temperature_ok(0.0));
        assert!(gen_temperature_ok(0.7));
        assert!(gen_temperature_ok(2.0));
        assert!(!gen_temperature_ok(-0.1));
        assert!(!gen_temperature_ok(2.1));
        assert!(!gen_temperature_ok(f64::NAN));

        assert!(gen_top_p_ok(0.0));
        assert!(gen_top_p_ok(1.0));
        assert!(!gen_top_p_ok(1.01));

        assert!(gen_max_tokens_ok(1));
        assert!(gen_max_tokens_ok(32768));
        assert!(!gen_max_tokens_ok(0));
        assert!(!gen_max_tokens_ok(32769));

        assert!(gen_seed_ok(0));
        assert!(gen_seed_ok(42));
        assert!(!gen_seed_ok(-1));
    }

    #[test]
    fn psu_candado_rechaza_ub_batch_spec() {
        // LM-NF-3: `-ub 1024` en un perfil se rechaza nombrando la flag.
        let f = |ss: &[&str]| ss.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(psu_unsafe_flag(&f(&["-ub", "1024"])).is_some());
        assert!(psu_unsafe_flag(&f(&["--ubatch-size=2048"])).is_some());
        assert!(psu_unsafe_flag(&f(&["-b", "2048"])).is_some());
        assert!(psu_unsafe_flag(&f(&["--spec-type", "draft-mtp"])).is_some());
        assert!(psu_unsafe_flag(&f(&["--spec-draft-n-max", "3"])).is_some());
        // P1-IO: flags de escritura arbitraria en disco rechazadas.
        assert!(psu_unsafe_flag(&f(&["--log-file", "C:/tmp/hack.log"])).is_some());
        assert!(psu_unsafe_flag(&f(&["--log-file=rel/log.txt"])).is_some());
        assert!(psu_unsafe_flag(&f(&["--out-file", "dump.bin"])).is_some());
        assert!(psu_unsafe_flag(&f(&["--out-file=out.txt"])).is_some());
        let err_log = psu_unsafe_flag(&f(&["--log-file", "out.log"])).unwrap();
        assert!(err_log.contains("seguridad de escritura"), "{}", err_log);
        assert!(psu_unsafe_flag(&f(&["-ub", "512"])).is_none());
        assert!(psu_unsafe_flag(&f(&["-b", "1024"])).is_none());
        assert!(psu_unsafe_flag(&f(&["--load-mode", "none"])).is_none());
        assert!(psu_unsafe_flag(&f(&["-kvu"])).is_none());
        assert!(psu_unsafe_flag(&[]).is_none());
    }

    // ---- Fase A4: `apply_config_patch` (misma forma que el handler servía) ----

    fn cfg_base() -> AppConfig {
        AppConfig::default()
    }

    fn patch(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn config_patch_rechaza_cuerpo_y_claves_desconocidas() {
        let base = cfg_base();
        // No-objeto.
        let err = apply_config_patch(&base, &patch(r#"[1,2]"#)).unwrap_err();
        assert!(err.contains("cuerpo inválido"), "{}", err);
        // Clave superior desconocida.
        let err = apply_config_patch(&base, &patch(r#"{"motor":{}}"#)).unwrap_err();
        assert!(err.contains("clave desconocida: motor"), "{}", err);
        // Campo de solo lectura en engine.
        let err = apply_config_patch(&base, &patch(r#"{"engine":{"device":"x"}}"#)).unwrap_err();
        assert!(
            err.contains("engine.device") && err.contains("solo lectura"),
            "{}",
            err
        );
        // Clave desconocida en generation y notifications.
        let err = apply_config_patch(&base, &patch(r#"{"generation":{"top_k":5}}"#)).unwrap_err();
        assert!(err.contains("generation.top_k"), "{}", err);
        let err =
            apply_config_patch(&base, &patch(r#"{"notifications":{"sms":true}}"#)).unwrap_err();
        assert!(err.contains("notifications.sms"), "{}", err);
    }

    #[test]
    fn config_patch_aplica_parcial_y_respeta_rangos() {
        let base = cfg_base();
        // Parche parcial válido: toca lo pedido, deja el resto.
        let next = apply_config_patch(
            &base,
            &patch(r#"{"engine":{"threads":4},"generation":{"temperature":0.5}}"#),
        )
        .unwrap();
        assert_eq!(next.engine.threads, Some(4));
        assert_eq!(next.generation.temperature, 0.5);
        assert_eq!(next.generation.top_p, base.generation.top_p);
        // Fuera de rango se rechaza con el campo culpable.
        let err = apply_config_patch(&base, &patch(r#"{"engine":{"threads":0}}"#)).unwrap_err();
        assert!(err.contains("engine.threads"), "{}", err);
        let err =
            apply_config_patch(&base, &patch(r#"{"generation":{"temperature":9.0}}"#)).unwrap_err();
        assert!(err.contains("generation.temperature"), "{}", err);
        let err =
            apply_config_patch(&base, &patch(r#"{"notifications":{"enabled":"si"}}"#)).unwrap_err();
        assert!(err.contains("notifications.enabled"), "{}", err);
        // `threads: null` limpia el override (igual que el handler).
        let next = apply_config_patch(&base, &patch(r#"{"engine":{"threads":null}}"#)).unwrap();
        assert_eq!(next.engine.threads, None);
    }

    #[test]
    fn config_patch_speculation_solo_enabled_booleano() {
        let base = cfg_base();
        let next = apply_config_patch(
            &base,
            &patch(r#"{"engine":{"speculation":{"enabled":false}}}"#),
        )
        .unwrap();
        assert_eq!(
            next.engine.speculation.as_ref().map(|s| s.enabled),
            Some(false)
        );
        let err = apply_config_patch(
            &base,
            &patch(r#"{"engine":{"speculation":{"enabled":true,"n":8}}}"#),
        )
        .unwrap_err();
        assert!(
            err.contains("engine.speculation.n") && err.contains("solo se acepta enabled"),
            "{}",
            err
        );
        let err = apply_config_patch(
            &base,
            &patch(r#"{"engine":{"speculation":{"enabled":"si"}}}"#),
        )
        .unwrap_err();
        assert!(err.contains("engine.speculation.enabled"), "{}", err);
    }

    // ---- Fase B1: URL remota del modo Cliente ----

    #[test]
    fn remote_url_acepta_red_del_dueno() {
        // IP privada / loopback / CGNAT, con o sin puerto y path.
        for u in [
            "http://192.168.1.10:17860",
            "http://192.168.1.10:17860/v1",
            "http://10.0.0.2/v1",
            "http://172.16.5.4:8080",
            "http://127.0.0.1:8080",
            "http://localhost:17860",
            "https://localhost:17860",
            "http://100.64.0.5:17860",
            "http://[::1]:17860",
        ] {
            assert!(remote_url_ok(u), "{}", u);
        }
        // Dominio del túnel: solo https.
        assert!(remote_url_ok("https://algo.trycloudflare.com"));
        assert!(remote_url_ok("https://mi-casa.tailnet.ts.net:8443/v1"));
    }

    #[test]
    fn remote_url_rechaza_resto() {
        for u in [
            "",
            "no-url",
            "ftp://192.168.1.10/",
            "http://",
            "http:///v1",
            "http://8.8.8.8/",
            "http://1.1.1.1:17860",
            "http://172.32.0.1/",
            "http://100.128.0.1/",
            "http://example.com/",
            "http://user:clave@192.168.1.10/",
            "http://user@192.168.1.10/",
            "http://192.168.1.10:puert/",
            "http://::1:17860",
            "http://[::1:17860",
            "localhost:17860",
        ] {
            assert!(!remote_url_ok(u), "{}", u);
        }
    }

    /// Sec producción: la base de la API de update solo acepta `https://` +
    /// host con punto (modelo `remote_url_ok` para dominios) y rechaza
    /// `http://` y credenciales (`@`). Sin esto un env local redirige el
    /// update a http interno.
    #[test]
    fn update_api_base_solo_https_con_punto() {
        for u in [
            "https://api.github.com",
            "https://api.github.com/",
            "https://ghe.mi-empresa.com/api/v3",
            "https://api.github.com:443/repos",
        ] {
            assert!(update_api_base_ok(u), "{}", u);
        }
        for u in [
            "",
            "http://api.github.com",
            "http://192.168.1.10:17860",
            "http://localhost:17860",
            "https://localhost",
            "https://intranet",
            "https://user:clave@api.github.com",
            "https://user@api.github.com/",
            "ftp://api.github.com",
            "api.github.com",
            "https://",
            "https:///repos",
        ] {
            assert!(!update_api_base_ok(u), "{}", u);
        }
    }

    // ---- Fase C: secciones lan/client del PATCH ----

    #[test]
    fn config_patch_lan_y_client_solo_enabled() {
        let base = cfg_base();
        assert!(!base.lan.enabled);
        assert!(!base.client.enabled);
        let next = apply_config_patch(&base, &patch(r#"{"lan":{"enabled":true}}"#)).unwrap();
        assert!(next.lan.enabled);
        assert!(!next.client.enabled);
        let next = apply_config_patch(&base, &patch(r#"{"client":{"enabled":true}}"#)).unwrap();
        assert!(next.client.enabled);
        assert!(!next.lan.enabled);
        // Sub-clave desconocida y tipo erróneo se rechazan con nombre.
        let err =
            apply_config_patch(&base, &patch(r#"{"lan":{"enabled":true,"port":1}}"#)).unwrap_err();
        assert!(err.contains("lan.port"), "{}", err);
        let err = apply_config_patch(&base, &patch(r#"{"client":{"enabled":"si"}}"#)).unwrap_err();
        assert!(err.contains("client.enabled"), "{}", err);
        let err = apply_config_patch(&base, &patch(r#"{"remote":{"url":"x"}}"#)).unwrap_err();
        assert!(err.contains("clave desconocida: remote"), "{}", err);
    }

    fn mig_profile(id: &str, ctx: usize, ram: usize, flags: &[&str]) -> HardwareProfile {
        HardwareProfile {
            id: id.to_string(),
            name: String::new(),
            description: String::new(),
            context: ctx,
            cache_ram: ram,
            extra_flags: flags.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn mig_untouched_stays_empty_and_version() {
        // v0/v2/v3 intacto (`[]` o canónico previo) → v4 neto `[]`.
        // La versión avanza igual (el brazo corre y sella v4).
        for flags in [&[] as &[&str], &["--no-mmap"], &["--load-mode", "none"]] {
            let mut stored = vec![mig_profile("libros", 131072, 6144, flags)];
            let from = if flags.contains(&"--no-mmap") { 1 } else { 0 };
            let touched = migrate_builtin_profiles_from(&mut stored, from);
            assert_eq!(
                stored[0].extra_flags,
                Vec::<String>::new(),
                "flags={:?}",
                flags
            );
            let _ = touched;
        }
        assert_eq!(PROFILES_VERSION, 7);
    }

    #[test]
    fn mig_v1_nommap_reverted_to_empty() {
        // v1 (`--no-mmap` intacto) → v4 `[]` (misma regla segura).
        let mut stored = vec![mig_profile("libros", 131072, 6144, &["--no-mmap"])];
        let touched = migrate_builtin_profiles_from(&mut stored, 1);
        assert_eq!(touched, vec!["libros".to_string()]);
        assert_eq!(stored[0].extra_flags, Vec::<String>::new());
    }

    #[test]
    fn mig_v3_canonical_reverted_to_empty() {
        // v3 (`--load-mode none` intacto) → v4 `[]` (seguridad: arranques baratos).
        let mut stored = vec![mig_profile(
            "libros",
            131072,
            6144,
            &["--load-mode", "none"],
        )];
        let touched = migrate_builtin_profiles_from(&mut stored, 3);
        assert_eq!(touched, vec!["libros".to_string()]);
        assert_eq!(stored[0].extra_flags, Vec::<String>::new());
    }

    #[test]
    fn mig_v5_desactiva_cache_de_prompts_sin_tocar_valores_custom() {
        let mut stored = vec![
            mig_profile("multi_doc", 65536, 4096, &[]),
            mig_profile("libros", 131072, 6144, &[]),
            mig_profile("max_contexto", 262144, 8192, &["-kvu"]),
        ];
        let touched = migrate_builtin_profiles_from(&mut stored, 5);
        assert_eq!(touched, vec!["multi_doc", "libros"]);
        assert_eq!(stored[0].cache_ram, 0);
        assert_eq!(stored[1].cache_ram, 0);
        assert_eq!(stored[2].cache_ram, 8192, "el límite custom se conserva");
    }

    #[test]
    fn mig_v6_refresca_presentacion_heredada_y_respeta_personalizaciones() {
        let mut stored = vec![
            mig_profile("velocidad", 32768, 0, &[]),
            mig_profile("multi_doc", 65536, 0, &[]),
            mig_profile("libros", 131072, 0, &[]),
        ];
        stored[0].name = "Recomendado · 32K (el más rápido)".to_string();
        stored[0].description = "Todo el modelo y su memoria de conversación (KV cache) viven íntegramente en la VRAM de la GPU. Es el modo más rápido con mínima latencia. Ideal para chat, preguntas y código corto (<30k palabras).".to_string();
        stored[1].name = "Multi-documento · 64K (velocidad levemente menor)".to_string();
        stored[1].description = "El modelo vive en VRAM y parte del KV cache pasa a RAM DDR5. Renuncia ~5% de velocidad para sostener ~60k palabras de contexto: varios documentos o un repo de código mediano en la misma conversación.".to_string();
        stored[2].name = "Mi perfil".to_string();
        stored[2].description = "KV adaptado manualmente a mi equipo".to_string();

        let touched = migrate_builtin_profiles_from(&mut stored, 6);
        let current = built_in_profiles();
        assert_eq!(touched, vec!["velocidad", "multi_doc"]);
        assert_eq!(stored[0].name, current[0].name);
        assert_eq!(stored[0].description, current[0].description);
        assert_eq!(stored[1].name, current[1].name);
        assert_eq!(stored[1].description, current[1].description);
        assert_eq!(stored[2].name, "Mi perfil");
        assert_eq!(stored[2].description, "KV adaptado manualmente a mi equipo");
    }

    #[test]
    fn mig_v6_actualiza_libros_con_defaults_y_textos_publicados() {
        let current = built_in_profiles();
        let (old_names, old_descriptions) = previous_built_in_presentation("libros").unwrap();
        let mut stored = vec![mig_profile("libros", 131072, 0, &[])];
        stored[0].name = old_names[0].to_string();
        stored[0].description = old_descriptions[0].to_string();

        let touched = migrate_builtin_profiles_from(&mut stored, 6);

        assert_eq!(touched, vec!["libros"]);
        assert_eq!(stored[0].name, current[2].name);
        assert_eq!(stored[0].description, current[2].description);
        assert_eq!(stored[0].cache_ram, 0);
    }

    #[test]
    fn mig_config_actualiza_textos_antiguos_y_escribe_una_copia() {
        let dir = std::env::temp_dir().join(format!("lm-mig-presentacion-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localmind.toml");
        let old = r#"profiles_version = 5

[[profiles]]
id = "velocidad"
name = "Velocidad máxima · 32K contextos cortos"
description = "Todo el modelo y su memoria de conversación (KV cache) viven en los 16 GB de la GPU. Es el modo más rápido: ~17 t/s. Ideal para chat, preguntas y código corto (sesiones de menos de ~30k palabras)."
context = 32768
cache_ram = 0
extra_flags = []
"#;
        std::fs::write(&path, old).unwrap();
        let store = ConfigStore::load_from_path(&path);
        let migrated = store.get();
        let expected = built_in_profiles().remove(0);
        assert_eq!(migrated.profiles_version, PROFILES_VERSION);
        assert_eq!(migrated.profiles[0].name, expected.name);
        assert_eq!(migrated.profiles[0].description, expected.description);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("profiles_version = 7"));
        assert!(!written.contains("16 GB de la GPU"));
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            old
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mig_user_flag_keeps_profile() {
        // La flag del usuario se conserva aunque migre el límite por defecto.
        let mut stored = vec![mig_profile("libros", 131072, 6144, &["--mi-flag"])];
        let touched = migrate_builtin_profiles(&mut stored);
        assert_eq!(touched, vec!["libros".to_string()]);
        assert_eq!(stored[0].extra_flags, vec!["--mi-flag".to_string()]);
        assert_eq!(stored[0].cache_ram, 0);
    }

    #[test]
    fn mig_user_context_left_alone() {
        // (c) Contexto customizado → intacto; flags sí (eran foto previa).
        let mut stored = vec![mig_profile("libros", 65536, 6144, &["--load-mode", "none"])];
        let touched = migrate_builtin_profiles_from(&mut stored, 3);
        assert_eq!(touched, vec!["libros".to_string()]);
        assert_eq!(stored[0].context, 65536);
        assert_eq!(stored[0].extra_flags, Vec::<String>::new());
    }

    #[test]
    fn mig_unknown_id_untouched() {
        // (d) Id desconocido → intacto.
        let mut stored = vec![mig_profile("mio", 999, 1, &[])];
        let touched = migrate_builtin_profiles(&mut stored);
        assert!(touched.is_empty());
        assert_eq!(stored[0].context, 999);
    }

    #[test]
    fn mig_current_version_writes_nothing() {
        // (e) Versión al día → `load` no reescribe (regla: solo si < actual).
        // Se prueba la condición, no el FS: un cfg ya en v1 no entra al brazo.
        let cfg = AppConfig::default();
        assert_eq!(cfg.profiles_version, PROFILES_VERSION);
        assert!(cfg.profiles_version < PROFILES_VERSION + 1);
        assert!(!(cfg.profiles_version < PROFILES_VERSION));
    }

    #[test]
    fn mig_proof_scratch_appdata_old_toml() {
        // TOML v0: v4 neto `[]`, versión avanza, customs intactos.
        let dir = std::env::temp_dir().join(format!("lm-mig-proof-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("LocalMind")).unwrap();
        let toml = "[engine]\n\n\
            [[profiles]]\nid = \"velocidad\"\nname = \"V\"\ndescription = \"d\"\ncontext = 32768\ncache_ram = 0\nextra_flags = []\n\n\
            [[profiles]]\nid = \"libros\"\nname = \"L\"\ndescription = \"d\"\ncontext = 131072\ncache_ram = 6144\nextra_flags = []\n\n\
            [[profiles]]\nid = \"multi_doc\"\nname = \"M\"\ndescription = \"d\"\ncontext = 65536\ncache_ram = 4096\nextra_flags = [\"--mi-flag\"]\n\n\
            [[profiles]]\nid = \"mio\"\nname = \"X\"\ndescription = \"d\"\ncontext = 999\ncache_ram = 1\nextra_flags = []\n\n\
            [last]\nprofile = \"libros\"\n";
        std::fs::write(dir.join("LocalMind").join("localmind.toml"), toml).unwrap();
        // Sin env global (`set_var` es data race entre tests paralelos):
        // path explícito vía `load_from_path`.
        let store = ConfigStore::load_from_path(&dir.join("LocalMind").join("localmind.toml"));
        let cfg = store.get();
        let libros = cfg.profiles.iter().find(|p| p.id == "libros").unwrap();
        assert_eq!(libros.extra_flags, Vec::<String>::new());
        let md = cfg.profiles.iter().find(|p| p.id == "multi_doc").unwrap();
        assert_eq!(md.extra_flags, vec!["--mi-flag".to_string()]);
        let mio = cfg.profiles.iter().find(|p| p.id == "mio").unwrap();
        assert_eq!(mio.context, 999);
        assert_eq!(cfg.profiles_version, PROFILES_VERSION);
        let back = std::fs::read_to_string(dir.join("LocalMind").join("localmind.toml")).unwrap();
        assert!(back.contains("profiles_version = 7"));
        assert!(!back.contains("--load-mode"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mig_proof_scratch_appdata_v1_toml_reverts() {
        // TOML v1 (`--no-mmap`): v4 lo deja en `[]`.
        let dir = std::env::temp_dir().join(format!("lm-mig-proof-v1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("LocalMind")).unwrap();
        let toml = "profiles_version = 1\n\n[engine]\n\n\
            [[profiles]]\nid = \"libros\"\nname = \"L\"\ndescription = \"d\"\ncontext = 131072\ncache_ram = 6144\nextra_flags = [\"--no-mmap\"]\n\n\
            [last]\nprofile = \"libros\"\n";
        std::fs::write(dir.join("LocalMind").join("localmind.toml"), toml).unwrap();
        let store = ConfigStore::load_from_path(&dir.join("LocalMind").join("localmind.toml"));
        let cfg = store.get();
        let libros = cfg.profiles.iter().find(|p| p.id == "libros").unwrap();
        assert_eq!(libros.extra_flags, Vec::<String>::new());
        assert_eq!(cfg.profiles_version, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mig_proof_scratch_appdata_v3_toml_reverts() {
        // TOML v3 (`--load-mode none` intacto): v4 lo deja en `[]` (seguridad).
        let dir = std::env::temp_dir().join(format!("lm-mig-proof-v3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("LocalMind")).unwrap();
        let toml = "profiles_version = 3\n\n[engine]\n\n\
            [[profiles]]\nid = \"libros\"\nname = \"L\"\ndescription = \"d\"\ncontext = 131072\ncache_ram = 6144\nextra_flags = [\"--load-mode\", \"none\"]\n\n\
            [last]\nprofile = \"libros\"\n";
        std::fs::write(dir.join("LocalMind").join("localmind.toml"), toml).unwrap();
        let store = ConfigStore::load_from_path(&dir.join("LocalMind").join("localmind.toml"));
        let cfg = store.get();
        let libros = cfg.profiles.iter().find(|p| p.id == "libros").unwrap();
        assert_eq!(libros.extra_flags, Vec::<String>::new());
        assert_eq!(cfg.profiles_version, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn eng_cfg(idle: u64, cd: u64, cap: u32, ps: bool, slow: f64) -> AppConfig {
        let mut c = AppConfig::default();
        c.engine.idle_timeout_secs = idle;
        c.engine.start_cooldown_secs = cd;
        c.engine.max_starts_per_hour = cap;
        c.engine.power_safe = ps;
        c.engine.slow_gate_tps = slow;
        c
    }

    #[test]
    fn mig_engine_old_defaults_refreshed() {
        // TOML viejo (1500 + sin campos nuevos relevantes): todo a v5.
        let mut cfg = eng_cfg(1500, 0, 0, false, 0.0);
        // Simular "ausente": los None de las fotos v0 cubren 1500; para los
        // campos nuevos el valor default-actual NO coincide con foto → se
        // usa la variante ausente: forzar pasando versión 0 con valores que
        // solo existen en fotos como None requiere el path de `load`, así que
        // aquí se prueba la parte con foto (idle) + defaults nuevos directos.
        let touched = migrate_engine_defaults(&mut cfg, 0);
        assert!(touched.contains(&"idle_timeout_secs".to_string()));
        assert_eq!(cfg.engine.idle_timeout_secs, 5400);
    }

    #[test]
    fn mig_engine_custom_idle_left_alone() {
        // 900 deliberado no coincide con ninguna foto → intacto.
        let mut cfg = eng_cfg(900, 120, 4, true, 20.0);
        let touched = migrate_engine_defaults(&mut cfg, 0);
        assert!(!touched.contains(&"idle_timeout_secs".to_string()));
        assert_eq!(cfg.engine.idle_timeout_secs, 900);
    }

    #[test]
    fn mig_engine_v4_cooldown_cap_upgraded() {
        // v4 traía 90/3: la v5 los lleva a 120/4; slow ausente → 20.0.
        let mut cfg = eng_cfg(1500, 90, 3, true, 0.0);
        // slow 0.0 no está en fotos (None) → necesita el brazo ausente.
        let touched = migrate_engine_defaults(&mut cfg, 4);
        assert!(touched.contains(&"start_cooldown_secs".to_string()));
        assert!(touched.contains(&"max_starts_per_hour".to_string()));
        assert_eq!(cfg.engine.start_cooldown_secs, 120);
        assert_eq!(cfg.engine.max_starts_per_hour, 4);
    }

    #[test]
    fn mig_proof_engine_old_and_custom_toml() {
        // Prueba de carga real: TOML viejo (1500) migra a 5400 + campos
        // nuevos; TOML con 900 deliberado queda intacto. Sin env global.
        for (tag, idle, expect_idle) in [("old", 1500u64, 5400u64), ("cust", 900u64, 900u64)] {
            let dir =
                std::env::temp_dir().join(format!("lm-eng-proof-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("LocalMind")).unwrap();
            let toml = format!(
                "[engine]\nidle_timeout_secs = {}\nthreads = 6\nbatch = 1024\n\n\
                [[profiles]]\nid = \"libros\"\nname = \"L\"\ndescription = \"d\"\ncontext = 131072\ncache_ram = 6144\nextra_flags = []\n\n\
                [last]\nprofile = \"libros\"\n",
                idle
            );
            std::fs::write(dir.join("LocalMind").join("localmind.toml"), &toml).unwrap();
            let store = ConfigStore::load_from_path(&dir.join("LocalMind").join("localmind.toml"));
            let cfg = store.get();
            assert_eq!(cfg.engine.idle_timeout_secs, expect_idle, "tag={}", tag);
            assert_eq!(cfg.engine.threads, Some(6), "tag={}", tag);
            assert_eq!(cfg.engine.batch, 1024, "tag={}", tag);
            assert_eq!(cfg.engine.start_cooldown_secs, 120, "tag={}", tag);
            assert_eq!(cfg.engine.max_starts_per_hour, 4, "tag={}", tag);
            assert!(cfg.engine.power_safe, "tag={}", tag);
            assert_eq!(cfg.engine.slow_gate_tps, 20.0, "tag={}", tag);
            assert_eq!(cfg.profiles_version, PROFILES_VERSION, "tag={}", tag);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn config_toml_ilegible_deja_nota() {
        // Un TOML corrupto es indistinguible de uno ausente si se traga el
        // error: ambos dan defaults. La diferencia que importa es que el
        // corrupto avise (si no, el próximo `save()` lo pisa en silencio).
        let dir = std::env::temp_dir().join(format!("lm-bad-toml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localmind.toml");
        let roto = "esto no [es TOML = \"valido\"\n[[[";
        std::fs::write(&path, roto).unwrap();
        let store = ConfigStore::load_from_path(&path);
        let note = store
            .take_migration_note()
            .expect("un TOML ilegible debe dejar nota");
        assert!(note.contains(&path.display().to_string()), "{}", note);
        // Carga: defaults en memoria, archivo corrupto intacto en disco.
        assert_eq!(store.get().profiles_version, PROFILES_VERSION);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), roto);
        // P2 producción: el TOML ilegible deja `.bak` con el contenido roto
        // (antes solo la migración v5 lo escribía) y la nota lo nombra.
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            roto
        );
        assert!(note.contains(".bak") || note.contains("copia"), "{}", note);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_toml_ausente_no_deja_nota() {
        // Primer arranque: ausencia de archivo es normal y debe ser silencioso.
        let dir = std::env::temp_dir().join(format!("lm-no-toml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = ConfigStore::load_from_path(&dir.join("localmind.toml"));
        assert!(store.take_migration_note().is_none());
        // Defaults completos (perfiles repuestos, D-1).
        assert!(!store.get().profiles.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lan_apagada_por_defecto_y_toml_viejo_sin_seccion() {
        // Fase B2: sin `[lan]` (TOML de cualquier versión anterior) → false,
        // o sea comportamiento histórico. Con la sección → respeta el valor.
        assert!(!AppConfig::default().lan.enabled);
        let dir = std::env::temp_dir().join(format!("lm-no-lan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localmind.toml");
        std::fs::write(&path, "[engine]\nthreads = 6\n").unwrap();
        let store = ConfigStore::load_from_path(&path);
        assert!(!store.get().lan.enabled);
        std::fs::write(&path, "[engine]\nthreads = 6\n\n[lan]\nenabled = true\n").unwrap();
        let store = ConfigStore::load_from_path(&path);
        assert!(store.get().lan.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cliente_y_remoto_apagados_por_defecto() {
        // Fase B3: TOML viejo sin secciones → Todo-aquí (nada que computar
        // en otro lado, nada a lo que conectarse).
        assert!(!AppConfig::default().client.enabled);
        assert!(AppConfig::default().remote.url.is_empty());
        assert!(AppConfig::default().remote.key.is_empty());
        let dir = std::env::temp_dir().join(format!("lm-no-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localmind.toml");
        std::fs::write(&path, "[engine]\nthreads = 6\n").unwrap();
        let cfg = ConfigStore::load_from_path(&path).get();
        assert!(!cfg.client.enabled);
        assert!(cfg.remote.url.is_empty());
        // …y con secciones → se respetan y sobreviven al save/load.
        std::fs::write(
            &path,
            "[engine]\nthreads = 6\n\n[client]\nenabled = true\n\n[remote]\nurl = \"http://192.168.1.10:17860\"\nkey = \"k\"\n",
        )
        .unwrap();
        let store = ConfigStore::load_from_path(&path);
        assert!(store.get().client.enabled);
        assert_eq!(store.get().remote.url, "http://192.168.1.10:17860");
        store.save().unwrap();
        let recargado = ConfigStore::load_from_path(&path).get();
        assert!(recargado.client.enabled);
        assert_eq!(recargado.remote.key, "k");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
