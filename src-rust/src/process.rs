use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::ConfigStore;
use crate::engine_gate::{
    engine_lost, eta_secs, gate_is_slow, load_key, should_auto_stop, vram_total_mb, vram_used_mb,
};

const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub filename: String,
    pub name: String,
    pub size_gb: f64,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub status: String,
    pub is_healthy: bool,
    pub pid: Option<u32>,
    pub model: String,
    pub context: usize,
    pub profile: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_remaining_secs: Option<u64>,
    pub last_error: Option<String>,
    // Puerta de aceptación (D1) + progreso/ETA (D4). Los Option se serializan
    // siempre (null si ausentes), como `last_error`; `default` solo para compat
    // de Deserialize.
    #[serde(default)]
    pub verifying: bool,
    #[serde(default)]
    pub starting_for_secs: u64,
    #[serde(default)]
    pub eta_secs: u64,
    /// Completion tokens por segundo dividido por el tiempo total de la
    /// solicitud de aceptación. Incluye prefill y no es decode puro.
    #[serde(default)]
    pub request_tps: Option<f64>,
    #[serde(default)]
    pub request_tps_samples: Vec<f64>,
    /// Tasa de decode que informa `timings.predicted_per_second` del motor.
    #[serde(default)]
    pub decode_tps: Option<f64>,
    /// Muestras de decode puro; vacías si el motor no informa timings.
    #[serde(default)]
    pub decode_tps_samples: Vec<f64>,
    /// true si el mínimo de la puerta quedó bajo `[engine] slow_gate_tps`
    /// (default 20). Solo informativo: sin reintentos (LM-NF-3). Mensaje UI:
    /// `El motor cargó lento (N t/s); puede mejorarse reiniciándolo una vez`.
    #[serde(default)]
    pub engine_slow: bool,
    /// false indica que el motor no informó `timings`; no equivale a rápido.
    #[serde(default)]
    pub engine_slow_known: bool,
    #[serde(default)]
    pub acceptance_ok: Option<bool>,
    #[serde(default)]
    pub acceptance_error: Option<String>,
}

impl Default for ServerStatus {
    fn default() -> Self {
        Self {
            status: "stopped".to_string(),
            is_healthy: false,
            pid: None,
            model: String::new(),
            context: 32768,
            profile: crate::config::DEFAULT_PROFILE_ID.to_string(),
            port: 0,
            idle_remaining_secs: None,
            last_error: None,
            verifying: false,
            starting_for_secs: 0,
            eta_secs: 0,
            request_tps: None,
            request_tps_samples: Vec::new(),
            decode_tps: None,
            decode_tps_samples: Vec::new(),
            engine_slow: false,
            engine_slow_known: false,
            acceptance_ok: None,
            acceptance_error: None,
        }
    }
}

fn engine_slow_assessment(decode_tps: Option<f64>, threshold: f64) -> (bool, bool) {
    decode_tps
        .map(|tps| (true, gate_is_slow(tps, threshold)))
        .unwrap_or((false, false))
}

#[derive(Debug, Clone, Copy)]
struct AcceptanceSample {
    request_tps: f64,
    decode_tps: Option<f64>,
}

#[derive(Debug)]
struct AcceptanceGateResult {
    request_tps: f64,
    request_tps_samples: Vec<f64>,
    decode_tps: Option<f64>,
    decode_tps_samples: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuDevice {
    pub id: String,
    pub name: String,
    pub vram: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareInfo {
    pub cpu_cores: usize,
    pub cpu_name: String,
    pub gpus: Vec<GpuDevice>,
    /// Núcleos FÍSICOS estimados (lógicos / 2 con SMT, best-effort): para
    /// sugerir hilos sin copiar la config del dueño (6 = su Ryzen, no el
    /// del usuario). `None` si no se puede estimar.
    #[serde(default)]
    pub cpu_physical: Option<usize>,
    /// RAM total del sistema en MB (best-effort, WMI); dato informativo, no se
    /// usa para afirmar que el KV activo se desborda a RAM. `None` sin dato.
    #[serde(default)]
    pub ram_total_mb: Option<u64>,
    /// VRAM total dedicada en MB (máximo entre GPUs dedicadas, best-effort):
    /// número para el guard y la UI, no texto libre. `None` sin dato.
    #[serde(default)]
    pub vram_total_mb: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub model: Option<String>,
    pub profile: Option<String>,
    pub context: Option<usize>,
    pub threads: Option<String>,
    pub priority: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LogEvent {
    pub seq: u64,
    pub line: String,
}

/// Parámetros ya resueltos de UN arranque lógico: lo que `start()` decidió
/// (modelo+path, contexto, perfil, hilos, prioridad, puerto, mmproj). El
/// reintento MTP dirigido relanza con ESTOS valores y solo omite `--spec-*`;
/// nunca lee `st.context`/`st.model` (stale: `[last]` se persiste al final
/// del `start()` original). Clonable para cruzar al hilo del poller.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ResolvedStart {
    model_filename: String,
    model_path: PathBuf,
    profile: crate::config::HardwareProfile,
    profile_id: String,
    context: usize,
    threads: usize,
    threads_batch: usize,
    priority: String,
    process_priority_class: u32,
    llama_port: u16,
    mmproj: Option<PathBuf>,
}
pub struct ProcessManager {
    child: Arc<Mutex<Option<Child>>>,
    status: Arc<RwLock<ServerStatus>>,
    recent_logs: Arc<RwLock<VecDeque<LogEvent>>>,
    log_senders: Arc<Mutex<Vec<mpsc::Sender<String>>>>,
    running_poll: Arc<AtomicBool>,
    log_seq: Arc<AtomicU64>,
    /// UNIX-epoch secs de la última generación (chat request). Para auto-stop por inactividad.
    last_activity: Arc<AtomicU64>,
    /// Instante en que empezó el arranque vigente (para `starting_for_secs` y ETA).
    start_instant: Arc<Mutex<Option<Instant>>>,
    /// Generación de arranque: se incrementa al ABRIR cada `start()` (tb. si se
    /// rechaza) y en cada `stop()`, para que el poller detecte un arranque nuevo
    /// aunque haya estado bloqueado en la puerta, y para que el veredicto de una
    /// puerta en vuelo se descarte si pertenece a un intento ya cerrado.
    start_epoch: Arc<AtomicU64>,
    /// Duraciones de cargas exitosas (clave = modelo + contexto), para ETA.
    load_times: Arc<Mutex<HashMap<String, u64>>>,
    /// Archivo de log con rotación (P30): se crea perezoso al primer `log()`.
    log_file: PathBuf,
    /// Historial de arranques (epoch secs) para cooldown + tope horario (LM-NF-3).
    start_history: Arc<Mutex<Vec<u64>>>,
    /// El guard sobrevive cierres/reinicios de OMNI; un historial ilegible
    /// bloquea el arranque en vez de resetear silenciosamente el límite.
    start_history_file: PathBuf,
    start_history_load_error: Option<String>,
    /// Reintento MTP ya consumido en el arranque vigente (una sola vez).
    mtp_retry_done: Arc<AtomicBool>,
    config: Arc<ConfigStore>,
    /// Parámetros resueltos del arranque vigente (para el reintento MTP).
    /// `None` fuera de `starting`; se fija en `start()` y se limpia en
    /// `stop()`/transición final. El reintento usa ESTO, nunca el status.
    pending_start: Arc<Mutex<Option<ResolvedStart>>>,
    base_dir: PathBuf,
    /// Lock global de arranque (P1-seg): `start()` lo toma con `try_lock` al
    /// abrir y lo sostiene hasta volver; si está ocupado devuelve
    /// `Err("...en curso...")` sin tocar estado. Hace atómico el
    /// check→spawn: dos `POST /api/start` concurrentes (el gateway atiende
    /// cada request en su hilo) no pueden validar y lanzar dos hijos a la vez.
    start_lock: Arc<Mutex<()>>,
    /// Lock de importación (P1 2026-10-10, re-auditoría B): `import_model_from_path`
    /// lo sostiene durante check+copy para que dos `POST /api/import_model`
    /// concurrentes con el mismo nombre no pasen ambos el 409 y se pisen.
    import_lock: Arc<Mutex<()>>,
    bin_dir: PathBuf,
    models_dir: PathBuf,
}

impl ProcessManager {
    pub fn new(base_dir: PathBuf, config: Arc<ConfigStore>) -> Self {
        let log_file = crate::filelog::log_file(&base_dir);
        Self::new_with_log_file(base_dir, config, log_file)
    }

    /// Núcleo con la ruta del log EXPLÍCITA, para que un test pueda construir
    /// un `ProcessManager` sin escribir en el log real del dueño. Mismo criterio
    /// que `ConfigStore::load_from_path`: la env global se resuelve una vez
    /// (`new()`) y el núcleo puro recibe el path, porque los tests corren en
    /// paralelo y `set_var` sería una data race. No es una API pública: solo la
    /// usan `new()` y los tests de este módulo.
    fn new_with_log_file(base_dir: PathBuf, config: Arc<ConfigStore>, log_file: PathBuf) -> Self {
        let bin_dir = base_dir.join("bin");
        let models_dir = base_dir.join("models");

        let start_history_file = config
            .path()
            .parent()
            .unwrap_or(&base_dir)
            .join("start-history.json");
        let (history, start_history_load_error) =
            match load_start_history(&start_history_file, unix_now_secs()) {
                Ok(history) => (history, None),
                Err(error) => (Vec::new(), Some(error)),
            };

        let cfg = config.get();
        let status = Arc::new(RwLock::new(ServerStatus {
            status: "stopped".to_string(),
            is_healthy: false,
            pid: None,
            model: String::new(),
            context: 32768,
            profile: crate::config::DEFAULT_PROFILE_ID.to_string(),
            port: cfg.engine.llama_port,
            idle_remaining_secs: None,
            last_error: None,
            verifying: false,
            starting_for_secs: 0,
            eta_secs: 0,
            request_tps: None,
            request_tps_samples: Vec::new(),
            decode_tps: None,
            decode_tps_samples: Vec::new(),
            engine_slow: false,
            engine_slow_known: false,
            acceptance_ok: None,
            acceptance_error: None,
        }));

        let recent_logs: Arc<RwLock<VecDeque<LogEvent>>> = Arc::new(RwLock::new(VecDeque::new()));
        let log_senders: Arc<Mutex<Vec<mpsc::Sender<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let running_poll = Arc::new(AtomicBool::new(true));
        let log_seq = Arc::new(AtomicU64::new(0));
        let last_activity = Arc::new(AtomicU64::new(0));
        let start_instant: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let start_epoch = Arc::new(AtomicU64::new(0));
        let start_history: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(history));
        let mtp_retry_done = Arc::new(AtomicBool::new(false));
        let pending_start: Arc<Mutex<Option<ResolvedStart>>> = Arc::new(Mutex::new(None));
        let start_lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let import_lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let load_times_path = Self::load_times_path();
        let load_times: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(
            load_times_load(&load_times_path).unwrap_or_default(),
        ));

        // Background poller: monitors health AND child process liveness
        let status_clone = Arc::clone(&status);
        let child_clone = Arc::clone(&child);
        let logs_for_err = Arc::clone(&recent_logs);
        let poll_flag = Arc::clone(&running_poll);
        let cfg_poll = Arc::clone(&config);
        let last_activity_poll = Arc::clone(&last_activity);
        let recent_logs_clone = Arc::clone(&recent_logs);
        let senders_clone = Arc::clone(&log_senders);
        let seq_clone = Arc::clone(&log_seq);
        let start_instant_poll = Arc::clone(&start_instant);
        let start_epoch_poll = Arc::clone(&start_epoch);
        let load_times_poll = Arc::clone(&load_times);
        let load_times_path_poll = load_times_path.clone();
        let models_dir_poll = models_dir.clone();
        let bin_dir_poll = bin_dir.clone();
        let base_dir_poll = base_dir.clone();
        let log_file_poll = log_file.clone();
        let mtp_retry_poll = Arc::clone(&mtp_retry_done);
        let pending_poll = Arc::clone(&pending_start);
        thread::spawn(move || {
            let mut consecutive_failures = 0u32;
            // La puerta de aceptación corre una sola vez por arranque (D1).
            let mut gate_done = false;
            let mut seen_epoch = start_epoch_poll.load(Ordering::Relaxed);
            while poll_flag.load(Ordering::Relaxed) {
                // Arranque nuevo (o stop): rearmar la puerta de aceptación.
                let ep = start_epoch_poll.load(Ordering::Relaxed);
                if ep != seen_epoch {
                    seen_epoch = ep;
                    gate_done = false;
                    consecutive_failures = 0;
                }
                // 1. Check if child process has exited or crashed
                let exited = {
                    let mut cl = child_clone.lock();
                    if let Some(c) = cl.as_mut() {
                        c.try_wait().ok().flatten()
                    } else {
                        None
                    }
                };
                let mut auto_stop = false;
                if let Some(exit_status) = exited {
                    let code = exit_status.to_string();
                    // Líneas recientes para el resumen Y para la firma MTP.
                    let lines: Vec<String> = {
                        let logs = logs_for_err.read();
                        logs.iter().map(|l| l.line.clone()).collect()
                    };
                    let summary = crash_summary(&lines, &code);
                    // Reintento MTP dirigido (NO es el auto-reintento genérico
                    // que prohíbe LM-NF-3): solo en `starting`, solo con la
                    // firma exacta, solo si spec está habilitada, solo una vez
                    // por arranque, y sin consumir presupuesto de cooldown/tope
                    // (es el mismo arranque lógico). Se ejecutainline: relanza
                    // el hijo sin `--spec-*` con los mismos argv ya resueltos
                    // (ver `retry_no_spec` abajo); si vuelve a fallar, `error`.
                    let was_starting = status_clone.read().status == "starting";
                    let spec_on = cfg_poll
                        .get()
                        .engine
                        .speculation
                        .as_ref()
                        .is_some_and(|s| s.enabled);
                    let mut mtp_retry = false;
                    if mtp_retry_decision(
                        was_starting,
                        spec_on,
                        mtp_retry_poll.load(Ordering::Relaxed),
                        mtp_unsupported_signature(&lines),
                    ) {
                        mtp_retry_poll.store(true, Ordering::Relaxed);
                        mtp_retry = true;
                    }
                    // Watchdog (LM-NF-3): si el hijo muere en starting/running
                    // (incluidos los primeros ~90 s tras un arranque), se marca
                    // `error` con el resumen y NO se reintenta ni se rearranca
                    // solo: los bucles de reintento convierten un fallo en
                    // transitorios repetidos = riesgo PSU. Reintento = Stop+Start
                    // manual (respetando cooldown + tope horario).
                    // Aviso P20 pendiente si el crash era visible (starting/running).
                    let mut st = status_clone.write();
                    let mut crash_notify: Option<(String, String, String)> = None;
                    if st.status == "starting" || st.status == "running" {
                        st.status = "error".to_string();
                        st.is_healthy = false;
                        st.pid = None;
                        st.verifying = false;
                        st.starting_for_secs = 0;
                        st.eta_secs = 0;
                        st.request_tps = None;
                        st.request_tps_samples = Vec::new();
                        st.decode_tps = None;
                        st.decode_tps_samples = Vec::new();
                        st.engine_slow = false;
                        st.engine_slow_known = false;
                        st.acceptance_ok = None;
                        st.acceptance_error = None;
                        crash_notify = Some((
                            "El motor falló".to_string(),
                            summary,
                            "engine-failure".to_string(),
                        ));
                    }
                    drop(st);
                    // Reintento dirigido: relanzar el MISMO arranque sin spec.
                    // Sin cooldown/tope (mismo arranque lógico: el guard ya
                    // pasó y `record_start` NO se repite), sin tocar la puerta
                    // (`starting` sigue, `gate_done` intacto). Si vuelve a
                    // fallar → `error` normal (el flag ya está consumido).
                    if mtp_retry {
                        Self::push_log_to(
                            &recent_logs_clone,
                            &senders_clone,
                            &seq_clone,
                            "[LocalMind] El modelo no soporta decodificación especulativa (MTP): reintentando sin --spec-*",
                            Some(&log_file_poll),
                        );
                        // Mismo arranque lógico: parámetros resueltos en
                        // `start()` (NO `st.context`/`st.model`: stale). Sin
                        // cooldown/tope (el guard ya pasó), sin tocar la
                        // puerta (`starting` sigue, `gate_done` intacto).
                        // `take()` = una sola vez aunque el poller repita.
                        let params = pending_poll.lock().take();
                        *child_clone.lock() = None;
                        *start_instant_poll.lock() = Some(Instant::now());
                        match params {
                            Some(p) => Self::spawn_child_nospec(
                                &child_clone,
                                &recent_logs_clone,
                                &senders_clone,
                                &seq_clone,
                                &log_file_poll,
                                &status_clone,
                                &cfg_poll,
                                bin_dir_poll.clone(),
                                base_dir_poll.clone(),
                                p,
                            ),
                            None => Self::push_log_to(
                                &recent_logs_clone,
                                &senders_clone,
                                &seq_clone,
                                "[LocalMind] Reintento MTP omitido: sin parámetros del arranque vigente.",
                                Some(&log_file_poll),
                            ),
                        }
                    }
                    if let Some((title, body, tag)) = crash_notify {
                        let ncfg = cfg_poll.get().notifications;
                        spawn_notify(
                            title,
                            body,
                            tag,
                            ncfg.enabled,
                            ncfg.on_failure,
                            log_file_poll.clone(),
                        );
                    }
                } else {
                    // 2. Poll health endpoint if still starting or running
                    let (current_status, port) = {
                        let st = status_clone.read();
                        (st.status.clone(), st.port)
                    };
                    if current_status == "starting" || current_status == "running" {
                        // Progreso/ETA (D4): sin bloquear el lock durante el HTTP.
                        let (elapsed_secs, model_snapshot, context_snapshot) = {
                            let st = status_clone.read();
                            let elapsed = start_instant_poll
                                .lock()
                                .as_ref()
                                .map(|t| t.elapsed().as_secs())
                                .unwrap_or(0);
                            (elapsed, st.model.clone(), st.context)
                        };
                        {
                            let mut st = status_clone.write();
                            st.starting_for_secs = if current_status == "starting" {
                                elapsed_secs
                            } else {
                                0
                            };
                            if current_status == "starting" {
                                let stored = load_times_poll
                                    .lock()
                                    .get(&load_key(&model_snapshot, context_snapshot))
                                    .copied();
                                let size_gb =
                                    Self::model_size_gb(&models_dir_poll, &model_snapshot);
                                st.eta_secs = eta_secs(stored, size_gb);
                            } else {
                                st.eta_secs = 0;
                            }
                        }
                        let is_ok = Self::health_check(port);
                        // Puerta de aceptación (D1): una sola vez por arranque, sin el lock
                        // cogido durante la request de verificación (el status sigue legible).
                        // CI (`-D warnings`): el alias no compensa para un local de 1 uso.
                        let mut gate_outcome: Option<Result<AcceptanceGateResult, String>> = None;
                        if is_ok && current_status == "starting" && !gate_done {
                            gate_done = true;
                            let my_epoch = seen_epoch;
                            {
                                let mut st = status_clone.write();
                                st.verifying = true;
                            }
                            let res = Self::run_acceptance_gate(port);
                            // Si hubo un stop()/start() durante la verificación (120 s),
                            // el resultado es de otro arranque: se descarta.
                            if start_epoch_poll.load(Ordering::Relaxed) == my_epoch {
                                gate_outcome = Some(res);
                            }
                        }
                        let mut pending_ok_log: Option<String> = None;
                        // Avisos P20 pendientes (se emiten tras soltar el lock).
                        let mut pending_notify: Option<(String, String, String)> = None;
                        {
                            let mut st = status_clone.write();
                            st.is_healthy = is_ok;
                            if let Some(outcome) = gate_outcome {
                                match outcome {
                                    Ok(gate_result) => {
                                        let request_tps = gate_result.request_tps;
                                        let decode_tps = gate_result.decode_tps;
                                        // Verificación de contexto real (cinturón:
                                        // el motor dice `n_ctx` en `/props`).
                                        // Si difiere del pedido → `error` en
                                        // español con ambos valores (nunca
                                        // sustitución silenciosa, D2).
                                        let req_ctx = st.context;
                                        let props_ctx = Self::engine_n_ctx(port);
                                        let mismatch = props_ctx.is_some_and(|n| n != req_ctx);
                                        if mismatch {
                                            let n = props_ctx.unwrap_or(0);
                                            let msg = format!(
                                                "El motor cargó con contexto {} pero se pidió {}.",
                                                n, req_ctx
                                            );
                                            st.status = "error".to_string();
                                            st.is_healthy = false;
                                            st.verifying = false;
                                            st.starting_for_secs = 0;
                                            st.eta_secs = 0;
                                            st.request_tps = Some(request_tps);
                                            st.request_tps_samples =
                                                gate_result.request_tps_samples.clone();
                                            st.decode_tps = decode_tps;
                                            st.decode_tps_samples =
                                                gate_result.decode_tps_samples.clone();
                                            (st.engine_slow_known, st.engine_slow) =
                                                engine_slow_assessment(
                                                    decode_tps,
                                                    crate::engine_gate::slow_threshold(
                                                        cfg_poll.get().engine.slow_gate_tps,
                                                        Self::model_size_gb(
                                                            &models_dir_poll,
                                                            &st.model,
                                                        ),
                                                    ),
                                                );
                                            st.acceptance_ok = Some(false);
                                            st.acceptance_error = Some(msg.clone());
                                            st.last_error = Some(msg.clone());
                                            pending_ok_log = Some(format!("[LocalMind] {}", msg));
                                            *start_instant_poll.lock() = None;
                                        } else {
                                            // Carga exitosa: registrar duración (D4) y declarar running.
                                            let secs = start_instant_poll
                                                .lock()
                                                .as_ref()
                                                .map(|t| t.elapsed().as_secs())
                                                .unwrap_or(0);
                                            let key = load_key(&st.model, st.context);
                                            load_times_poll.lock().insert(key.clone(), secs.max(1));
                                            let _ = load_times_save(
                                                &load_times_path_poll,
                                                &load_times_poll.lock(),
                                            );
                                            let slow_at = crate::engine_gate::slow_threshold(
                                                cfg_poll.get().engine.slow_gate_tps,
                                                Self::model_size_gb(&models_dir_poll, &st.model),
                                            );
                                            st.status = "running".to_string();
                                            st.verifying = false;
                                            st.starting_for_secs = 0;
                                            st.eta_secs = 0;
                                            st.request_tps = Some(request_tps);
                                            st.request_tps_samples =
                                                gate_result.request_tps_samples.clone();
                                            st.decode_tps = decode_tps;
                                            st.decode_tps_samples =
                                                gate_result.decode_tps_samples.clone();
                                            // engine_slow: solo informativo, sin
                                            // reintentos (LM-NF-3 lo prohíbe).
                                            (st.engine_slow_known, st.engine_slow) =
                                                engine_slow_assessment(decode_tps, slow_at);
                                            st.acceptance_ok = Some(true);
                                            st.acceptance_error = None;
                                            st.last_error = None;
                                            let decode_note = decode_tps
                                                .map(|tps| format!("decode {:.1} t/s", tps))
                                                .unwrap_or_else(|| {
                                                    "decode no reportado".to_string()
                                                });
                                            pending_ok_log = Some(format!(
                                                "[LocalMind] Puerta de arranque OK: request {:.1} t/s; {}; {} muestra(s).",
                                                request_tps,
                                                decode_note,
                                                gate_result.request_tps_samples.len()
                                            ));
                                            // Aviso P20: motor listo, con decode solo si el motor lo reportó.
                                            pending_notify = Some((
                                                "Motor listo".to_string(),
                                                format!(
                                                    "«{}» respondió; request {:.1} t/s; {}.",
                                                    st.model, request_tps, decode_note
                                                ),
                                                "engine-ready".to_string(),
                                            ));
                                        }
                                    }
                                    Err(detail) => {
                                        st.status = "error".to_string();
                                        st.is_healthy = false;
                                        st.verifying = false;
                                        st.starting_for_secs = 0;
                                        st.eta_secs = 0;
                                        st.request_tps = None;
                                        st.request_tps_samples = Vec::new();
                                        st.decode_tps = None;
                                        st.decode_tps_samples = Vec::new();
                                        st.engine_slow = false;
                                        st.engine_slow_known = false;
                                        st.acceptance_error = Some(detail.clone());
                                        st.last_error = Some(detail.clone());
                                        // Aviso P20: fallo de la puerta con el motivo.
                                        pending_notify = Some((
                                            "El motor falló".to_string(),
                                            detail,
                                            "engine-failure".to_string(),
                                        ));
                                        *start_instant_poll.lock() = None;
                                    }
                                }
                            }
                            consecutive_failures = if is_ok {
                                0
                            } else {
                                consecutive_failures.saturating_add(1)
                            };
                            // 10 fallos seguidos (~10s) con status running → el hijo murió sin exit visible
                            if engine_lost(consecutive_failures, st.status == "running") {
                                st.status = "error".to_string();
                                st.last_error = Some(
                                    "El motor dejó de responder el endpoint /health".to_string(),
                                );
                            }
                            // Auto-stop por inactividad (solo si está running y healthy)
                            let timeout = cfg_poll.get().engine.idle_timeout_secs;
                            if timeout > 0 && st.status == "running" {
                                // Verificar si llama-server tiene slots procesando actualmente
                                let is_busy = Self::check_slots_busy(port);
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0);
                                if is_busy {
                                    // El motor está trabajando activamente: refrescar marca de actividad
                                    last_activity_poll.store(now, Ordering::Relaxed);
                                } else if should_auto_stop(
                                    timeout,
                                    last_activity_poll.load(Ordering::Relaxed),
                                    now,
                                ) {
                                    st.status = "stopped".to_string();
                                    st.is_healthy = false;
                                    st.pid = None;
                                    auto_stop = true;
                                } else {
                                    // Pre-aviso único 5 min antes: cualquier request
                                    // proxyeado refresca `last_activity` y lo cancela.
                                    let elapsed = now
                                        .saturating_sub(last_activity_poll.load(Ordering::Relaxed));
                                    let left = timeout.saturating_sub(elapsed);
                                    if left <= 300 && left > 0 {
                                        static WARNED_ONCE: std::sync::atomic::AtomicU64 =
                                            std::sync::atomic::AtomicU64::new(0);
                                        // Marcar por ventana de 5 min (evita spam
                                        // del poller cada 500 ms).
                                        let mark = now / 300;
                                        if WARNED_ONCE
                                            .swap(mark, std::sync::atomic::Ordering::Relaxed)
                                            != mark
                                        {
                                            let ncfg = cfg_poll.get().notifications;
                                            if crate::notify::should_notify(
                                                ncfg.enabled,
                                                ncfg.on_autostop,
                                            ) {
                                                let logf = log_file_poll.clone();
                                                let mins = left / 60;
                                                let secs = left % 60;
                                                std::thread::spawn(move || {
                                                    crate::notify::notify(
                                                        "El motor se apagará pronto",
                                                        &format!(
                                                            "Sin actividad {}m {}s: se apagará solo y liberará la VRAM. Usa el chat o un agente para mantenerlo vivo.",
                                                            mins, secs
                                                        ),
                                                        "engine-autostop-soon",
                                                        |err| crate::filelog::write_log_line(&logf, err),
                                                    );
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(msg) = pending_ok_log {
                            Self::push_log_to(
                                &recent_logs_clone,
                                &senders_clone,
                                &seq_clone,
                                &msg,
                                Some(&log_file_poll),
                            );
                        }
                        // Avisos P20 (fuera del lock): respeta la config viva.
                        if let Some((title, body, tag)) = pending_notify {
                            let ncfg = cfg_poll.get().notifications;
                            let flag = match tag.as_str() {
                                "engine-ready" => ncfg.on_ready,
                                "engine-failure" => ncfg.on_failure,
                                _ => true,
                            };
                            spawn_notify(
                                title,
                                body,
                                tag,
                                ncfg.enabled,
                                flag,
                                log_file_poll.clone(),
                            );
                        }
                    }
                }

                if auto_stop {
                    // Kill del árbol del proceso desde el poller (no podemos usar &self aquí).
                    let pid = {
                        let cl = child_clone.lock();
                        cl.as_ref().map(|c| c.id())
                    };
                    if let Some(pid) = pid {
                        let mut tk = Command::new("taskkill");
                        tk.args(["/F", "/T", "/PID", &pid.to_string()]);
                        tk.creation_flags(CREATE_NO_WINDOW);
                        let _ = tk.output();
                        let _ = child_clone.lock().take().map(|mut c| {
                            let _ = c.kill();
                            let _ = c.wait();
                        });
                        Self::push_log_to(&recent_logs_clone, &senders_clone, &seq_clone,
                            "[LocalMind] Auto-stop: motor apagado por inactividad. VRAM y memoria liberadas.",
                            Some(&log_file_poll));
                        // Aviso P20: auto-stop.
                        let ncfg = cfg_poll.get().notifications;
                        if crate::notify::should_notify(ncfg.enabled, ncfg.on_autostop) {
                            let logf = log_file_poll.clone();
                            std::thread::spawn(move || {
                                crate::notify::notify(
                                    "Motor apagado por inactividad",
                                    "El motor se detuvo solo: llevaba 25 min sin uso. La VRAM quedó libre.",
                                    "engine-autostop",
                                    |err| crate::filelog::write_log_line(&logf, err),
                                );
                            });
                        }
                    }
                }
                thread::sleep(Duration::from_millis(500));
            }
        });

        Self {
            child,
            status,
            recent_logs,
            log_senders,
            running_poll,
            log_seq,
            last_activity: Arc::clone(&last_activity),
            start_instant,
            start_epoch,
            load_times,
            log_file,
            start_history,
            start_history_file,
            start_history_load_error,
            mtp_retry_done,
            pending_start,
            import_lock,
            start_lock,
            config,
            base_dir,
            bin_dir,
            models_dir,
        }
    }

    /// Constructor del argv del motor (extraído de `start()` para que el
    /// reintento MTP dirigido relance el MISMO arranque sin `--spec-*`.
    /// `skip_spec` = true solo en ese reintento. Sin pánicos en flags:
    /// el PSU-lock ya validó `extra_flags` en `start()`.
    ///
    /// D-47: esta es la FUENTE ÚNICA de las flags del motor. `start()` ya no
    /// las vuelve a anexar: cada flag llega al hijo exactamente una vez y el
    /// argv del log es, por fin, el argv del hijo. `api_key` es la clave del
    /// gateway, que el motor exige SIN ir en argv (P1-seg): viaja por la
    /// variable de entorno del hijo `LLAMA_API_KEY` (D-45). El build 10743
    /// documenta `(env: LLAMA_API_KEY)` junto a `--api-key` en su `--help`,
    /// así que el soporte vía env está verificado contra el binario real.
    /// Motivo: el argv es legible por cualquier proceso local (Task Manager,
    /// `wmic process get commandline`, ETW); el env del hijo solo lo lee su
    /// propio proceso. El proxy interno sigue mandando
    /// `Authorization: Bearer <clave>` en cada llamada al motor.
    #[allow(clippy::too_many_arguments)]
    fn build_engine_cmd(
        llama_bin: &std::path::Path,
        base_dir: &std::path::Path,
        process_priority_class: u32,
        model_path: &std::path::Path,
        context: usize,
        threads: usize,
        threads_batch: usize,
        priority: &str,
        engine: &crate::config::EngineConfig,
        profile: &crate::config::HardwareProfile,
        llama_port: u16,
        skip_spec: bool,
        api_key: &str,
    ) -> std::process::Command {
        let ubatch = "512";
        let mut cmd = std::process::Command::new(llama_bin);
        cmd.current_dir(base_dir);
        cmd.creation_flags(CREATE_NO_WINDOW | process_priority_class);
        // Sin `unwrap`: con `panic = "abort"` una ruta no-UTF-8 mataría el
        // proceso entero (WebView + HTTP + motor huérfano). En Windows la ruta
        // es UTF-8 siempre; `to_string_lossy` solo evita el panic.
        let model_arg = model_path.to_string_lossy().to_string();
        cmd.args([
            "-m",
            model_arg.as_str(),
            "-ngl",
            "99",
            "-c",
            &context.to_string(),
            "-ctk",
            "q4_0",
            "-ctv",
            "q4_0",
            "-a",
            "localmind",
            "--reuse-port",
            "-t",
            &threads.to_string(),
            "-tb",
            &threads_batch.to_string(),
            "--prio",
            priority,
            "-b",
            &engine.batch.to_string(),
            "-ub",
            ubatch,
            "--device",
            &engine.device,
            "--split-mode",
            "none",
            "--host",
            "127.0.0.1",
            "--port",
            &llama_port.to_string(),
            "-np",
            "1",
            "--poll",
            &engine.poll.to_string(),
            "--prio-batch",
            &engine.priority_batch,
        ]);
        if engine.flash_attention {
            cmd.args(["-fa", "on"]);
        }
        if engine.metrics {
            cmd.arg("--metrics");
        }
        // LM-NF-6 / P29: el puerto crudo del motor deja de ser una puerta
        // abierta. El build 10743 acepta la clave por `--api-key` o por su env
        // `LLAMA_API_KEY` (`--help`: `(env: LLAMA_API_KEY)`), y rechaza sin el
        // `Authorization: Bearer <clave>` (`unauthorized: Invalid API Key`).
        // Se usa el ENV del hijo, no el argv: el argv lo lee cualquier proceso
        // local y la clave quedaba expuesta en Task Manager / wmic / ETW.
        // `Command::env` solo afecta al hijo (no toca el env del gateway).
        // Reutiliza la clave del gateway: no se acuña una segunda. Los 8
        // puntos que hablan con el motor (health/slots/props/chat en
        // `process.rs`, metrics/chat en `server.rs`) la envían.
        cmd.env("LLAMA_API_KEY", api_key);
        if let Some(spec) = engine
            .speculation
            .as_ref()
            .filter(|s| s.enabled && !skip_spec)
        {
            cmd.args([
                "--spec-type",
                &spec.ty,
                "--spec-draft-n-max",
                &spec.draft_n_max.to_string(),
                "--spec-draft-p-split",
                &spec.draft_p_split.to_string(),
            ]);
        }
        if engine.reasoning_preserve {
            cmd.arg("--reasoning-preserve");
        }
        // P29: `--cache-ram` limita la caché de prompts en RAM de llama-server;
        // no ubica ni desborda el KV activo. Hay que pasar también 0 porque el
        // default del binario la habilita (8192 MiB); omitir el flag no la apaga.
        cmd.args(["--cache-ram", &profile.cache_ram.to_string()]);
        for flag in &profile.extra_flags {
            cmd.arg(flag);
        }
        for flag in &engine.extra_flags {
            cmd.arg(flag);
        }
        cmd
    }
    /// Relanzador MTP (solo lo usa el reintento dirigido): relanza el hijo SIN
    /// `--spec-*` con los parámetros RESUELTOS del arranque vigente (`params`:
    /// mismo modelo/path, contexto, perfil, hilos, prioridad, puerto, mmproj).
    /// Difiere del primer spawn SOLO en omitir spec. NO toca cooldown/tope
    /// (mismo arranque lógico), NO re-registra historial, NO revalida (ya
    /// validado), NO re-resuelve fallbacks, NO lee `st.context`/`st.model`.
    /// Reengancha stdout/stderr a los lectores; deja `starting` para la puerta.
    /// CI (`-D warnings`): 10 args espejo de `start()`; struct rompería la paridad.
    #[allow(clippy::too_many_arguments)]
    fn spawn_child_nospec(
        child: &Arc<Mutex<Option<std::process::Child>>>,
        logs: &Arc<RwLock<VecDeque<LogEvent>>>,
        senders: &Arc<Mutex<Vec<mpsc::Sender<String>>>>,
        seq: &Arc<AtomicU64>,
        log_file: &std::path::Path,
        status: &Arc<RwLock<ServerStatus>>,
        config: &Arc<ConfigStore>,
        bin_dir: std::path::PathBuf,
        base_dir: std::path::PathBuf,
        params: ResolvedStart,
    ) {
        let cfg = config.get();
        let llama_bin = bin_dir.join("llama-server.exe");
        let mut cmd = Self::build_engine_cmd(
            &llama_bin,
            &base_dir,
            params.process_priority_class,
            &params.model_path,
            params.context,
            params.threads,
            params.threads_batch,
            &params.priority,
            &cfg.engine,
            &params.profile,
            params.llama_port,
            true,
            // Misma clave que el primer arranque: el reintento relanza el mismo
            // login del motor, no uno nuevo.
            crate::auth::gateway_key(),
        );
        if let Some(mm) = params.mmproj.as_ref() {
            let mm_arg = mm.to_string_lossy().to_string();
            cmd.args(["--mmproj", mm_arg.as_str()]);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child_proc = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Error al iniciar llama-server: {}", e);
                Self::push_log_to(logs, senders, seq, &msg, Some(log_file));
                let mut st = status.write();
                st.status = "error".to_string();
                st.last_error = Some(msg);
                return;
            }
        };
        let pid = child_proc.id();
        let stdout = child_proc.stdout.take();
        let stderr = child_proc.stderr.take();
        Self::attach_reader_threads(stdout, stderr, logs, senders, seq, log_file);
        *child.lock() = Some(child_proc);
        {
            let mut st = status.write();
            st.status = "starting".to_string();
            st.is_healthy = false;
            st.pid = Some(pid);
            st.last_error = None;
            st.verifying = false;
        }
    }

    fn attach_reader_threads(
        stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
        logs: &Arc<RwLock<VecDeque<LogEvent>>>,
        senders: &Arc<Mutex<Vec<mpsc::Sender<String>>>>,
        seq: &Arc<AtomicU64>,
        log_file: &std::path::Path,
    ) {
        if let Some(out) = stdout {
            let l_c = Arc::clone(logs);
            let s_c = Arc::clone(senders);
            let q_c = Arc::clone(seq);
            let f_c = log_file.to_path_buf();
            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(out);
                use std::io::BufRead;
                for line in reader.lines().map_while(Result::ok) {
                    let s = q_c.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut l = l_c.write();
                        if l.len() >= 250 {
                            l.pop_front();
                        }
                        l.push_back(LogEvent {
                            seq: s,
                            line: line.clone(),
                        });
                    }
                    {
                        let mut x = s_c.lock();
                        x.retain(|tx| tx.send(line.clone()).is_ok());
                    }
                    crate::filelog::write_log_line(&f_c, &line);
                }
            });
        }
        if let Some(err) = stderr {
            let l_c = Arc::clone(logs);
            let s_c = Arc::clone(senders);
            let q_c = Arc::clone(seq);
            let f_c = log_file.to_path_buf();
            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(err);
                use std::io::BufRead;
                for line in reader.lines().map_while(Result::ok) {
                    let s = q_c.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut l = l_c.write();
                        if l.len() >= 250 {
                            l.pop_front();
                        }
                        l.push_back(LogEvent {
                            seq: s,
                            line: line.clone(),
                        });
                    }
                    {
                        let mut x = s_c.lock();
                        x.retain(|tx| tx.send(line.clone()).is_ok());
                    }
                    crate::filelog::write_log_line(&f_c, &line);
                }
            });
        }
    }
    fn health_check(port: u16) -> bool {
        matches!(
            ureq::get(&format!("http://127.0.0.1:{}/health", port))
                .set("Authorization", &crate::auth::bearer(crate::auth::gateway_key()))
                .timeout(Duration::from_millis(600))
                .call(),
            Ok(resp) if resp.status() == 200
        )
    }

    /// Línea de arranque del log. D-44: describe lo que se PASA, no lo que se
    /// configuró. Antes anunciaba `cache-reuse: <n>` leyendo
    /// `engine.cache_reuse`, pero `--cache-reuse` no se anexa al argv: el build
    /// 10683 lo rechazó en este contexto y el 10743 aún no se ha validado en
    /// vivo con Qwen3.8. El log describía un argv que el proceso no recibió.
    /// Se quita la afirmación en vez de inventar un valor.
    ///
    /// El contexto y los hilos sí son verdad (van como `-c`/`-t`), igual que
    /// el puerto (`--port`) y el uBatch (`-ub`).
    fn start_banner(context: usize, threads: usize, ubatch: &str, port: u16) -> String {
        format!(
            "[LocalMind] Contexto: {} tokens | Hilos: {} | uBatch: {} | Puerto: {}",
            context, threads, ubatch, port
        )
    }

    fn check_slots_busy(port: u16) -> bool {
        let url = format!("http://127.0.0.1:{}/slots", port);
        if let Ok(resp) = ureq::get(&url)
            .set(
                "Authorization",
                &crate::auth::bearer(crate::auth::gateway_key()),
            )
            .timeout(Duration::from_millis(400))
            .call()
        {
            if let Ok(slots) = resp.into_json::<Vec<serde_json::Value>>() {
                return slots.iter().any(|s| {
                    s.get("is_processing")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                });
            }
        }
        false
    }

    /// `n_ctx` real del motor vía `/props` (cinturón D2): `None` si el campo
    /// falta o el endpoint no responde (sin falso error).
    fn engine_n_ctx(port: u16) -> Option<usize> {
        let text = ureq::get(&format!("http://127.0.0.1:{}/props", port))
            .set(
                "Authorization",
                &crate::auth::bearer(crate::auth::gateway_key()),
            )
            .timeout(Duration::from_secs(5))
            .call()
            .ok()?
            .into_string()
            .ok()?;
        props_n_ctx(&text)
    }

    /// Encontrar un puerto libre empezando en `preferred` (hasta +50 intentos).
    fn find_free_port(preferred: u16) -> u16 {
        for offset in 0..50 {
            let p = preferred + offset;
            if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
                return p;
            }
        }
        preferred
    }

    /// Puerta de aceptación (D1): una completion real que prueba que el motor
    /// responde y supera el mínimo de tasa por solicitud completa. El resultado
    /// conserva esa tasa aparte de decode puro, si el motor informa timings.
    /// No identifica qué dispositivo ejecutó el modelo.
    /// Nunca paniquea: todo fallo se devuelve como `Err(detalle)` en español.
    /// La puerta usa `temperature: 0` FIJO a propósito (determinista): aunque
    /// `[generation]` (P16) exponga otra temperatura para el chat, la puerta no
    /// la consume. Calienta con 1 request desechable y toma hasta 2 muestras.
    /// El mínimo de dos muestras válidas es una cota conservadora para la tasa
    /// de request; las tasas de decode se resumen solo si `timings` existe.
    fn run_acceptance_gate(port: u16) -> Result<AcceptanceGateResult, String> {
        // Calentamiento desechable (mismo path de chat, pocos tokens): estabiliza
        // la primera medida. Solo en la puerta; con el motor en `running` no
        // hay re-calentamiento (la puerta solo corre en starting).
        let _ = Self::gate_sample(port, 16);
        let mut request_samples = Vec::new();
        let mut decode_samples = Vec::new();
        let mut last_err = String::new();
        for _ in 0..2 {
            match Self::gate_sample(port, 200) {
                Ok(sample) => {
                    request_samples.push(sample.request_tps);
                    if let Some(tps) = sample.decode_tps {
                        decode_samples.push(tps);
                    }
                }
                Err(e) => last_err = e,
            }
        }
        if request_samples.is_empty() {
            return Err(if last_err.is_empty() {
                "El modelo no respondió durante la verificación de arranque (sin usage válido)."
                    .to_string()
            } else {
                last_err
            });
        }
        Ok(AcceptanceGateResult {
            request_tps: gate_median(&request_samples).unwrap_or(0.0),
            request_tps_samples: request_samples,
            decode_tps: gate_median(&decode_samples),
            decode_tps_samples: decode_samples,
        })
    }

    /// Una muestra de la puerta separa completion tokens / request total de
    /// `timings.predicted_per_second`, que es decode informado por llama.cpp.
    fn gate_sample(port: u16, max_tokens: u32) -> Result<AcceptanceSample, String> {
        let url = format!("http://127.0.0.1:{}/v1/chat/completions", port);
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "Count from 1 to 60. Nothing else."}],
            "max_tokens": max_tokens,
            "temperature": 0,
            "stream": false,
        });
        let start = Instant::now();
        let resp = ureq::post(&url)
            .set(
                "Authorization",
                &crate::auth::bearer(crate::auth::gateway_key()),
            )
            .timeout(Duration::from_secs(120))
            .send_json(body);
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                return Err(format!(
                    "El modelo no respondió durante la verificación de arranque ({}).",
                    e
                ))
            }
        };
        let text = match resp.into_string() {
            Ok(text) => text,
            Err(e) => {
                return Err(format!(
                    "El modelo no respondió durante la verificación de arranque (no se pudo leer la respuesta: {}).",
                    e
                ));
            }
        };
        // ureq devuelve Response al recibir cabeceras; el cuerpo aún se lee aquí.
        // El cronómetro debe incluir la generación completa, no sólo el TTFT.
        let elapsed_ms = start.elapsed().as_millis().max(1) as u64;
        if text.is_empty() {
            return Err(
                "El modelo no respondió durante la verificación de arranque (respuesta vacía)."
                    .to_string(),
            );
        }
        match parse_usage(&text) {
            Some((_, completion)) => match acceptance_verdict(completion, elapsed_ms) {
                Ok(request_tps) => Ok(AcceptanceSample {
                    request_tps,
                    decode_tps: parse_engine_decode_tps(&text),
                }),
                Err(kind) if kind == "tokens" => Err(format!(
                    "El modelo no respondió durante la verificación de arranque (solo {} completion tokens).",
                    completion
                )),
                Err(_) => Err(format!(
                    "La puerta obtuvo {} completion tokens por segundo de solicitud completa; queda debajo del mínimo de 3.",
                    completion.saturating_mul(1000) / elapsed_ms.max(1)
                )),
            },
            None => Err("El modelo no respondió durante la verificación de arranque (sin usage válido).".to_string()),
        }
    }

    /// Ruta del registro de duraciones de carga (D4).
    fn load_times_path() -> PathBuf {
        std::env::var("APPDATA")
            .map(|p| Path::new(&p).join("LocalMind").join("load-times.json"))
            .unwrap_or_else(|_| PathBuf::from("load-times.json"))
    }

    fn model_size_gb(models_dir: &Path, model: &str) -> f64 {
        if model.trim().is_empty() {
            return 0.0;
        }
        // Misma resolución segura que el arranque (basename o `rel`); si no
        // resuelve, tamaño desconocido (0) en vez de pánico o escape.
        resolve_model_path(models_dir, model)
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| (m.len() as f64) / (1024.0 * 1024.0 * 1024.0))
            .unwrap_or(0.0)
    }

    /// Push con archivo opcional (P30): el poller y los lectores del motor
    /// pasan su `log_file`; `None` solo en tests.
    fn push_log_to(
        logs: &Arc<RwLock<VecDeque<LogEvent>>>,
        senders: &Arc<Mutex<Vec<mpsc::Sender<String>>>>,
        seq: &Arc<AtomicU64>,
        msg: &str,
        file: Option<&Path>,
    ) {
        {
            let mut l = logs.write();
            if l.len() >= 250 {
                l.pop_front();
            }
            l.push_back(LogEvent {
                seq: seq.fetch_add(1, Ordering::Relaxed),
                line: msg.to_string(),
            });
        }
        for tx in senders.lock().iter() {
            let _ = tx.send(msg.to_string());
        }
        if let Some(f) = file {
            crate::filelog::write_log_line(f, msg);
        }
    }

    /// Marcar actividad de generación (llamado por el server en cada chat request).
    pub fn touch_activity(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_activity.store(now, Ordering::Relaxed);
    }

    pub fn subscribe_logs(&self) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        self.log_senders.lock().push(tx);
        rx
    }

    /// Historial bufferizado con seq (para el Cliente SSE que conecta tarde).
    pub fn get_log_events(&self) -> Vec<LogEvent> {
        self.recent_logs.read().iter().cloned().collect()
    }

    pub fn get_recent_logs(&self) -> Vec<String> {
        self.recent_logs
            .read()
            .iter()
            .map(|l| l.line.clone())
            .collect()
    }

    pub fn log(&self, msg: &str) {
        let seq = self.log_seq.fetch_add(1, Ordering::Relaxed);
        {
            let mut logs = self.recent_logs.write();
            if logs.len() >= 250 {
                logs.pop_front();
            }
            logs.push_back(LogEvent {
                seq,
                line: msg.to_string(),
            });
        }

        let mut senders = self.log_senders.lock();
        senders.retain(|tx| tx.send(msg.to_string()).is_ok());
        // Archivo con rotación (P30): best-effort, nunca falla al llamador.
        crate::filelog::write_log_line(&self.log_file, msg);
    }

    /// Error que impide una operación (`[ERROR]` en disco; en memoria igual
    /// que `log` para no romper los filtros de la UI).
    pub fn log_error(&self, msg: &str) {
        self.log(msg);
        crate::filelog::write_error(&self.log_file, msg);
    }

    /// Degradación no bloqueante (`[WARN]` en disco).
    pub fn log_warn(&self, msg: &str) {
        self.log(msg);
        crate::filelog::write_warn(&self.log_file, msg);
    }

    pub fn get_status(&self) -> ServerStatus {
        let mut st = self.status.read().clone();
        let timeout = self.config.get().engine.idle_timeout_secs;
        if timeout > 0 && st.status == "running" {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let last = self.last_activity.load(Ordering::Relaxed);
            if last > 0 {
                let elapsed = now.saturating_sub(last);
                st.idle_remaining_secs = Some(timeout.saturating_sub(elapsed));
            }
        }
        // Progreso en vivo sin tocar el poller (el poller también lo actualiza).
        if st.status == "starting" {
            let elapsed = self
                .start_instant
                .lock()
                .as_ref()
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            st.starting_for_secs = elapsed;
            let stored = self
                .load_times
                .lock()
                .get(&load_key(&st.model, st.context))
                .copied();
            st.eta_secs = eta_secs(stored, Self::model_size_gb(&self.models_dir, &st.model));
        } else {
            st.starting_for_secs = 0;
            st.eta_secs = 0;
        }
        st
    }

    pub fn detect_hardware(&self) -> HardwareInfo {
        let cpu_cores = num_cpus();
        let cpu_name = std::env::var("PROCESSOR_IDENTIFIER")
            .unwrap_or_else(|_| format!("CPU x86_64 ({} hilos)", cpu_cores));

        let mut gpus = Vec::new();
        let mut device_vram_total_mb = None;
        let selected_device = self.config.get().engine.device;
        let llama_bin = self.bin_dir.join("llama-server.exe");
        if llama_bin.exists() {
            if let Some(text) = crate::engine_gate::llama_device_list(&llama_bin) {
                device_vram_total_mb =
                    crate::engine_gate::parse_llama_device_memory(&text, &selected_device)
                        .map(|memory| memory.total_mb);
                for line in text.lines() {
                    let line = line.trim();
                    if line.contains(':') && !line.starts_with("Available") {
                        let mut parts = line.splitn(2, ':');
                        let dev_id = parts.next().unwrap_or("").trim().to_string();
                        let rest = parts.next().unwrap_or("").trim();
                        let (name, vram) = if let Some(idx) = rest.rfind('(') {
                            let n = rest[..idx].trim().to_string();
                            let v = rest[idx + 1..].trim_end_matches(')').trim().to_string();
                            (n, v)
                        } else {
                            (rest.to_string(), "Desconocido".to_string())
                        };
                        gpus.push(GpuDevice {
                            id: dev_id,
                            name,
                            vram,
                        });
                    }
                }
            }
        }
        // D-21: consumo REAL de VRAM (Windows): `nvidia-smi` si hay NVIDIA.
        // Sin NVIDIA (AMD/Intel) no hay contador de USO barato y estable desde
        // aquí y no se inventa. El WMI `AdapterRAM` solo se usa como último
        // recurso (sin dato del motor): si `--list-devices` ya trae la VRAM
        // (texto con "MiB"/"GB"/"MB"), NO se anexa nada de WMI — sus drivers
        // suelen saturar el uint32 (p. ej. 4095 MB falsos en una RX 6800 XT
        // de 16 GB) y ensucia el dato real. Best-effort: sin dato se deja el
        // texto del motor intacto.
        if let Some(used) = vram_used_mb() {
            for (i, g) in gpus.iter_mut().enumerate() {
                if i == 0 {
                    g.vram = format!("{} (en uso ~{} MB)", g.vram, used);
                }
            }
        } else if gpus.is_empty() {
            // Sin --list-devices (motor ausente): al menos el total WMI.
            if let Some(total) = vram_total_mb() {
                gpus.push(GpuDevice {
                    id: "gpu0".to_string(),
                    name: "GPU (WMI)".to_string(),
                    vram: format!("{} MB instalados (uso no disponible en AMD/Intel)", total),
                });
            }
        }
        // Con dato del motor: nada de WMI (evita el "4095 MB instalados" falso).
        HardwareInfo {
            cpu_cores,
            cpu_name,
            gpus,
            cpu_physical: physical_cores(cpu_cores),
            ram_total_mb: ram_total_mb(),
            vram_total_mb: device_vram_total_mb.or_else(crate::engine_gate::vram_total_mb),
        }
    }
    /// Importar un `.gguf` a `models/` con `overwrite` opt-in. Delega la
    /// resolución+copia en la primitiva compartida de `models`
    /// (`resolve_import_dest` + `copy_atomic_tmp`, wave-4): mismas garantías
    /// en ambas vías de import (self-import no-op, tmp+rename sin parciales).
    /// El `import_lock` serializa check+copy+rename del proceso (TOCTOU
    /// 2026-10-10); `Exists` → el handler lo traduce a 409 por VARIANTE
    /// (ABIERTO-1 wave-6: nunca por substring).
    pub fn import_model_from_path(
        &self,
        source_path: &std::path::Path,
        overwrite: bool,
    ) -> Result<String, crate::models::ImportError> {
        let _guard = self.import_lock.lock();
        if !source_path.exists() {
            return Err(crate::models::ImportError::Invalid(format!(
                "Archivo no encontrado: {:?}",
                source_path
            )));
        }
        let (filename, dest, self_import) =
            crate::models::resolve_import_dest(&self.models_dir, source_path, overwrite)?;
        if self_import {
            return Ok(filename);
        }
        crate::models::copy_atomic_tmp(source_path, &dest)?;
        self.log(&format!("[LocalMind] Modelo importado: {}", filename));
        Ok(filename)
    }

    /// Vía `POST /api/models/import_pick` (ABIERTO-1 wave-5): mismo
    /// `import_lock` que `import_model_from_path`, así que pick-vs-pick y
    /// pick-vs-import_model no violan el 409 (`overwrite=false`).
    pub fn import_picked(
        &self,
        files: &[std::path::PathBuf],
        overwrite: bool,
    ) -> Result<Vec<String>, crate::models::ImportError> {
        let _guard = self.import_lock.lock();
        crate::models::copy_picked(&self.models_dir, files, overwrite)
    }

    pub fn models_dir(&self) -> &std::path::Path {
        &self.models_dir
    }

    /// Foto de la config viva (para avisos fuera del poller, p. ej. bandeja).
    pub fn config_snapshot(&self) -> crate::config::AppConfig {
        self.config.get()
    }

    /// Ruta del log con rotación (para el `log_on_fail` de los avisos).
    pub fn log_file_path(&self) -> std::path::PathBuf {
        self.log_file.clone()
    }

    #[cfg(test)]
    pub fn list_models(&self) -> Vec<ModelInfo> {
        let cfg = self.config.get();
        self.list_models_with_aliases(&cfg.engine.aliases)
    }

    fn list_models_with_aliases(&self, aliases: &[String]) -> Vec<ModelInfo> {
        let mut models = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.models_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                // `file_name()` es `None` solo en `..`; se salta igual que el
                // no-archivo de arriba en vez de arriesgar un `unwrap`.
                let Some(fname) = path.file_name() else {
                    continue;
                };
                let name = fname.to_string_lossy().to_string();
                if name.to_lowercase().ends_with(".gguf") && !name.to_lowercase().contains("mmproj")
                {
                    let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    let size_gb = (size_bytes as f64) / (1024.0 * 1024.0 * 1024.0);
                    models.push(ModelInfo {
                        filename: name.clone(),
                        name: name.trim_end_matches(".gguf").to_string(),
                        size_gb: (size_gb * 100.0).round() / 100.0,
                        path: path.to_string_lossy().to_string(),
                    });
                }
            }
        }

        // Ordenar: primero los modelos cuyo nombre contiene un alias configurado.
        models.sort_by(|a, b| {
            let arank = aliases
                .iter()
                .position(|al| a.filename.to_lowercase().contains(&al.to_lowercase()))
                .unwrap_or(usize::MAX);
            let brank = aliases
                .iter()
                .position(|al| b.filename.to_lowercase().contains(&al.to_lowercase()))
                .unwrap_or(usize::MAX);
            match arank.cmp(&brank) {
                std::cmp::Ordering::Equal => a.filename.cmp(&b.filename),
                o => o,
            }
        });

        models
    }

    /// Elegir mmproj desde la misma foto de config usada por el arranque.
    fn find_mmproj(&self, cfg: &crate::config::AppConfig) -> Option<PathBuf> {
        if !cfg.mmproj.auto {
            return None;
        }
        cfg.mmproj
            .files
            .iter()
            .map(|f| self.models_dir.join(f))
            .find(|p| p.exists())
    }

    pub fn start(&self, req: StartRequest) -> Result<u32, String> {
        // Lock global de arranque (P1-seg): hace atómico el check→spawn. El
        // gateway atiende cada request en su hilo, así que dos `POST
        // /api/start` concurrentes llegaban juntos a validar y lanzar dos
        // hijos. `try_lock` (no bloqueante): el segundo recibe
        // `Err("...en curso...")` en vez de encolar y duplicar el motor. Se
        // toma ANTES de tocar cualquier estado y se sostiene hasta volver
        // (`_guard` vive toda la fn); los rechazos tempranos también quedan
        // serializados, que es lo correcto: el epoch y la pizarra se mueven
        // por intento. Sin pánicos: `try_lock` devuelve `None` si ocupado.
        let _guard = self.start_lock.try_lock().ok_or_else(|| {
            "Ya hay un arranque en curso: esperá a que termine antes de reintentar.".to_string()
        })?;
        if let Some(error) = &self.start_history_load_error {
            let msg = format!(
                "Protección de energía: no se pudo leer el historial de arranques; se bloquea este inicio ({})",
                error
            );
            self.log(&format!("[LocalMind] {}", msg));
            return Err(msg);
        }
        // Intento nuevo = pizarra de error limpia, y se limpia AL ABRIRLO, no
        // solo al lograrse (el `= None` de más abajo, tras el `spawn()`): una
        // validación rechazada devuelve ANTES de `stop()` y del `spawn()`, así
        // que sin esto el `last_error` del arranque ANTERIOR sobrevivía y el
        // poller de la UI lo volvía a pintar en el banner, pisando el mensaje
        // "Error al iniciar: ..." recién escrito con uno de un evento que el
        // usuario no acababa de provocar. `stop()` tampoco limpia `last_error`.
        // Los tres campos van juntos porque los lee la misma rama del banner:
        // un veredicto viejo de la puerta pisaría el banner igual de solo.
        {
            let mut st = self.status.write();
            st.last_error = None;
            st.acceptance_error = None;
            st.acceptance_ok = None;
        }
        // Un intento —incluso uno que vaya a ser RECHAZADO— abre una
        // generación nueva. La puerta de aceptación del intento ANTERIOR puede
        // seguir en vuelo (hasta ~120 s de probe HTTP) y, sin esto, su veredicto
        // aterrizaba DESPUÉS del `= None` de arriba y volvía a pintar la
        // pizarra con un `last_error`/`acceptance_error` que el rechazo acababa
        // de borrar. El poller ya sabe descartar esos veredictos: al terminar
        // la puerta compara el epoch vigente contra el suyo, así que moverlo
        // ANTES de validar deja esa puerta con el veredicto muerto.
        // Solo cambia el camino que `stop()` y el `spawn()` ya cubrían: ellos
        // matan el motor viejo, pero las validaciones que rechazan devuelven
        // ANTES de `stop()` (ver el orden en el resto de esta fn), y eran las
        // que dejaban la puerta viva.
        self.start_epoch.fetch_add(1, Ordering::Relaxed);
        // Validación del request explícito (nunca sustituir en silencio lo que
        // el usuario pidió: D-1/contexto fantasma). Solo valida CAMPOS
        // NOMBRADOS; omitidos resuelven por la precedencia habitual. Va ANTES
        // del guardarraíl para no consumir cooldown/tope con un typo.
        let cfg = self.config.get();
        let valid_ids: Vec<String> = cfg.profiles.iter().map(|p| p.id.clone()).collect();
        validate_req_profile(req.profile.as_deref(), &valid_ids)?;
        validate_req_context(req.context)?;
        let models = self.list_models_with_aliases(&cfg.engine.aliases);
        let filenames: Vec<String> = models.iter().map(|m| m.filename.clone()).collect();
        let names: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
        let rels: Vec<String> = models
            .iter()
            .filter_map(|m| {
                std::path::Path::new(&m.path)
                    .strip_prefix(self.models_dir.clone())
                    .ok()
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
            })
            .collect();
        let mut known_models = filenames;
        known_models.extend(names);
        known_models.extend(rels);
        validate_req_model(
            req.model.as_deref(),
            &known_models,
            &[],
            &cfg.engine.aliases,
        )?;

        // Rechazar falta de RAM antes de detener un motor que ya sirve
        // peticiones. El mínimo estima páginas/buffers del GGUF para mmap
        // y conserva 2 GiB para Windows. Solo opciones conocidas que
        // retienen pesos en CPU reservan el GGUF completo. `-kvu` y
        // `--cache-ram` no se confunden con ubicación de pesos: el segundo
        // es un tope de crecimiento posterior del prompt cache.
        let profile = if let Some(id) = req.profile.as_ref().filter(|s| !s.is_empty()) {
            crate::config::resolve_profile(&cfg.profiles, id)
        } else if let Some(last) = cfg.last.profile.as_ref().filter(|s| !s.is_empty()) {
            crate::config::resolve_profile(&cfg.profiles, last)
        } else {
            cfg.profiles.first().cloned().unwrap_or_else(|| {
                crate::config::resolve_profile(&cfg.profiles, crate::config::DEFAULT_PROFILE_ID)
            })
        };
        let profile_id = profile.id.clone();
        let power_safe_on = cfg.engine.power_safe;
        let model_filename = resolve_model_filename(
            req.model.as_deref(),
            &cfg.engine.aliases,
            &models,
            cfg.last.model.as_deref(),
            &self.models_dir,
        )?;
        let model_path = resolve_model_path(&self.models_dir, &model_filename)?;
        let req_profile_opt = req
            .profile
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| s.as_str());
        let context = resolve_context(
            req.context,
            req_profile_opt,
            profile.context,
            cfg.last.profile.as_deref(),
            cfg.last.model.as_deref(),
            cfg.last.context,
            &profile.id,
            &model_filename,
        );
        let mmproj_path = self.find_mmproj(&cfg);
        let model_mb = std::fs::metadata(&model_path)
            .map_err(|e| {
                format!("No se pudo leer el tamaño del GGUF para el prechequeo de RAM: {e}")
            })?
            .len()
            .div_ceil(1024 * 1024);
        let all_extra_flags: Vec<String> = profile
            .extra_flags
            .iter()
            .chain(cfg.engine.extra_flags.iter())
            .cloned()
            .collect();
        // Rechazar opciones inseguras antes de detener el motor actual. La
        // comprobación vuelve a usar la misma foto de perfil y engine que
        // resolverá el comando de arranque.
        if let Some(flag) = crate::config::cache_type_override_flag(&all_extra_flags) {
            let err = format!(
                "Flag bloqueada: «{}» intenta cambiar el tipo KV fijo q4_0.",
                flag
            );
            self.log(&err);
            return Err(err);
        }
        if let Some(err) = crate::config::psu_unsafe_flag(&all_extra_flags) {
            self.log(&err);
            return Err(err);
        }
        let host_load_mode = crate::engine_gate::host_model_load_mode(&all_extra_flags);
        let prompt_cache_mb = crate::engine_gate::effective_prompt_cache_mb(
            &all_extra_flags,
            profile.cache_ram as u64,
        );
        if let Some(available_mb) = crate::engine_gate::available_ram_mb() {
            match crate::engine_gate::check_host_ram_fit(
                    model_mb,
                    host_load_mode,
                    available_mb,
                ) {
                    crate::engine_gate::HostRamFit::Fits {
                        free_mb,
                        required_mb,
                    } => self.log(&format!(
                        "[LocalMind] RAM previa suficiente: {} MiB libres; mínimo de inicio estimado {} MiB (GGUF {} MiB; modo de carga: {}; margen Windows: 2048 MiB). Límite de caché de prompts: {} MiB; puede crecer durante el uso.",
                        free_mb,
                        required_mb,
                        model_mb,
                        match host_load_mode {
                            crate::engine_gate::HostModelLoadMode::Mmap => "auto/mmap",
                            crate::engine_gate::HostModelLoadMode::NoMmap => "sin mmap/direct IO",
                            crate::engine_gate::HostModelLoadMode::HostResident => "pesos en host",
                        },
                        prompt_cache_mb
                    )),
                    crate::engine_gate::HostRamFit::Insufficient {
                        free_mb,
                        required_mb,
                    } => {
                        let msg = format!(
                            "Inicio bloqueado por protección de RAM: hay {} MiB disponibles y el mínimo estimado para este modo de carga es {} MiB (incluye 2 GiB para Windows). La caché de prompts puede crecer después del arranque y el uso KV depende del contexto y la VRAM.",
                            free_mb, required_mb
                        );
                        self.log(&msg);
                        return Err(msg);
                    }
                }
        } else {
            self.log("[LocalMind] No se pudo consultar la RAM física libre; continúa el prechequeo sin esa estimación.");
        }
        // Guardarraíles de energía (LM-NF-3): cooldown + tope horario + modo
        // seguro. Cada arranque lee ~13 GB a VRAM (el transitorio más grande
        // del sistema); se evalúan ANTES de tocar el motor en marcha.
        let now = unix_now_secs();
        if let Err(wait) = check_start_guard(
            cfg.engine.start_cooldown_secs,
            cfg.engine.max_starts_per_hour,
            &mut self.start_history.lock(),
            now,
        ) {
            let msg = start_guard_error(wait);
            self.log(&msg);
            return Err(msg);
        }
        self.stop();

        // Brief delay to allow Windows to clear any TIME_WAIT TCP sockets
        thread::sleep(Duration::from_millis(150));

        // `profile_id` es siempre un id real (resuelto arriba): nunca se
        // persiste ni se expone el fantasma (D-1) aunque el request o el TOML
        // traigan un id desconocido.
        // Modo seguro (LM-NF-3, SIN restringir contexto: el dueño trabaja
        // siempre en 128K-262K). El aviso de 262K se emite tras resolver el
        // contexto real (ver `power_note_262k` abajo).
        // Estimación best-effort con el GGUF y la memoria libre del dispositivo
        // elegido. `--cache-ram` no interviene: limita la caché de prompts en RAM.
        // Es una advertencia, no un bloqueo: llama.cpp puede dejar capas en CPU;
        // la puerta posterior comprueba que el servidor realmente responda.
        let llama_bin = self.bin_dir.join("llama-server.exe");
        let device_memory = crate::engine_gate::llama_device_memory(&llama_bin, &cfg.engine.device);
        // Las flags extra se anexan después de los argumentos controlados y
        // podrían cambiar modelo, contexto, dispositivo u offload. `-kvu` no
        // cambia el tamaño de los pesos ni el KV q4_0 estimado; el resto deja
        // la estimación en desconocido.
        let estimate_is_complete =
            crate::engine_gate::vram_estimate_is_complete(&all_extra_flags, mmproj_path.is_some());
        let estimate = if !estimate_is_complete && mmproj_path.is_some() {
            crate::engine_gate::VramFit::Unknown(
                "Hay un mmproj activo y esta estimación solo contempla los pesos GGUF y el KV.",
            )
        } else if !estimate_is_complete {
            crate::engine_gate::VramFit::Unknown(
                "Hay flags extra que pueden cambiar la asignación efectiva de memoria.",
            )
        } else {
            crate::engine_gate::estimate_vram_fit(
                &model_path,
                context,
                device_memory.map(|memory| memory.free_mb),
            )
        };
        match estimate {
            crate::engine_gate::VramFit::Fits {
                model_mb,
                kv_mb,
                required_mb,
                free_mb,
            } => {
                self.log(&format!("[LocalMind] Estimación VRAM {}: pesos ~{} MiB + KV q4_0 ~{} MiB = ~{} MiB; {} informa {} MiB libres.", cfg.engine.device, model_mb, kv_mb, required_mb, cfg.engine.device, free_mb));
            }
            crate::engine_gate::VramFit::Exceeds {
                model_mb,
                kv_mb,
                required_mb,
                free_mb,
            } => {
                self.log(&format!("[LocalMind] Aviso VRAM {}: pesos ~{} MiB + KV q4_0 ~{} MiB = ~{} MiB frente a {} MiB libres. Se deja continuar porque el motor puede repartir capas en CPU.", cfg.engine.device, model_mb, kv_mb, required_mb, free_mb));
            }
            crate::engine_gate::VramFit::Unknown(reason) => {
                self.log(&format!(
                    "[LocalMind] Estimación VRAM no disponible: {}",
                    reason
                ));
            }
        }
        let threads = req
            .threads
            .clone()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<usize>()
                    .unwrap_or_else(|_| cfg.engine.threads.unwrap_or_else(default_engine_threads))
            })
            .unwrap_or_else(|| cfg.engine.threads.unwrap_or_else(default_engine_threads));
        let threads_batch = cfg
            .engine
            .threads_batch
            .unwrap_or(if cfg.engine.limit_threads_batch {
                threads.min(4) // prompt batch: el pico de CPU; limitado para no disparar PSU/VRAM
            } else {
                threads
            });
        let priority = req
            .priority
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cfg.engine.priority.clone());
        let process_priority_class = match cfg.engine.process_priority.to_lowercase().as_str() {
            "low" => 0x00000040u32, // IDLE_PRIORITY_CLASS
            "normal" => 0x00000020, // NORMAL_PRIORITY_CLASS
            _ => 0x00004000,        // BELOW_NORMAL_PRIORITY_CLASS
        };

        let llama_bin = self.bin_dir.join("llama-server.exe");
        if !llama_bin.exists() {
            return Err(format!("No se encontró llama-server en {:?}", llama_bin));
        }
        // LM-NF-6 / P29: una sola clave para gateway y motor.
        let api_key = crate::auth::gateway_key();

        // Puerto dinámico del motor: preferido desde config; si está ocupado, +1 hasta libre.
        let llama_port = Self::find_free_port(cfg.engine.llama_port);

        // Performance tuning seguro: uBatch fijado en 512 para evitar transitorios de energía
        let ubatch = "512";
        let mut cmd = Self::build_engine_cmd(
            &llama_bin,
            &self.base_dir,
            process_priority_class,
            &model_path,
            context,
            threads,
            threads_batch,
            &priority,
            &cfg.engine,
            &profile,
            llama_port,
            self.mtp_retry_done
                .load(std::sync::atomic::Ordering::Relaxed),
            api_key,
        );

        // D-47: `build_engine_cmd` YA anexa `--reasoning-preserve`,
        // `--cache-ram` y todas las `extra_flags` (perfil + engine). Este bloque
        // los repetía y el hijo los recibía dos veces
        // (`DEPRECATED: argument '--reasoning-preserve' specified multiple
        // times`), con el efecto peor que el ruido: el argv del log dejaba de
        // ser el argv del hijo, que es justo lo que lo hace evidencia.
        // No se reimplementan aquí: la fuente única es `build_engine_cmd`.
        //
        // `--cache-reuse` NO se pasa: 10683 lo rechazó en este contexto,
        // mientras que 10743 ya lo ofrece; falta validar su efecto con Qwen3.8
        // y este tamaño de contexto. Mantenerlo fuera del argv evita cambiar el
        // motor antes de tener una comparación A/B (ver `config.rs`).

        if let Some(mm) = mmproj_path.as_ref() {
            let mm_arg = mm.to_string_lossy().to_string();
            cmd.args(["--mmproj", mm_arg.as_str()]);
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        self.log("================================================================");
        self.log(&format!("[LocalMind] Cargando modelo: {}", model_filename));
        self.log(&format!("[LocalMind] Perfil: {}", profile.name));
        self.log(&Self::start_banner(context, threads, ubatch, llama_port));
        // Aviso power_safe en 262K (SIN restringir: el dueño trabaja en
        // 128K-262K siempre). Visible en log/SSE/archivo P30.
        if power_safe_on && context >= 262144 {
            self.log("Contexto 262K: consumo y picos más altos; arranques limitados (cooldown 120 s + tope 4/hora). Para liberar VRAM usá Stop manual.");
        }
        self.log("================================================================");

        // Persistir ANTES de crear el hijo: si OMNI cae justo después del
        // spawn, el siguiente proceso debe seguir viendo este arranque. El
        // segundo chequeo usa el instante real del spawn y el write atómico
        // falla cerrado; nunca se carga el modelo sin actualizar el guard.
        let start_now = unix_now_secs();
        {
            let mut history = self.start_history.lock();
            if let Err(wait) = check_start_guard(
                cfg.engine.start_cooldown_secs,
                cfg.engine.max_starts_per_hour,
                &mut history,
                start_now,
            ) {
                let msg = start_guard_error(wait);
                self.log(&msg);
                return Err(msg);
            }
            let mut next_history = history.clone();
            drop(history);
            record_start(&mut next_history, start_now);
            if let Err(error) = persist_start_history(&self.start_history_file, &next_history) {
                let msg = format!(
                    "Protección de energía: no se pudo guardar el historial; el motor no se inició ({})",
                    error
                );
                self.log(&format!("[LocalMind] {}", msg));
                return Err(msg);
            }
            *self.start_history.lock() = next_history;
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Error al iniciar llama-server: {}", e))?;
        let pid = child.id();

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let logs_clone = Arc::clone(&self.recent_logs);
        let senders_clone = Arc::clone(&self.log_senders);
        let seq_clone = Arc::clone(&self.log_seq);
        let file_clone = self.log_file.clone();
        if let Some(out) = stdout {
            let l_c = Arc::clone(&logs_clone);
            let s_c = Arc::clone(&senders_clone);
            let q_c = Arc::clone(&seq_clone);
            let f_c = file_clone.clone();
            thread::spawn(move || {
                let reader = BufReader::new(out);
                for line in reader.lines().map_while(Result::ok) {
                    let seq = q_c.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut logs = l_c.write();
                        if logs.len() >= 250 {
                            logs.pop_front();
                        }
                        logs.push_back(LogEvent {
                            seq,
                            line: line.clone(),
                        });
                    }
                    let mut s = s_c.lock();
                    s.retain(|tx| tx.send(line.clone()).is_ok());
                    // Archivo con rotación (P30): best-effort.
                    crate::filelog::write_log_line(&f_c, &line);
                }
            });
        }

        if let Some(err) = stderr {
            let l_c = Arc::clone(&logs_clone);
            let s_c = Arc::clone(&senders_clone);
            let q_c = Arc::clone(&seq_clone);
            let f_c = file_clone.clone();
            thread::spawn(move || {
                let reader = BufReader::new(err);
                for line in reader.lines().map_while(Result::ok) {
                    let seq = q_c.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut logs = l_c.write();
                        if logs.len() >= 250 {
                            logs.pop_front();
                        }
                        logs.push_back(LogEvent {
                            seq,
                            line: line.clone(),
                        });
                    }
                    let mut s = s_c.lock();
                    s.retain(|tx| tx.send(line.clone()).is_ok());
                    // Archivo con rotación (P30): best-effort.
                    crate::filelog::write_log_line(&f_c, &line);
                }
            });
        }

        *self.child.lock() = Some(child);
        // Arranque fresco: el reintento MTP vuelve a estar disponible, y los
        // parámetros RESUELTOS quedan guardados para el reintento (nunca
        // `st.context`/`st.model`: `[last]` se persiste abajo, stale).
        self.mtp_retry_done.store(false, Ordering::Relaxed);
        *self.pending_start.lock() = Some(ResolvedStart {
            model_filename: model_filename.clone(),
            model_path: model_path.clone(),
            profile: profile.clone(),
            profile_id: profile_id.clone(),
            context,
            threads,
            threads_batch,
            priority: priority.clone(),
            process_priority_class,
            llama_port,
            mmproj: mmproj_path.clone(),
        });
        {
            let mut st = self.status.write();
            st.status = "starting".to_string();
            st.is_healthy = false;
            st.pid = Some(pid);
            st.model = model_filename.clone();
            st.context = context;
            st.profile = profile_id.clone();
            st.port = llama_port;
            st.last_error = None;
            // Estado inicial de la puerta de aceptación (D1) y del progreso (D4).
            st.verifying = false;
            st.starting_for_secs = 0;
            let stored = self
                .load_times
                .lock()
                .get(&load_key(&model_filename, context))
                .copied();
            st.eta_secs = eta_secs(
                stored,
                Self::model_size_gb(&self.models_dir, &model_filename),
            );
            st.request_tps = None;
            st.request_tps_samples = Vec::new();
            st.decode_tps = None;
            st.decode_tps_samples = Vec::new();
            st.engine_slow = false;
            st.engine_slow_known = false;
            st.acceptance_ok = None;
        }
        *self.start_instant.lock() = Some(Instant::now());
        // Nuevo arranque: invalida cualquier puerta de aceptación en curso (D1).
        self.start_epoch.fetch_add(1, Ordering::Relaxed);
        // Sembrar el timer de inactividad al arrancar (D-20): sin esto
        // `last_activity` queda en 0 y el auto-stop por inactividad no dispara nunca.
        self.touch_activity();
        // Persistir "última configuración usada" para el próximo arranque.
        self.config.update(|c| {
            c.last = crate::config::LastSettings {
                model: Some(model_filename.clone()),
                profile: Some(profile_id.clone()),
                context: Some(context),
            };
            // (modelo/perfil ya se movieron arriba; usar referencias)
        });
        let _ = self.config.save();

        Ok(pid)
    }

    pub fn stop(&self) {
        {
            let mut child_lock = self.child.lock();
            if let Some(mut c) = child_lock.take() {
                let pid = c.id();
                // Kill del árbol del proceso: tty hijos heredados (tokenizer, etc.)
                let mut tk = Command::new("taskkill");
                tk.args(["/F", "/T", "/PID", &pid.to_string()]);
                tk.creation_flags(CREATE_NO_WINDOW);
                let _ = tk.output();
                let _ = c.kill();
                let _ = c.wait();
            }
        }

        let mut st = self.status.write();
        st.status = "stopped".to_string();
        st.is_healthy = false;
        st.pid = None;
        st.verifying = false;
        st.starting_for_secs = 0;
        st.eta_secs = 0;
        st.request_tps = None;
        st.request_tps_samples = Vec::new();
        st.decode_tps = None;
        st.decode_tps_samples = Vec::new();
        st.engine_slow = false;
        st.engine_slow_known = false;
        st.acceptance_ok = None;
        st.acceptance_error = None;
        *self.start_instant.lock() = None;
        // Invalida cualquier puerta de aceptación en curso (D1).
        self.start_epoch.fetch_add(1, Ordering::Relaxed);
        // El próximo arranque tiene su propio reintento MTP disponible.
        self.mtp_retry_done.store(false, Ordering::Relaxed);
        *self.pending_start.lock() = None;
        self.log("[LocalMind] Proceso del motor detenido y finalizado.");
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        self.running_poll.store(false, Ordering::Relaxed);
        self.stop();
    }
}
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(6)
}

/// Por defecto se usan núcleos físicos estimados, no todos los hilos lógicos.
/// La carga del prompt es costosa en RAM/CPU y el límite anterior duplicaba
/// hilos en equipos con SMT (p. ej. Ryzen 6C/12T), aunque la UI documentaba 6.
fn default_engine_threads() -> usize {
    default_engine_threads_from_logical(num_cpus())
}

fn default_engine_threads_from_logical(logical: usize) -> usize {
    physical_cores(logical).unwrap_or(6)
}

/// Núcleos físicos estimados (portabilidad): la mayoría x86 trae SMT×2, así
/// que lógicos/2 con mínimo 1. Mejor que copiar el 6 del dueño: en un i3 de
/// 4 hilos sugiere 2, en un 7950X de 32 sugiere 16. `None` si no hay dato.
fn physical_cores(logical: usize) -> Option<usize> {
    if logical == 0 {
        return None;
    }
    Some((logical / 2).max(1))
}

/// RAM total del sistema en MB (Windows, best-effort): WMI
/// `Win32_ComputerSystem.TotalPhysicalMemory`. `None` sin dato.
fn ram_total_mb() -> Option<u64> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let out = std::process::Command::new("wmic")
        .args(["computersystem", "get", "totalphysicalmemory", "/value"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let v = line.split('=').nth(1).unwrap_or("").trim();
        let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u64>() {
            if n >= 512 * 1024 * 1024 {
                return Some(n / (1024 * 1024));
            }
        }
    }
    None
}
// (extraídos a `engine_gate.rs`: `vram_used_mb`, `vram_total_mb`,
// `parse_adapter_ram`. Se usan como `crate::engine_gate::`.)

/// Resultado del guardarraíl de arranque (LM-NF-3): cuánto falta de cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartGuardWait {
    /// Segundos restantes de cooldown (0 = el tope horario es el que bloquea).
    pub cooldown_left: u64,
}

const START_HISTORY_MAX_ENTRIES: usize = 64;
const START_HISTORY_MAX_BYTES: u64 = 4096;

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn start_guard_error(wait: StartGuardWait) -> String {
    if wait.cooldown_left > 0 {
        format!(
            "Arranque demasiado pronto: esperá {} s antes de reintentar (protección de energía).",
            wait.cooldown_left
        )
    } else {
        "Límite de arranques por hora alcanzado (protección de energía): reintentá más tarde."
            .to_string()
    }
}

/// Leer el guard persistente. Un archivo inválido no se interpreta como una
/// ventana vacía: eso convertiría una corrupción en permiso para arrancar.
fn load_start_history(path: &Path, now: u64) -> Result<Vec<u64>, String> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!("lectura fallida: {}", error));
        }
    };
    if raw.len() as u64 > START_HISTORY_MAX_BYTES {
        return Err("archivo excede el tamaño esperado".to_string());
    }
    let mut history: Vec<u64> =
        serde_json::from_slice(&raw).map_err(|error| format!("contenido inválido: {}", error))?;
    if history.len() > START_HISTORY_MAX_ENTRIES {
        return Err("contiene más entradas de las permitidas".to_string());
    }
    let had_future_timestamp = history.iter().any(|timestamp| *timestamp > now);
    for timestamp in &mut history {
        *timestamp = (*timestamp).min(now);
    }
    history.retain(|timestamp| now - *timestamp < 3600);
    if had_future_timestamp {
        // Un reloj atrasado o un valor corrupto no debe bloquear arranques
        // hasta una fecha arbitraria. Normalizar a `now` conserva el cooldown
        // completo y deja corregido el archivo para próximos reinicios.
        persist_start_history(path, &history)?;
    }
    Ok(history)
}

/// Guardar en un archivo temporal y renombrar: una caída durante la escritura
/// deja intacto el historial anterior. El fallo se propaga para que `start()`
/// no cree el hijo sin haber registrado antes el transitorio de carga.
fn persist_start_history(path: &Path, history: &[u64]) -> Result<(), String> {
    if history.len() > START_HISTORY_MAX_ENTRIES {
        return Err("demasiadas entradas para persistir".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "la ruta no tiene directorio padre".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| format!("directorio: {}", error))?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec(history).map_err(|error| format!("serialización: {}", error))?;
    let write_result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|error| format!("temporal: {}", error))?;
        file.write_all(&bytes)
            .map_err(|error| format!("escritura: {}", error))?;
        file.sync_all()
            .map_err(|error| format!("flush: {}", error))?;
        std::fs::rename(&tmp, path).map_err(|error| format!("reemplazo: {}", error))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

/// Guardarraíl de arranques (LM-NF-3, puro y testeable): cooldown mínimo entre
/// arranques + tope por hora rodante. `history` son epoch-secs de arranques
/// previos (solo se podan entradas >1 h). Devuelve `Ok` y NO toca nada si
/// puede arrancar; `Err` con la espera restante si no. Sin pánicos.
pub fn check_start_guard(
    cooldown_secs: u64,
    max_per_hour: u32,
    history: &mut Vec<u64>,
    now: u64,
) -> Result<(), StartGuardWait> {
    for timestamp in history.iter_mut() {
        *timestamp = (*timestamp).min(now);
    }
    history.retain(|t| now - *t < 3600);
    if let Some(last) = history.iter().max() {
        let elapsed = now.saturating_sub(*last);
        if elapsed < cooldown_secs {
            return Err(StartGuardWait {
                cooldown_left: cooldown_secs - elapsed,
            });
        }
    }
    if max_per_hour > 0 && (history.len() as u32) >= max_per_hour {
        return Err(StartGuardWait { cooldown_left: 0 });
    }
    Ok(())
}

/// Registrar un arranque permitido (LM-NF-3). Acota el historial a 64
/// entradas para no crecer sin límite (D-8).
pub fn record_start(history: &mut Vec<u64>, now: u64) {
    history.push(now);
    if history.len() > 64 {
        let excess = history.len() - 64;
        history.drain(..excess);
    }
}
// ---------------------------------------------------------------------------
// Puerta de aceptación + crash legible + ETA (funciones puras, testeables)
// ---------------------------------------------------------------------------

/// Extraer `(prompt_tokens, completion_tokens)` del `usage` de una respuesta
/// OpenAI-compatible. Acepta JSON no-stream y el chunk final SSE (`data: {...}`).
fn parse_usage(body: &str) -> Option<(u64, u64)> {
    let trimmed = body.trim();
    // Camino SSE: última línea `data:` con JSON que traiga `usage`.
    if trimmed.contains("data:") {
        let mut found = None;
        for line in trimmed.lines() {
            let line = line.trim();
            let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
            if payload == "[DONE]" || payload.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                if v.get("usage").is_some() {
                    found = Some(v);
                }
            }
        }
        if let Some(v) = found {
            return usage_from_value(&v);
        }
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| usage_from_value(&v))
}

fn usage_from_value(v: &serde_json::Value) -> Option<(u64, u64)> {
    let usage = v.get("usage")?;
    let prompt = usage
        .get("prompt_tokens")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    if prompt == 0 && completion == 0 {
        return None;
    }
    Some((prompt, completion))
}

/// Lee el decode real informado por llama.cpp. `timings` sólo representa la
/// ejecución del motor; nunca se sustituye por tokens/request/tiempo total.
fn parse_engine_decode_tps(body: &str) -> Option<f64> {
    fn from_value(v: &serde_json::Value) -> Option<f64> {
        let timings = v.get("timings")?;
        let predicted = timings.get("predicted_n")?.as_u64()?;
        if predicted == 0 {
            return None;
        }
        let reported = timings
            .get("predicted_per_second")
            .and_then(|n| n.as_f64())
            .filter(|n| n.is_finite() && *n > 0.0);
        let computed = timings
            .get("predicted_ms")
            .and_then(|n| n.as_f64())
            .filter(|ms| ms.is_finite() && *ms > 0.0)
            .map(|ms| (predicted as f64) * 1000.0 / ms);
        reported.or(computed).filter(|n| n.is_finite() && *n > 0.0)
    }

    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return from_value(&value);
    }
    let mut found = None;
    for line in trimmed.lines() {
        let Some(payload) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
            if value.get("timings").is_some() {
                if let Some(tps) = from_value(&value) {
                    found = Some(tps);
                }
            }
        }
    }
    found
}

/// Veredicto de la puerta de aceptación: `>= 20` completion tokens y `>= 3` t/s
/// (redondeo hacia abajo). Devuelve el t/s medido o el motivo (`tokens`/`tps`).
fn acceptance_verdict(tokens: u64, ms: u64) -> Result<f64, String> {
    if tokens < 20 {
        return Err("tokens".to_string());
    }
    let tps = (tokens as f64) * 1000.0 / (ms.max(1) as f64);
    if tokens.saturating_mul(1000) / ms.max(1) < 3 {
        return Err("tps".to_string());
    }
    Ok(tps)
}

/// Mediana de las muestras de la puerta (pura y testeable): con 3 elige la
/// del medio; con 2 la menor (cota inferior: no inflar el reporte); vacía
/// → `None`. No decide el veredicto, solo resume.
fn gate_median(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if sorted.len() == 2 {
        return Some(sorted[0]);
    }
    Some(sorted[sorted.len() / 2])
}

/// Validar el `profile` explícito del request (puro): si se nombró uno que no
/// existe entre los perfiles vigentes → 400 con los ids válidos. `None`/vacío
/// (campo omitido) resuelve por la precedencia habitual, sin error.
fn validate_req_profile(req_profile: Option<&str>, valid_ids: &[String]) -> Result<(), String> {
    match req_profile.filter(|s| !s.is_empty()) {
        None => Ok(()),
        Some(id) if valid_ids.iter().any(|v| v == id) => Ok(()),
        Some(id) => Err(format!(
            "Perfil desconocido: '{}'. Válidos: {}.",
            id,
            valid_ids.join(", ")
        )),
    }
}

/// Validar el `context` explícito del request (puro): 1024..=1048576 en pasos
/// de 1024 (misma regla que `profile_context_ok`). Omitido → precedencia
/// habitual, sin error.
fn validate_req_context(req_ctx: Option<usize>) -> Result<(), String> {
    match req_ctx {
        None => Ok(()),
        Some(c) if crate::config::profile_context_ok(c) => Ok(()),
        Some(c) => Err(format!(
            "Contexto no válido: {}. Rango permitido: 1024..=1048576 en múltiplos de 1024.",
            c
        )),
    }
}

/// Validar el `model` explícito del request (puro): basename/`rel` listado,
/// `name` sin extensión, o alias configurado. Omitido → fallbacks habituales.
fn validate_req_model(
    req_model: Option<&str>,
    filenames: &[String],
    names: &[String],
    aliases: &[String],
) -> Result<(), String> {
    match req_model.filter(|s| !s.is_empty()) {
        None => Ok(()),
        Some(m) => {
            let hit_file = filenames.iter().any(|f| f == m);
            let hit_name = names.iter().any(|n| n == m);
            let hit_alias = aliases
                .iter()
                .any(|a| !a.is_empty() && m.to_lowercase().contains(&a.to_lowercase()));
            if hit_file || hit_name || hit_alias {
                Ok(())
            } else {
                Err(format!("Modelo desconocido: '{}'.", m))
            }
        }
    }
}

/// `engine_slow`, auto-stop, motor perdido, ETA, clave de duraciones y VRAM:
/// viven en `engine_gate.rs` (extraído de este fichero). Uso cualificado
/// `crate::engine_gate::`.
/// Firma MTP-no-soportado (pura y testeable): el motor murió en el arranque
/// porque el modelo no trae capas MTP (`creating MTP draft context` →
/// `model doesn't contain MTP layers` / `failed to create MTP context`).
/// Solo dispara el reintento dirigido sin spec (una vez, mismo arranque).
fn mtp_unsupported_signature(lines: &[String]) -> bool {
    let joined = lines.join("\n").to_lowercase();
    joined.contains("mtp")
        && (joined.contains("draft context")
            || joined.contains("doesn't contain mtp")
            || joined.contains("failed to create mtp"))
}

/// Decisión del reintento MTP dirigido (pura y testeable): una sola vez por
/// arranque (`retry_done`), solo en `starting`, solo con spec habilitada y
/// solo con la firma exacta. No toca presupuesto: el llamador NO registra en
/// el historial (mismo arranque lógico).
fn mtp_retry_decision(
    was_starting: bool,
    spec_on: bool,
    retry_done: bool,
    signature: bool,
) -> bool {
    was_starting && spec_on && !retry_done && signature
}

/// `n_ctx` desde el cuerpo de `/props` (puro y testeable): busca `n_ctx`
/// de nivel raíz o en `default_generation_settings.n_ctx`. Ausente o no
/// numérico → `None` (sin falso error: el llamador sigue sin verificar).
fn props_n_ctx(body: &str) -> Option<usize> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if let Some(n) = v.get("n_ctx").and_then(|x| x.as_u64()) {
        return usize::try_from(n).ok();
    }
    if let Some(n) = v
        .get("default_generation_settings")
        .and_then(|o| o.get("n_ctx"))
        .and_then(|x| x.as_u64())
    {
        return usize::try_from(n).ok();
    }
    None
}

// (extraídos a `engine_gate.rs`: `eta_secs`, `load_key`, `should_auto_stop`,
// `engine_lost`. Se usan como `crate::engine_gate::`.)

/// Despacho único de avisos P20 (Fase A5): los dos puntos del poller
/// (fallo de arranque y eventos en curso) hacían el mismo
/// `should_notify` + `spawn`. El llamador ya resolvió el flag del evento.
fn spawn_notify(
    title: String,
    body: String,
    tag: String,
    enabled: bool,
    flag_on: bool,
    log_file: std::path::PathBuf,
) {
    if !crate::notify::should_notify(enabled, flag_on) {
        return;
    }
    std::thread::spawn(move || {
        crate::notify::notify(&title, &body, &tag, |err| {
            crate::filelog::write_log_line(&log_file, err);
        });
    });
}

/// Resolver el modelo pedido contra `models_dir`: acepta el basename plano
/// (histórico, lo que manda la UI) o el `rel` de /api/models (`sub/m.gguf`).
/// Rechaza `..`, rutas absolutas, prefijo de unidad y escapes de `models_dir`.
/// Sin pánicos: todo fallo es `Err` en español estilo `start()`.
fn resolve_model_path(models_dir: &Path, requested: &str) -> Result<PathBuf, String> {
    let req = requested.trim();
    if req.is_empty() {
        return Err("No se encontró ningún modelo .gguf en la carpeta models/".to_string());
    }
    let norm = req.replace('\\', "/");
    if norm.starts_with('/') || norm.starts_with('\\') {
        return Err(format!("Ruta de modelo no válida: {:?}", requested));
    }
    if norm.contains(':') {
        return Err(format!("Ruta de modelo no válida: {:?}", requested));
    }
    if norm.split('/').any(|seg| seg == "..") {
        return Err(format!("Ruta de modelo no válida: {:?}", requested));
    }
    let candidate = models_dir.join(norm.replace('/', std::path::MAIN_SEPARATOR_STR));
    // Cinturón: aunque el join no debería escapar tras los filtros, verificarlo.
    let base = models_dir;
    if candidate != *base && !candidate.starts_with(base) {
        return Err(format!("Ruta de modelo no válida: {:?}", requested));
    }
    if !candidate.is_file() {
        return Err(format!("Archivo no encontrado: {:?}", candidate));
    }
    Ok(candidate)
}

/// Precedencia del MODELO al arrancar (puro y testeable, sin pánicos).
///
/// `last.model` va por ENCIMA del primer `.gguf` porque `models/` suele traer
/// varios y arrancar siempre con el primero ignora la sesión en curso: antes
/// iba detrás y era código muerto (`models.first()` es `Some` en cuanto hay un
/// modelo, luego la rama nunca se alcanzaba). Como `last.model` NO pasa por
/// `validate_req_model`, se guarda contra un archivo que ya no existe y degrada
/// al primer `.gguf` en vez de volver un arranque válido un error duro.
/// Orden: 1) `req_model` explícito; 2) alias configurado (una petición de
/// launcher también es explícita, por eso el alias gana a `last.model`);
/// 3) `last.model` de la sesión anterior; 4) primer `.gguf` de la carpeta.
fn resolve_model_filename(
    req_model: Option<&str>,
    aliases: &[String],
    models: &[ModelInfo],
    last_model: Option<&str>,
    models_dir: &Path,
) -> Result<String, String> {
    if let Some(m) = req_model.filter(|s| !s.is_empty()) {
        return Ok(m.to_string());
    }
    if let Some(hit) = aliases.iter().find_map(|al| {
        models
            .iter()
            .find(|m| m.filename.to_lowercase().contains(&al.to_lowercase()))
            .map(|m| m.filename.clone())
    }) {
        return Ok(hit);
    }
    if let Some(last) = last_model.filter(|s| !s.is_empty()) {
        if resolve_model_path(models_dir, last).is_ok() {
            return Ok(last.to_string());
        }
    }
    models
        .first()
        .map(|m| m.filename.clone())
        .ok_or_else(|| "No se encontró ningún modelo .gguf en la carpeta models/".to_string())
}

/// Precedencia de contexto al arrancar (un perfil explícito nunca pierde contra
/// una sesión vieja): 1) `req_ctx` explícito; 2) perfil explícito en el request
/// → contexto de ESE perfil; 3) sin perfil explícito y última sesión con el
/// mismo perfil+modelo → contexto guardado; 4) contexto del perfil resuelto.
/// CI (`-D warnings`): 8 args de precedencia documentada 1-4; struct oscurece.
#[allow(clippy::too_many_arguments)]
fn resolve_context(
    req_ctx: Option<usize>,
    req_profile: Option<&str>,
    profile_ctx: usize,
    last_profile: Option<&str>,
    last_model: Option<&str>,
    last_ctx: Option<usize>,
    resolved_profile: &str,
    model: &str,
) -> usize {
    if let Some(c) = req_ctx {
        return c;
    }
    if req_profile.filter(|s| !s.is_empty()).is_some() {
        return profile_ctx;
    }
    if let Some(saved) = last_ctx {
        let same_profile = last_profile.filter(|s| !s.is_empty()) == Some(resolved_profile);
        let same_model = match last_model.filter(|s| !s.is_empty()) {
            Some(m) => m == model,
            None => true,
        };
        if same_profile && same_model {
            return saved;
        }
    }
    profile_ctx
}

/// Resumen legible de un crash (D3): últimas ≤30 líneas no vacías, preferencia a
/// las que mencionan error/fallo/abort/bind/memoria/excepción; si ninguna
/// coincide, las últimas 5; todo truncado a 600 chars + código de salida.
fn crash_summary(lines: &[String], status: &str) -> String {
    let nonempty: Vec<&str> = lines
        .iter()
        .map(|l| l.as_str())
        .filter(|l| !l.trim().is_empty())
        .collect();
    if nonempty.is_empty() {
        return format!(
            "El servidor de IA se detuvo inesperadamente (código: {})",
            status
        );
    }
    let tail: Vec<&str> = nonempty.iter().rev().take(30).rev().copied().collect();
    let mut picked: Vec<&str> = tail
        .iter()
        .filter(|l| {
            let lower = l.to_lowercase();
            lower.contains("error")
                || lower.contains("failed")
                || lower.contains("abort")
                || lower.contains("couldn't bind")
                || lower.contains("out of")
                || lower.contains("exception")
        })
        .copied()
        .collect();
    if picked.is_empty() {
        picked = tail.iter().rev().take(5).rev().copied().collect();
    }
    let mut joined = picked.join(" | ");
    if joined.chars().count() > 600 {
        joined = joined.chars().take(600).collect();
    }
    format!("{} (código: {})", joined, status)
}

const MAX_LOAD_TIMES: usize = 64;

fn load_times_load(path: &Path) -> Result<HashMap<String, u64>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let mut map: HashMap<String, u64> = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    // Acotar lecturas de archivos ajenos/más grandes (D-8: buffer acotado).
    if map.len() > MAX_LOAD_TIMES {
        map = map.into_iter().take(MAX_LOAD_TIMES).collect();
    }
    Ok(map)
}

fn load_times_save(path: &Path, map: &HashMap<String, u64>) -> Result<(), String> {
    let mut capped: HashMap<String, u64> = HashMap::new();
    // Sin orden estable en HashMap: conservar una muestra acotada cualquiera.
    for (k, v) in map.iter().take(MAX_LOAD_TIMES) {
        capped.insert(k.clone(), *v);
    }
    let text = serde_json::to_string_pretty(&capped).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    // Escritura atómica: tmp + rename (D4).
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motor_usa_nucleos_fisicos_estimados_por_defecto() {
        assert_eq!(physical_cores(12), Some(6));
        assert_eq!(default_engine_threads_from_logical(12), 6);
        assert_eq!(default_engine_threads_from_logical(8), 4);
        assert_eq!(default_engine_threads_from_logical(1), 1);
        assert_eq!(default_engine_threads_from_logical(0), 6);
    }

    fn mi_model(file: &str) -> ModelInfo {
        ModelInfo {
            filename: file.to_string(),
            name: file.trim_end_matches(".gguf").to_string(),
            size_gb: 1.0,
            path: file.to_string(),
        }
    }

    /// models_dir scratch con .gguf reales (la guarda de `last.model` mira el
    /// disco, no solo la lista). `tag` mantiene el directorio propio de cada
    /// test: corren en paralelo y comparten PID.
    fn dir_con_modelos(tag: &str, files: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lm-modelres-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"gguf").unwrap();
        }
        dir
    }

    #[test]
    fn last_model_gana_al_primer_gguf() {
        // El bug: `last.model` estaba por detrás de `models.first()`, que es
        // `Some` siempre que haya un modelo → la rama era inalcanzable.
        let dir = dir_con_modelos("last-gana", &["aaa-2b.gguf", "bbb-27b.gguf"]);
        let models = vec![mi_model("aaa-2b.gguf"), mi_model("bbb-27b.gguf")];
        let got = resolve_model_filename(None, &[], &models, Some("bbb-27b.gguf"), &dir).unwrap();
        assert_eq!(got, "bbb-27b.gguf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_model_inexistente_cae_al_primer_gguf() {
        // `last.model` nunca se valida: si el archivo ya no está, degrada.
        let dir = dir_con_modelos("last-inexistente", &["aaa-2b.gguf", "bbb-27b.gguf"]);
        let models = vec![mi_model("aaa-2b.gguf"), mi_model("bbb-27b.gguf")];
        let got =
            resolve_model_filename(None, &[], &models, Some("borrado-7b.gguf"), &dir).unwrap();
        assert_eq!(got, "aaa-2b.gguf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alias_manda_sobre_last_model() {
        // El alias es petición explícita de un launcher: gana a `last.model`.
        let dir = dir_con_modelos("alias-gana", &["aaa-2b.gguf", "bbb-27b.gguf"]);
        let models = vec![mi_model("aaa-2b.gguf"), mi_model("bbb-27b.gguf")];
        let aliases = vec!["aaa".to_string()];
        let got =
            resolve_model_filename(None, &aliases, &models, Some("bbb-27b.gguf"), &dir).unwrap();
        assert_eq!(got, "aaa-2b.gguf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modelo_explicito_manda_sobre_todo() {
        let dir = dir_con_modelos("explicito", &["aaa-2b.gguf", "bbb-27b.gguf"]);
        let models = vec![mi_model("aaa-2b.gguf"), mi_model("bbb-27b.gguf")];
        let aliases = vec!["aaa".to_string()];
        let got = resolve_model_filename(
            Some("bbb-27b.gguf"),
            &aliases,
            &models,
            Some("aaa-2b.gguf"),
            &dir,
        )
        .unwrap();
        assert_eq!(got, "bbb-27b.gguf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sin_modelos_devuelve_error_en_espanol() {
        let dir = std::env::temp_dir().join(format!("lm-modelres-vacio-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let err = resolve_model_filename(None, &[], &[], Some("borrado.gguf"), &dir).unwrap_err();
        assert!(err.contains("No se encontró ningún modelo"), "{}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn contexto_guardado_se_restaura_si_el_modelo_es_el_mismo() {
        // Regla 3 de `resolve_context`. Estaba muerta en la práctica porque
        // `model` nunca coincidía con `last.model` (siempre ganaba el primer
        // .gguf o el alias); al arreglar la precedencia vuelve a aplicar.
        assert_eq!(
            resolve_context(
                None,
                None,
                32768,
                Some("libros"),
                Some("LFM2.5-2.6B-Q4_K_M.gguf"),
                Some(131072),
                "libros",
                "LFM2.5-2.6B-Q4_K_M.gguf",
            ),
            131072
        );
        // Mismo perfil, OTRO modelo → la sesión vieja no aplica.
        assert_eq!(
            resolve_context(
                None,
                None,
                32768,
                Some("libros"),
                Some("otro.gguf"),
                Some(131072),
                "libros",
                "LFM2.5-2.6B-Q4_K_M.gguf",
            ),
            32768
        );
        // Contexto explícito o perfil explícito ganan igual.
        assert_eq!(
            resolve_context(
                Some(65536),
                None,
                32768,
                Some("libros"),
                None,
                Some(131072),
                "libros",
                "x.gguf"
            ),
            65536
        );
        assert_eq!(
            resolve_context(
                None,
                Some("velocidad"),
                32768,
                Some("libros"),
                None,
                Some(131072),
                "velocidad",
                "x.gguf"
            ),
            32768
        );
    }

    /// PathBuf cuyos bytes NO son UTF-8 válido (lone surrogate en Windows,
    /// 0x80 en Unix). Con `panic = "abort"` el anterior `to_str().unwrap()`
    /// en el argv mataba el proceso entero.
    fn path_no_utf8() -> std::path::PathBuf {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&[0x0061, 0xD800, 0x0062]))
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![0x61, 0x80, 0x62]))
        }
    }

    #[test]
    fn path_no_utf8_convierte_sin_pania() {
        let p = path_no_utf8();
        // El presupuesto del test: el path ES no-UTF-8, así que el `unwrap`
        // anterior habría entrado en panic (y con abort, terminado el proceso).
        assert!(p.to_str().is_none(), "el fixture debe ser no-UTF-8");
        let s = p.to_string_lossy().to_string();
        assert!(!s.is_empty());
    }

    #[test]
    fn argv_con_modelo_no_utf8_no_pania() {
        // El argv real del motor con un path de modelo no-UTF-8: no debe
        // entrar en panic ni truncar silenciosamente la ruta.
        let dir = std::env::temp_dir().join(format!("lm-argv-utf8-{}", std::process::id()));
        let bin = dir.join("llama-server.exe");
        let base = dir.clone();
        let model = path_no_utf8();
        let cmd = ProcessManager::build_engine_cmd(
            &bin,
            &base,
            0,
            &model,
            32768,
            6,
            512,
            "0",
            &crate::config::EngineConfig::default(),
            &crate::config::HardwareProfile::default(),
            8080,
            false,
            "clave-de-prueba-123",
        );
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // `-m` presente y seguido de algo (el reemplazo de bytes no puede
        // dejar el argumento vacío).
        let i = argv.iter().position(|a| a == "-m").expect("-m en el argv");
        assert!(!argv[i + 1].is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// El defecto que faltaba tras el prefijo del banner: un `last_error` (o
    /// un veredicto de la puerta) del arranque ANTERIOR sobrevivía a un arranque
    /// RECHAZADO, porque la validación devuelve antes de `stop()` y del
    /// `spawn()` y `stop()` no limpia el error. El poller de la UI leía ese
    /// error viejo y pisaba el "Error al iniciar: ..." recién escrito.
    ///
    /// Se alcanza por el path REAL de `start()` sin lanzar ningún proceso: un
    /// modelo desconocido se rechaza en la validación, muy antes del chequeo
    /// de `llama_bin.exists()` y del `spawn()`. `base_dir` es temporal y no
    /// tiene `models/`, así que cualquier modelo es desconocido.
    #[test]
    fn intento_rechazado_limpia_el_error_anterior() {
        let dir = std::env::temp_dir().join(format!(
            "lm-slate-{}-{}",
            "intento-rechazado",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(
            &dir.join("config.toml"),
        ));
        let mgr = ProcessManager::new_with_log_file(dir.clone(), cfg, scratch_log(&dir));
        // Estado previo: el motor murió y el poller dejó su error, más un
        // veredicto viejo de la puerta (mismo arranque, mismo banner).
        {
            let mut st = mgr.status.write();
            st.status = "error".to_string();
            st.last_error = Some("El motor dejó de responder el endpoint /health.".to_string());
            st.acceptance_ok = Some(false);
            st.acceptance_error = Some("veredicto viejo de la puerta".to_string());
        }
        let res = mgr.start(StartRequest {
            model: Some("noexiste.gguf".to_string()),
            profile: None,
            context: None,
            threads: None,
            priority: None,
        });
        assert!(res.is_err(), "un modelo desconocido debe rechazarse");
        // Lo que el poller lee después: sin error, la rama del banner no entra.
        let st = mgr.get_status();
        assert!(
            st.last_error.is_none(),
            "el intento nuevo debe partir de `last_error` limpio, no arrastrar el anterior: {:?}",
            st.last_error
        );
        assert!(
            st.acceptance_error.is_none(),
            "mismo motivo para `acceptance_error` (rama hermana del banner): {:?}",
            st.acceptance_error
        );
        assert!(
            st.acceptance_ok.is_none(),
            "el veredicto de la puerta pertenece al arranque anterior, no a este"
        );
        drop(mgr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flags_bloqueadas_no_detienen_el_motor_actual() {
        let dir = std::env::temp_dir().join(format!("lm-rejected-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(dir.join("models/model.gguf"), b"fixture").unwrap();
        let cfg = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(
            &dir.join("config.toml"),
        ));
        cfg.update(|config| config.engine.extra_flags = vec!["--spec-type".to_string()]);
        let mgr = ProcessManager::new_with_log_file(dir.clone(), cfg, scratch_log(&dir));
        {
            let mut st = mgr.status.write();
            st.status = "running".to_string();
            st.is_healthy = true;
        }

        let err = mgr
            .start(StartRequest {
                model: Some("model.gguf".to_string()),
                profile: None,
                context: None,
                threads: None,
                priority: None,
            })
            .unwrap_err();
        assert!(err.contains("Flag bloqueada"), "{}", err);
        assert_eq!(mgr.get_status().status, "running");
        assert!(mgr.get_status().is_healthy);
        drop(mgr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `cargo test` escribía en el log REAL del dueño. Medido: 6498 → 6499
    /// líneas en `%APPDATA%\LocalMind\logs\localmind.log` con UN solo
    /// `ProcessManager` construido en un temporal. El culpable no es `log()`
    /// sino el `Drop` de CUALQUIER `ProcessManager`: `stop()` escribe el
    /// mensaje de proceso detenido y el archivo lo recibía porque `logs_dir`
    /// resuelve `%APPDATA%` e ignora el `base_dir`
    /// que recibe.
    ///
    /// No se toca esa precedencia (`%APPDATA%` gana sobre `base_dir` en
    /// producción: el log va al perfil del usuario, no junto al exe). Lo que se
    /// arregla es la CONSTRUCCIÓN: `new()` resuelve la ruta real y la entrega
    /// a `new_with_log_file`, así que un test puede darle la de su temporal sin
    /// tocar env global (los tests corren en paralelo; `set_var` sería data
    /// race, el mismo motivo por el que existe `ConfigStore::load_from_path`).
    #[test]
    fn process_manager_de_test_escribe_en_su_temporal_y_no_en_el_log_real() {
        let dir = std::env::temp_dir().join(format!(
            "lm-logdir-{}-{}",
            "aislamiento",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Validez del propio test: si la ruta que resuelve la app real coincidiera
        // con el temporal, "escribir en el temporal" sería escribir en el log
        // real y el assert de abajo no probaría nada.
        let real = crate::filelog::log_file(&dir);
        let scratch = scratch_log(&dir);
        assert_ne!(
            real, scratch,
            "si la resolución de producción cayera en el temporal, este test \
             no distinguiría nada; la precedencia de `%APPDATA%` cambió"
        );

        let cfg = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(
            &dir.join("config.toml"),
        ));
        let mgr = ProcessManager::new_with_log_file(dir.clone(), cfg, scratch.clone());
        assert_eq!(
            mgr.log_file, scratch,
            "el `ProcessManager` debe escribir en la ruta que le dieron, no en la \
             que resolvería `base_dir`"
        );

        // `log()` reparte a TRES destinos: anillo, SSE y archivo. Solo el
        // archivo se redirige; los otros dos se comprueban para que la
        // corrección no los rompa en silencio.
        let rx = mgr.subscribe_logs();
        mgr.log("[LocalMind] linea de prueba de aislamiento");
        let ring = mgr.get_recent_logs();
        assert_eq!(ring.len(), 1, "el anillo en memoria no se toca");
        assert_eq!(ring[0], "[LocalMind] linea de prueba de aislamiento");
        assert_eq!(
            rx.try_recv().expect("suscriptor SSE"),
            "[LocalMind] linea de prueba de aislamiento",
            "los suscriptores de /api/events no se tocan"
        );

        // El camino que realmente fugaba: `Drop` → `stop()`.
        drop(mgr);
        let escrito = std::fs::read_to_string(&scratch).unwrap_or_default();
        assert!(
            escrito.contains("linea de prueba de aislamiento"),
            "la línea de `log()` debe estar en el temporal: {:?}",
            escrito
        );
        assert!(
            escrito.contains("Proceso del motor detenido y finalizado."),
            "el `Drop` debe escribir en el log temporal del dueño; ahora debe escribir en \
             el temporal: {:?}",
            escrito
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Un `POST /api/start` RECHAZADO deja la pizarra limpia, pero eso no
    /// alcanzaba: la puerta de aceptación del `starting` anterior puede seguir
    /// en vuelo (son ~120 s de probe HTTP contra el motor) y su veredicto
    /// aterrizaba después del `= None`, repoblando `last_error` /
    /// `acceptance_error`. El epoch es el mecanismo que ya descarta esos
    /// veredictos (`start_epoch == my_epoch` en el poller); lo que faltaba era
    /// moverlo en el camino de rechazo, que devuelve antes de `stop()`.
    ///
    /// Lo que se verifica acá es la SEÑAL de invalidación (que el rechazo mueve
    /// el epoch), no el descarte: el descarte ocurre dentro del hilo poller,
    /// comparando contra el epoch que este test acaba de mover. Ejercitar el
    /// poller real exigiría un `llama-server` escuchando en un puerto y por lo
    /// tanto está fuera de una corrida offline.
    #[test]
    fn intento_rechazado_invalida_la_puerta_en_vuelo() {
        let dir =
            std::env::temp_dir().join(format!("lm-epoch-{}-{}", "rechazo", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(
            &dir.join("config.toml"),
        ));
        let mgr = ProcessManager::new_with_log_file(dir.clone(), cfg, scratch_log(&dir));
        let antes = mgr.start_epoch.load(Ordering::Relaxed);
        let res = mgr.start(StartRequest {
            model: Some("noexiste.gguf".to_string()),
            profile: None,
            context: None,
            threads: None,
            priority: None,
        });
        assert!(res.is_err(), "un modelo desconocido debe rechazarse");
        assert!(
            mgr.start_epoch.load(Ordering::Relaxed) > antes,
            "un intento rechazado debe invalidar la puerta en vuelo: sin este \
             bump, su veredicto se escribe después del `= None` de la pizarra"
        );
        drop(mgr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D-45 / P1-seg: el puerto crudo del motor deja de ser una puerta abierta.
    /// La clave del gateway viaja por el ENV del hijo (`LLAMA_API_KEY`), NUNCA
    /// en el argv: el argv lo lee cualquier proceso local (Task Manager, `wmic
    /// process get commandline`, ETW) y la clave quedaba expuesta. El build
    /// 10743 documenta `(env: LLAMA_API_KEY)` junto a `--api-key` en su
    /// `--help`, así que el soporte vía env está verificado contra el binario.
    /// Debe ser la MISMA clave del gateway, no una acuñada aparte: si
    /// divergieran, el proxy interno (health, puerta, metrics) respondería 401.
    #[test]
    fn argv_lleva_la_clave_del_gateway_al_motor() {
        let dir = std::env::temp_dir().join(format!("lm-argv-key-{}", std::process::id()));
        let key = "clave-de-prueba-123";
        let cmd = ProcessManager::build_engine_cmd(
            &dir.join("llama-server.exe"),
            &dir,
            0,
            &dir.join("m.gguf"),
            32768,
            6,
            512,
            "0",
            &crate::config::EngineConfig::default(),
            &crate::config::HardwareProfile::default(),
            8080,
            false,
            key,
        );
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // La clave NO puede ir en el argv (expuesta a cualquier proceso local).
        assert!(
            !argv.iter().any(|a| a == "--api-key"),
            "`--api-key` no debe aparecer en el argv: la clave va por env: {:?}",
            argv
        );
        assert!(
            !argv.iter().any(|a| a.as_str() == key),
            "la clave no puede colarse en el argv ni suelta ni como valor: {:?}",
            argv
        );
        // ...sino en el env del hijo, con la MISMA clave del gateway.
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|s| s.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert!(
            envs.iter()
                .any(|(k, v)| k == "LLAMA_API_KEY" && v.as_deref() == Some(key)),
            "LLAMA_API_KEY debe llevar la clave del gateway en el env del hijo: {:?}",
            envs
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// El reintento MTP relanza el mismo arranque sin `--spec-*`, pero con la
    /// MISMA clave por env: si la olvidara, el motor pediría credenciales y el
    /// reintento moriría en 401 en vez de probar la hipótesis de decodificación
    /// especulativa.
    #[test]
    fn argv_del_reintento_mtp_tambien_lleva_la_clave() {
        let dir = std::env::temp_dir().join(format!("lm-argv-mtpkey-{}", std::process::id()));
        let key = "clave-de-prueba-123";
        let cmd = ProcessManager::build_engine_cmd(
            &dir.join("llama-server.exe"),
            &dir,
            0,
            &dir.join("m.gguf"),
            32768,
            6,
            512,
            "0",
            &crate::config::EngineConfig::default(),
            &crate::config::HardwareProfile::default(),
            8080,
            true, // skip_spec = el reintento MTP
            key,
        );
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !argv.iter().any(|a| a == "--api-key"),
            "`--api-key` no debe aparecer ni en el reintento: {:?}",
            argv
        );
        assert!(
            !argv.iter().any(|a| a.as_str() == key),
            "la clave no puede colarse en el argv del reintento: {:?}",
            argv
        );
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|s| s.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert!(
            envs.iter()
                .any(|(k, v)| k == "LLAMA_API_KEY" && v.as_deref() == Some(key)),
            "el reintento también lleva la clave por env: {:?}",
            envs
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-seg: dos `start()` concurrentes no pueden lanzar dos hijos. El lock
    /// global (`try_lock` al abrir) hace atómico el check→spawn: el segundo
    /// intento recibe `Err("...en curso...")` sin tocar estado.
    ///
    /// Se alcanza sin motor real: el `base_dir` temporal no tiene
    /// `bin/llama-server.exe`, así que el primer intento toma el lock y muere
    /// en `No se encontró llama-server` (lock sostenido hasta volver), y el
    /// segundo —lanzado mientras el primero duerme dentro del lock— debe caer
    /// en `en curso`. Sin el lock ambos pasarían la validación y el segundo
    /// pisaría el `child`/estado del primero.
    #[test]
    fn arranques_concurrentes_un_solo_intento_prospera() {
        let dir = std::env::temp_dir().join(format!(
            "lm-startlock-{}-{}",
            "concurrente",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(
            &dir.join("config.toml"),
        ));
        let mgr = std::sync::Arc::new(ProcessManager::new_with_log_file(
            dir.clone(),
            cfg,
            scratch_log(&dir),
        ));
        // El guard sostiene el lock mientras el hilo intenta `start()`: simula
        // un `start()` en vuelo dentro de la ventana check→spawn. El hilo debe
        // rebotar con `en curso`. Se suelta (`drop`) antes de `drop(mgr)`:
        // el guard pide prestado `mgr` y el orden inverso no compila (E0505).
        let guard = mgr.start_lock.lock();
        let mgr2 = std::sync::Arc::clone(&mgr);
        let h = std::thread::spawn(move || {
            mgr2.start(StartRequest {
                model: None,
                profile: None,
                context: None,
                threads: None,
                priority: None,
            })
        });
        // Pequeña espera para que el hilo llegue al `try_lock` (el lock sigue
        // sostenido por este test; 50 ms sobran en CI local).
        std::thread::sleep(std::time::Duration::from_millis(50));
        let res = h.join().expect("el hilo de arranque no debe abortar");
        drop(guard);
        assert!(
            res.is_err(),
            "con el lock ocupado el segundo start debe rebotar"
        );
        let msg = res.unwrap_err();
        assert!(
            msg.contains("en curso"),
            "el rechazo debe decir `en curso`, no otro motivo: {}",
            msg
        );
        drop(mgr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-IO (cambio pedido por el slice de IO): `import_model_from_path` y
    /// `list_models` aceptan `.GGUF` en mayúsculas, coherente con
    /// `list_gguf_files`/`copy_picked` en `models.rs` (ya case-insensitive).
    /// En Windows el casing se preserva y un modelo válido no puede
    /// rechazarse por mayúsculas.
    #[test]
    fn import_y_listado_aceptan_gguf_en_mayusculas() {
        let dir = std::env::temp_dir().join(format!(
            "lm-ggufcase-{}-{}",
            "mayusculas",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("models")).unwrap();
        let cfg = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(
            &dir.join("config.toml"),
        ));
        let mgr = ProcessManager::new_with_log_file(dir.clone(), cfg, scratch_log(&dir));
        // `import_model_from_path` no rechaza por casing...
        let fuente = dir.join("MODELO.GGUF");
        std::fs::write(&fuente, b"gguf").unwrap();
        let nombre = mgr
            .import_model_from_path(&fuente, false)
            .expect("MODELO.GGUF debe importarse");
        assert_eq!(nombre, "MODELO.GGUF");
        // ...y `list_models` lo enumera (antes lo filtraba `ends_with`).
        let modelos = mgr.list_models();
        assert!(
            modelos.iter().any(|m| m.filename == "MODELO.GGUF"),
            "list_models debe incluir el .GGUF en mayúsculas: {:?}",
            modelos.iter().map(|m| &m.filename).collect::<Vec<_>>()
        );
        drop(mgr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TOCTOU 2026-10-10 (re-auditoría B): el chequeo `overwrite` vive DENTRO
    /// de `import_model_from_path`. Sin `overwrite`, el segundo import del
    /// mismo nombre falla con "ya existe"; con `overwrite=true`, pisa.
    /// Concurrente: de N hilos con `overwrite=false`, exactamente UNO gana.
    #[test]
    fn import_colision_y_concurrencia_no_pisan() {
        let dir = std::env::temp_dir().join(format!("lm-import-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("models")).unwrap();
        let cfg = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(
            &dir.join("config.toml"),
        ));
        let mgr = std::sync::Arc::new(ProcessManager::new_with_log_file(
            dir.clone(),
            cfg,
            scratch_log(&dir),
        ));
        let fuente = dir.join("dupe.gguf");
        std::fs::write(&fuente, b"gguf-1").unwrap();
        assert_eq!(
            mgr.import_model_from_path(&fuente, false).unwrap(),
            "dupe.gguf"
        );
        let err = mgr.import_model_from_path(&fuente, false).unwrap_err();
        assert!(err.is_exists(), "{}", err);
        // Carrera: N hilos, mismo nombre, sin overwrite → 1 gana, N-1 ven 409.
        let fuente2 = dir.join("carrera.gguf");
        std::fs::write(&fuente2, b"gguf").unwrap();
        let mut hilos = Vec::new();
        for _ in 0..8 {
            let (m, f) = (std::sync::Arc::clone(&mgr), fuente2.clone());
            hilos.push(std::thread::spawn(move || {
                m.import_model_from_path(&f, false)
            }));
        }
        let res: Vec<_> = hilos.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(res.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            res.iter()
                .filter(|r| matches!(r, Err(e) if e.is_exists()))
                .count(),
            7
        );
        // N1 wave-3: sin parciales — no queda ningún `.tmp-*` tras todo lo
        // anterior (ni tras la carrera ni tras los overwrite).
        let restos: Vec<_> = std::fs::read_dir(dir.join("models"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(restos.is_empty(), "tmps sin limpiar: {:?}", restos);
        // B3 wave-3: self-import (origen == destino) es no-op, no trunca.
        let ya = dir.join("models").join("dupe.gguf");
        let antes = std::fs::read(&ya).unwrap();
        assert_eq!(mgr.import_model_from_path(&ya, true).unwrap(), "dupe.gguf");
        assert_eq!(std::fs::read(&ya).unwrap(), antes);
        assert_eq!(mgr.import_model_from_path(&ya, false).unwrap(), "dupe.gguf");
        // ABIERTO-1 wave-5: pick-vs-pick y pick-vs-import comparten el lock:
        // 4 `import_picked` + 4 `import_model_from_path` concurrentes del
        // mismo nombre sin overwrite → exactamente 1 gana con ok.
        let fuente3 = dir.join("mezcla.gguf");
        std::fs::write(&fuente3, b"gguf").unwrap();
        let mut h2 = Vec::new();
        for i in 0..8 {
            let (m, f) = (std::sync::Arc::clone(&mgr), fuente3.clone());
            h2.push(std::thread::spawn(move || {
                if i % 2 == 0 {
                    m.import_picked(std::slice::from_ref(&f), false)
                        .map(|v| v.join(","))
                } else {
                    m.import_model_from_path(&f, false)
                }
            }));
        }
        let r2: Vec<_> = h2.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(r2.iter().filter(|r| r.is_ok()).count(), 1);
        drop(mgr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D-47: cada flag del motor llega al hijo UNA sola vez. Antes
    /// `build_engine_cmd` las anexaba y `start()` las repetía, así que el hijo
    /// veía `--reasoning-preserve` y las `extra_flags` duplicadas
    /// (`DEPRECATED: argument ... specified multiple times`).
    #[test]
    fn argv_no_repite_las_flags_del_motor() {
        let engine = crate::config::EngineConfig {
            reasoning_preserve: true,
            extra_flags: vec!["--jinja".to_string(), "--no-warmup".to_string()],
            ..crate::config::EngineConfig::default()
        };
        let profile = crate::config::HardwareProfile {
            cache_ram: 8192,
            extra_flags: vec!["--mlock".to_string()],
            ..crate::config::HardwareProfile::default()
        };

        let tokens = argv_tokens(&engine, &profile, 32768, false);
        let count = |flag: &str| tokens.iter().filter(|t| t.as_str() == flag).count();

        assert_eq!(
            count("--reasoning-preserve"),
            1,
            "--reasoning-preserve duplicado: {:?}",
            tokens
        );
        assert_eq!(
            count("--mlock"),
            1,
            "extra_flags del perfil duplicada: {:?}",
            tokens
        );
        assert_eq!(
            count("--jinja"),
            1,
            "extra_flags del engine duplicada: {:?}",
            tokens
        );
        assert_eq!(
            count("--no-warmup"),
            1,
            "extra_flags del engine duplicada: {:?}",
            tokens
        );
        // Y la banderola de la puerta PSU no se cuela en el argv.
        assert_eq!(
            count("--cache-reuse"),
            0,
            "--cache-reuse nunca se pasa: {:?}",
            tokens
        );
    }

    #[test]
    fn argv_pasa_cache_ram_cero_para_desactivar_el_default_del_motor() {
        let engine = crate::config::EngineConfig::default();
        let profile = crate::config::HardwareProfile {
            cache_ram: 0,
            ..crate::config::HardwareProfile::default()
        };
        let tokens = argv_tokens(&engine, &profile, 32768, false);
        let index = tokens.iter().position(|token| token == "--cache-ram");
        assert!(
            index.is_some(),
            "debe anularse el default de llama-server: {tokens:?}"
        );
        assert_eq!(
            tokens.get(index.unwrap() + 1).map(String::as_str),
            Some("0")
        );
    }

    /// El valor repetido también tenía que ser correcto, no solo único: cada
    /// flag que depende de un valor debe ir seguida del suyo.
    #[test]
    fn argv_no_repite_los_valores_de_las_flags() {
        let engine = crate::config::EngineConfig {
            reasoning_preserve: true,
            ..crate::config::EngineConfig::default()
        };
        let profile = crate::config::HardwareProfile {
            cache_ram: 8192,
            ..crate::config::HardwareProfile::default()
        };

        let tokens = argv_tokens(&engine, &profile, 32768, false);
        let values = |flag: &str| -> Vec<String> {
            tokens
                .iter()
                .enumerate()
                .filter(|(_, t)| t.as_str() == flag)
                .map(|(i, _)| tokens.get(i + 1).cloned().unwrap_or_default())
                .collect()
        };
        assert_eq!(
            values("--cache-ram"),
            vec!["8192".to_string()],
            "cache-ram: {:?}",
            tokens
        );
        assert_eq!(
            values("-c"),
            vec!["32768".to_string()],
            "contexto duplicado: {:?}",
            tokens
        );
        assert_eq!(
            values("-t"),
            vec!["6".to_string()],
            "hilos duplicados: {:?}",
            tokens
        );
        assert_eq!(
            values("--port"),
            vec!["8080".to_string()],
            "puerto duplicado: {:?}",
            tokens
        );
    }

    /// D-44: el banner de arranque describe el argv REAL. `--cache-reuse` no
    /// se envía mientras falte validarlo con el build 10743 y Qwen3.8, así que
    /// anunciarlo sería mentir sobre lo que se pasó.
    #[test]
    fn banner_de_arranque_no_anuncia_cache_reuse() {
        let b = ProcessManager::start_banner(32768, 6, "512", 8080);
        assert!(
            !b.to_lowercase().contains("cache-reuse"),
            "el banner no puede afirmar cache-reuse: {:?}",
            b
        );
        assert!(
            !b.contains("--cache-reuse"),
            "tampoco la flag literal: {:?}",
            b
        );
        // Lo que sí se pasa sigue estando, que es lo que hace útil el banner.
        assert!(b.contains("Contexto: 32768"), "{:?}", b);
        assert!(b.contains("Hilos: 6"), "{:?}", b);
        assert!(b.contains("uBatch: 512"), "{:?}", b);
        assert!(b.contains("Puerto: 8080"), "{:?}", b);
    }

    // (movido a `engine_gate.rs`: `adapter_ram_da_techo_maximo_y_filtra_basura`.)

    /// La cabecera que los clientes internos mandan al motor tiene que ser la
    /// que el motor acepta: `Authorization: Bearer <clave>`. El build 10743
    /// acepta `Authorization` y `X-Api-Key`; se usa `Authorization` para no
    /// inventar un segundo esquema.
    #[test]
    fn cabecera_al_motor_es_bearer() {
        assert_eq!(
            crate::auth::bearer("clave-de-prueba-123"),
            "Bearer clave-de-prueba-123"
        );
    }

    #[test]
    fn usage_non_stream() {
        let body =
            r#"{"id":"x","usage":{"prompt_tokens":8,"completion_tokens":64,"total_tokens":72}}"#;
        assert_eq!(parse_usage(body), Some((8, 64)));
    }

    #[test]
    fn usage_sse_final_chunk() {
        let body = "data: {\"id\":\"x\"}\n\ndata: {\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":61,\"total_tokens\":69}}\n\ndata: [DONE]\n";
        assert_eq!(parse_usage(body), Some((8, 61)));
    }

    #[test]
    fn engine_decode_tps_solo_lee_timings_del_motor() {
        let body = r#"{"usage":{"prompt_tokens":700,"completion_tokens":120},"timings":{"predicted_n":120,"predicted_ms":6000,"predicted_per_second":20.0}}"#;
        assert_eq!(parse_engine_decode_tps(body), Some(20.0));

        let sse = "data: {\"choices\":[]}\n\ndata: {\"timings\":{\"predicted_n\":40,\"predicted_ms\":2000}}\n\ndata: [DONE]\n";
        assert_eq!(parse_engine_decode_tps(sse), Some(20.0));

        let sse_then_empty_timings = "data: {\"timings\":{\"predicted_n\":40,\"predicted_ms\":2000}}\n\ndata: {\"timings\":{\"predicted_n\":0,\"predicted_ms\":0}}\n\ndata: [DONE]\n";
        assert_eq!(parse_engine_decode_tps(sse_then_empty_timings), Some(20.0));

        let pretty_json_with_data_text = r#"{
            "message": "data: descriptive text",
            "timings": {"predicted_n": 40, "predicted_ms": 2000}
        }"#;
        assert_eq!(
            parse_engine_decode_tps(pretty_json_with_data_text),
            Some(20.0)
        );

        let no_timings = r#"{"usage":{"prompt_tokens":700,"completion_tokens":120}}"#;
        assert_eq!(parse_engine_decode_tps(no_timings), None);
        let zero_decode =
            r#"{"timings":{"predicted_n":0,"predicted_ms":1,"predicted_per_second":0.0}}"#;
        assert_eq!(parse_engine_decode_tps(zero_decode), None);
    }

    #[test]
    fn gate_rate_incluye_el_cuerpo_completo_y_separa_decode() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = r#"{"usage":{"prompt_tokens":8,"completion_tokens":30},"timings":{"predicted_n":30,"predicted_ms":300,"predicted_per_second":100.0}}"#;
        let response_body = body.as_bytes().to_vec();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            let header_end;
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_len = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while request.len().saturating_sub(header_end) < content_len {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0, "el cuerpo de la petición quedó incompleto");
                request.extend_from_slice(&buf[..n]);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            )
            .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(250));
            stream.write_all(&response_body).unwrap();
        });

        let sample = ProcessManager::gate_sample(port, 200).unwrap();
        server.join().unwrap();
        assert!(
            sample.request_tps < 200.0,
            "la tasa de request debe incluir la espera del cuerpo: {:?}",
            sample
        );
        assert_eq!(sample.decode_tps, Some(100.0));
    }

    #[test]
    fn verdict_exactly_3tps_passes() {
        // 30 tokens en 10000 ms = 3 t/s exactos → pasa.
        let tps = acceptance_verdict(30, 10_000).expect("debe pasar");
        assert!((tps - 3.0).abs() < 1e-9);
    }

    #[test]
    fn verdict_19_tokens_fails() {
        assert_eq!(acceptance_verdict(19, 1000), Err("tokens".to_string()));
    }

    #[test]
    fn verdict_20_tokens_slow_fails() {
        // 20 tokens en 7000 ms = 2 t/s (redondeo abajo) → falla por velocidad.
        assert_eq!(acceptance_verdict(20, 7000), Err("tps".to_string()));
    }

    // (movido a `engine_gate.rs`: `eta_stored_and_cold`.)

    #[test]
    fn crash_summary_prefers_errors_and_trims() {
        let mut lines = Vec::new();
        for i in 0..300 {
            lines.push(format!("info de arranque {}", i));
        }
        lines.push("CUDA error: out of memory allocating 4 GB".to_string());
        lines.push("llama_model_load failed: bad weights".to_string());
        let summary = crash_summary(&lines, "1");
        assert!(summary.contains("(código: 1)"));
        assert!(summary.contains("out of memory") || summary.contains("failed"));
        assert!(summary.chars().count() <= 600 + " (código: 1)".chars().count());
        // Solo caben ≤30 líneas de entrada en la selección.
        assert!(summary.matches("info de arranque").count() <= 28);
    }

    #[test]
    fn crash_summary_fallback_last5() {
        let lines: Vec<String> = (0..10).map(|i| format!("linea {}", i)).collect();
        let summary = crash_summary(&lines, "exit code: 0");
        assert!(summary.contains("linea 9"));
        assert!(summary.contains("(código: exit code: 0)"));
    }

    #[test]
    fn load_times_roundtrip_cap64_and_evict65th() {
        let dir = std::env::temp_dir().join(format!("lm-loadtimes-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("load-times.json");
        let mut map = HashMap::new();
        for i in 0..64 {
            map.insert(format!("m.gguf|{}", 1000 + i), i as u64);
        }
        load_times_save(&path, &map).expect("save 64");
        let back = load_times_load(&path).expect("load 64");
        assert_eq!(back.len(), 64);
        // Clave 65: insertar + guardar recorta a 64 (evicción por cap).
        let mut map65 = back.clone();
        map65.insert("nuevo.gguf|32768".to_string(), 42);
        assert_eq!(map65.len(), 65);
        load_times_save(&path, &map65).expect("save 65");
        let back65 = load_times_load(&path).expect("load 65");
        assert_eq!(back65.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctx_explicit_wins_over_everything() {
        // Request explícito manda aunque perfil y sesión digan otra cosa.
        assert_eq!(
            resolve_context(
                Some(65536),
                Some("velocidad"),
                32768,
                Some("velocidad"),
                Some("m.gguf"),
                Some(131072),
                "velocidad",
                "m.gguf"
            ),
            65536
        );
    }

    #[test]
    fn ctx_explicit_profile_beats_stale_last() {
        // Regresión auditada: {"profile":"velocidad"} con last=131072 → 32768.
        assert_eq!(
            resolve_context(
                None,
                Some("velocidad"),
                32768,
                Some("libros"),
                Some("m.gguf"),
                Some(131072),
                "velocidad",
                "m.gguf"
            ),
            32768
        );
    }

    #[test]
    fn ctx_no_profile_matching_last_uses_last() {
        // Sin perfil en el request y misma sesión (perfil+modelo) → contexto guardado.
        assert_eq!(
            resolve_context(
                None,
                None,
                32768,
                Some("velocidad"),
                Some("m.gguf"),
                Some(32768),
                "velocidad",
                "m.gguf"
            ),
            32768
        );
    }

    #[test]
    fn ctx_no_profile_no_last_uses_default() {
        // Sin perfil ni sesión → contexto del perfil resuelto por defecto.
        assert_eq!(
            resolve_context(None, None, 32768, None, None, None, "velocidad", "m.gguf"),
            32768
        );
    }

    #[test]
    fn ctx_explicit_profile_same_as_last_still_profile() {
        // Perfil explícito igual al de la última sesión, sin contexto → perfil manda.
        assert_eq!(
            resolve_context(
                None,
                Some("velocidad"),
                32768,
                Some("velocidad"),
                Some("m.gguf"),
                Some(131072),
                "velocidad",
                "m.gguf"
            ),
            32768
        );
    }

    /// Log de un `ProcessManager` de test: SIEMPRE dentro de su temporal.
    /// Deliberadamente NO es `filelog::log_file`, que resuelve `%APPDATA%` y
    /// por tanto el log real del dueño. Passar esa por accidente devuelve el
    /// defecto que este archivo acaba de cerrar, así que el nombre lo dice.
    fn scratch_log(dir: &std::path::Path) -> PathBuf {
        dir.join("logs").join(crate::filelog::LOG_FILE_NAME)
    }

    fn scratch_models(tag: &str) -> PathBuf {
        // Directorio único por test: los tests corren en paralelo en el mismo
        // proceso y compartirlo provocaba borrados cruzados (remove_dir_all).
        let dir = std::env::temp_dir().join(format!("lm-resolve-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("sub"));
        let _ = std::fs::write(dir.join("plano.gguf"), b"x");
        let _ = std::fs::write(dir.join("sub").join("x.gguf"), b"y");
        dir
    }

    #[test]
    fn model_basename_resolves() {
        let dir = scratch_models("base");
        let p = resolve_model_path(&dir, "plano.gguf").expect("basename debe resolver");
        assert_eq!(p, dir.join("plano.gguf"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_rel_nested_resolves() {
        let dir = scratch_models("nested");
        let p = resolve_model_path(&dir, "sub/x.gguf").expect("rel anidado debe resolver");
        assert_eq!(p, dir.join("sub").join("x.gguf"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_traversal_rejected() {
        let dir = scratch_models("trav");
        assert!(resolve_model_path(&dir, "../x.gguf").is_err());
        assert!(resolve_model_path(&dir, "sub/../../x.gguf").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_absolute_rejected() {
        let dir = scratch_models("abs");
        assert!(resolve_model_path(&dir, "C:/abs/x.gguf").is_err());
        assert!(resolve_model_path(&dir, "/abs/x.gguf").is_err());
        assert!(resolve_model_path(&dir, "C:\\abs\\x.gguf").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_unknown_returns_not_found() {
        let dir = scratch_models("unknown");
        let err = resolve_model_path(&dir, "noexiste.gguf").unwrap_err();
        assert!(err.contains("Archivo no encontrado"), "inesperado: {}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_profile_id_is_real() {
        // D-1: el estado fresco nunca expone el fantasma `turbo`.
        let profiles = crate::config::built_in_profiles();
        let ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&crate::config::DEFAULT_PROFILE_ID));
        assert_ne!(crate::config::DEFAULT_PROFILE_ID, "turbo");
    }

    #[test]
    fn unknown_last_profile_falls_back_to_default() {
        // D-1: un id desconocido en el TOML cae al default real, no al fantasma.
        let profiles = crate::config::built_in_profiles();
        let resolved = crate::config::resolve_profile(&profiles, "turbo");
        assert_eq!(resolved.id, profiles[0].id);
        assert_eq!(profiles[0].id, crate::config::DEFAULT_PROFILE_ID);
    }

    #[test]
    fn guard_cooldown_blocks_then_allows() {
        // Cooldown 120 s: segundo arranque a los 30 s bloqueado con espera 90.
        let mut h = vec![1000u64];
        let err = check_start_guard(120, 4, &mut h, 1030).unwrap_err();
        assert_eq!(err.cooldown_left, 90);
        // Tras la ventana, permite (y no muta en el check).
        assert!(check_start_guard(120, 4, &mut h, 1120).is_ok());
        assert_eq!(h, vec![1000u64]);
    }

    #[test]
    fn guard_hourly_cap_blocks_fourth() {
        // Tope 4/hora: con 4 en la última hora, el 5º se rechaza sin espera.
        let mut h = vec![100u64, 200, 300, 400];
        let err = check_start_guard(0, 4, &mut h, 500).unwrap_err();
        assert_eq!(err.cooldown_left, 0);
        // Ventana rodada (el más viejo expira) vuelve a permitir.
        assert!(check_start_guard(0, 4, &mut h, 3701).is_ok());
    }

    #[test]
    fn guard_watchdog_does_not_retry() {
        // El watchdog es solo comentario + `error` sin reintento: verificar que
        // `check_start_guard` no registra nada solo (el registro ocurre una
        // vez en el arranque permitido, vía `record_start`).
        let mut h: Vec<u64> = Vec::new();
        assert!(check_start_guard(120, 4, &mut h, 9999).is_ok());
        assert!(h.is_empty());
        record_start(&mut h, 9999);
        assert_eq!(h, vec![9999u64]);
    }

    #[test]
    fn start_history_survives_process_manager_recreation() {
        let dir = std::env::temp_dir().join(format!(
            "lm-start-history-{}-{}",
            "recreation",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        let log_file = scratch_log(&dir);
        let now = unix_now_secs();

        let cfg_a = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(&config_path));
        let first = ProcessManager::new_with_log_file(dir.clone(), cfg_a, log_file.clone());
        {
            let mut history = first.start_history.lock();
            record_start(&mut history, now.saturating_sub(15));
            persist_start_history(&first.start_history_file, &history).unwrap();
        }
        drop(first);

        let cfg_b = std::sync::Arc::new(crate::config::ConfigStore::load_from_path(&config_path));
        let second = ProcessManager::new_with_log_file(dir.clone(), cfg_b, log_file);
        let mut loaded = second.start_history.lock();
        assert_eq!(
            loaded.len(),
            1,
            "el nuevo manager debe recuperar el arranque"
        );
        let wait = check_start_guard(120, 4, &mut loaded, unix_now_secs()).unwrap_err();
        assert!(wait.cooldown_left > 0 && wait.cooldown_left <= 105);
        drop(loaded);
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_history_replacement_is_atomic_and_corruption_fails_closed() {
        let dir = std::env::temp_dir().join(format!(
            "lm-start-history-{}-{}",
            "replace",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("start-history.json");
        let now = unix_now_secs();

        persist_start_history(&path, &[now.saturating_sub(50)]).unwrap();
        persist_start_history(&path, &[now.saturating_sub(40), now.saturating_sub(20)]).unwrap();
        assert_eq!(
            load_start_history(&path, now).unwrap(),
            vec![now.saturating_sub(40), now.saturating_sub(20)],
            "el rename debe reemplazar el historial previo en Windows"
        );

        std::fs::write(&path, b"not-json").unwrap();
        assert!(load_start_history(&path, now).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_history_future_timestamp_is_normalized_and_keeps_one_cooldown() {
        let dir =
            std::env::temp_dir().join(format!("lm-start-history-future-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("start-history.json");
        let now = 1_800_000_000u64;

        persist_start_history(&path, &[u64::MAX]).unwrap();
        assert_eq!(load_start_history(&path, now).unwrap(), vec![now]);
        assert_eq!(
            serde_json::from_slice::<Vec<u64>>(&std::fs::read(&path).unwrap()).unwrap(),
            vec![now],
            "el archivo también debe quedar corregido para reinicios posteriores"
        );

        let mut loaded_history = load_start_history(&path, now).unwrap();
        let wait = check_start_guard(120, 4, &mut loaded_history, now + 10).unwrap_err();
        assert_eq!(wait.cooldown_left, 110);
        assert_eq!(loaded_history, vec![now]);

        let mut in_memory_future = vec![u64::MAX];
        let wait = check_start_guard(120, 4, &mut in_memory_future, now + 10).unwrap_err();
        assert_eq!(wait.cooldown_left, 120);
        assert_eq!(in_memory_future, vec![now + 10]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slow_assessment_distinguishes_missing_decode_metrics_from_fast() {
        assert_eq!(engine_slow_assessment(None, 20.0), (false, false));
        assert_eq!(engine_slow_assessment(Some(19.9), 20.0), (true, true));
        assert_eq!(engine_slow_assessment(Some(20.0), 20.0), (true, false));
    }

    #[test]
    fn gate_median_picks_middle_of_three() {
        // Bimodalidad 128K: [13,0, 25,0, 15,0] → mediana 15,0 (no la fría).
        assert_eq!(gate_median(&[13.0, 25.0, 15.0]), Some(15.0));
        assert_eq!(gate_median(&[30.0, 14.0, 28.0]), Some(28.0));
    }

    #[test]
    fn gate_median_lower_bound_with_two() {
        // Con 2 muestras (una falló) se reporta la menor: no inflar.
        assert_eq!(gate_median(&[24.0, 15.0]), Some(15.0));
        assert_eq!(gate_median(&[]), None);
    }

    // (movidos a `engine_gate.rs`: `gate_lento_bajo_umbral`,
    // `auto_stop_borde_y_guardas`, `motor_perdido_a_los_10_fallos_en_running`.)

    fn val_ids() -> Vec<String> {
        vec!["velocidad".to_string(), "libros".to_string()]
    }

    #[test]
    fn req_unknown_profile_refused_listing_ids() {
        // `{"profile":"noexiste"}` → 400 con los ids válidos.
        let err = validate_req_profile(Some("noexiste"), &val_ids()).unwrap_err();
        assert!(err.contains("Perfil desconocido: 'noexiste'"), "{}", err);
        assert!(
            err.contains("velocidad") && err.contains("libros"),
            "{}",
            err
        );
        assert!(validate_req_profile(Some("libros"), &val_ids()).is_ok());
        assert!(validate_req_profile(None, &val_ids()).is_ok());
        assert!(validate_req_profile(Some(""), &val_ids()).is_ok());
    }

    #[test]
    fn req_context_bounds() {
        // 123 se rechaza; 4096 (múltiplo válido) pasa; omitido pasa.
        let err = validate_req_context(Some(123)).unwrap_err();
        assert!(err.contains("Contexto no válido: 123"), "{}", err);
        assert!(err.contains("1024"), "{}", err);
        assert!(validate_req_context(Some(4096)).is_ok());
        assert!(validate_req_context(Some(262144)).is_ok());
        assert!(validate_req_context(None).is_ok());
    }

    #[test]
    fn req_model_known_and_unknown() {
        let files = vec!["m.gguf".to_string(), "sub/x.gguf".to_string()];
        let names = vec!["m".to_string(), "x".to_string()];
        let aliases = vec!["localmind".to_string()];
        assert!(validate_req_model(Some("m.gguf"), &files, &names, &aliases).is_ok());
        assert!(validate_req_model(Some("sub/x.gguf"), &files, &names, &aliases).is_ok());
        assert!(validate_req_model(Some("localmind"), &files, &names, &aliases).is_ok());
        let err = validate_req_model(Some("otro.gguf"), &files, &names, &aliases).unwrap_err();
        assert!(err.contains("Modelo desconocido: 'otro.gguf'"), "{}", err);
        assert!(validate_req_model(None, &files, &names, &aliases).is_ok());
    }

    #[test]
    fn req_omitted_fields_resolve_as_before() {
        // Campos omitidos/vacíos no fallan: la precedencia sigue intacta.
        assert!(validate_req_profile(None, &val_ids()).is_ok());
        assert!(validate_req_context(None).is_ok());
        // `resolve_context` sin nada explícito → contexto del perfil.
        assert_eq!(
            resolve_context(None, None, 32768, None, None, None, "velocidad", "m.gguf"),
            32768
        );
    }

    #[test]
    fn req_refused_start_keeps_guard_window() {
        // Un arranque rechazado por validación NO consume cooldown/tope: el
        // check corre antes del guardarraíl y no toca `start_history`.
        // (Contrato: `validate_*` no recibe ni muta el historial.)
        let mut h: Vec<u64> = Vec::new();
        assert!(validate_req_profile(Some("noexiste"), &val_ids()).is_err());
        assert!(check_start_guard(120, 4, &mut h, 5000).is_ok());
        assert!(h.is_empty());
    }

    fn mtp_lines() -> Vec<String> {
        vec![
            "creating MTP draft context against the target model".to_string(),
            "model doesn't contain MTP layers".to_string(),
            "failed to create MTP context".to_string(),
            "exiting due to model loading error".to_string(),
        ]
    }

    #[test]
    fn mtp_signature_matches_real_log() {
        // Positivo con la línea real del fallo (LFM2.5).
        assert!(mtp_unsupported_signature(&mtp_lines()));
        // Negativos: errores ajenos no disparan el reintento.
        assert!(!mtp_unsupported_signature(&[
            "couldn't bind to port 8080".to_string()
        ]));
        assert!(!mtp_unsupported_signature(&[
            "CUDA error: out of memory".to_string()
        ]));
        assert!(!mtp_unsupported_signature(&[]));
        // Solo "MTP" suelto sin draft/fallo → no dispara.
        assert!(!mtp_unsupported_signature(&["mtp draft ok".to_string()]));
    }

    #[test]
    fn mtp_retry_once_starting_spec_signature() {
        // Solo starting + spec on + firma + no consumido → true.
        assert!(mtp_retry_decision(true, true, false, true));
        // Una vez consumido → false (una sola vez por arranque).
        assert!(!mtp_retry_decision(true, true, true, true));
        // En running (no starting) → false.
        assert!(!mtp_retry_decision(false, true, false, true));
        // Spec apagada → false (nada que omitir).
        assert!(!mtp_retry_decision(true, false, false, true));
        // Sin firma → false.
        assert!(!mtp_retry_decision(true, true, false, false));
    }

    #[test]
    fn props_n_ctx_shapes() {
        // Raíz, anidado, ausente (sin falso error), no numérico.
        assert_eq!(props_n_ctx(r#"{"n_ctx":65536}"#), Some(65536));
        assert_eq!(
            props_n_ctx(r#"{"default_generation_settings":{"n_ctx":131072}}"#),
            Some(131072)
        );
        assert_eq!(props_n_ctx(r#"{"n_ctx_slot":32768}"#), None);
        assert_eq!(props_n_ctx(r#"{"n_ctx":"mucho"}"#), None);
        assert_eq!(props_n_ctx("no-json"), None);
    }

    fn argv_tokens(
        engine: &crate::config::EngineConfig,
        profile: &crate::config::HardwareProfile,
        context: usize,
        skip_spec: bool,
    ) -> Vec<String> {
        // Reconstruye el argv vía `build_engine_cmd` y lo aplana a tokens.
        let cmd = ProcessManager::build_engine_cmd(
            &PathBuf::from("llama-server.exe"),
            &PathBuf::from("."),
            0x00004000,
            &PathBuf::from("m.gguf"),
            context,
            6,
            4,
            "2",
            engine,
            profile,
            8080,
            skip_spec,
            "clave-de-prueba-123",
        );
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    fn spec_engine() -> crate::config::EngineConfig {
        crate::config::EngineConfig {
            speculation: Some(crate::config::SpeculationConfig::default()),
            ..crate::config::EngineConfig::default()
        }
    }

    fn ctx_profile(context: usize) -> crate::config::HardwareProfile {
        crate::config::HardwareProfile {
            id: "t".to_string(),
            name: String::new(),
            description: String::new(),
            context,
            cache_ram: 0,
            extra_flags: Vec::new(),
        }
    }

    #[test]
    fn retry_argv_equals_first_minus_spec_32k() {
        // 32768: el reintento difiere SOLO en los `--spec-*` (mismo `-c`).
        let engine = spec_engine();
        let profile = ctx_profile(32768);
        let first = argv_tokens(&engine, &profile, 32768, false);
        let retry = argv_tokens(&engine, &profile, 32768, true);
        assert!(first.windows(2).any(|w| w[0] == "--spec-type"));
        assert!(!retry.iter().any(|t| t.starts_with("--spec")));
        // Mismo `-c 32768` en ambos (el bug stale era 32768 vs pedido).
        assert!(retry.windows(2).any(|w| w[0] == "-c" && w[1] == "32768"));
        // `retry` = `first` menos exactamente 6 tokens spec
        // (`--spec-type X --spec-draft-n-max Y --spec-draft-p-split Z`).
        assert_eq!(first.len(), retry.len() + 6);
        let mut fi = first.iter().peekable();
        let mut ri = retry.iter().peekable();
        loop {
            match (fi.peek(), ri.peek()) {
                (Some(f), Some(r)) if f == r => {
                    fi.next();
                    ri.next();
                }
                (Some(f), _) if f.starts_with("--spec") => {
                    // Saltar el par flag+valor SOLO en `first`.
                    fi.next();
                    fi.next();
                }
                (None, None) => break,
                other => panic!("divergen: {:?}", other),
            }
        }
    }

    #[test]
    fn retry_argv_equals_first_minus_spec_128k() {
        // 131072: mismo contrato (el bug stale era `n_ctx_slot = 32768`).
        let engine = spec_engine();
        let profile = ctx_profile(131072);
        let first = argv_tokens(&engine, &profile, 131072, false);
        let retry = argv_tokens(&engine, &profile, 131072, true);
        assert!(retry.windows(2).any(|w| w[0] == "-c" && w[1] == "131072"));
        assert!(!retry.iter().any(|t| t.starts_with("--spec")));
        assert!(first.windows(2).any(|w| w[0] == "--spec-type"));
    }
}
