//! Gestor de modelos (P6/P9, deuda D-10).
//!
//! - `POST /api/models/download {"repo":"owner/name","revision":"<sha opcional>","files":"<fichero opcional>"}`:
//!   fija el revision (si se omite, el SHA de `main` vía la API de HF), lista el
//!   árbol en ese revision y descarga cada fichero (o el indicado) en
//!   `models/<repo>/` (repo de un solo fichero → el `.gguf` queda en
//!   `models/<fichero>` para que el motor lo use tal cual). Verifica tamaño y
//!   sha256 contra el listado (`lfs.oid`; sin `lfs.oid` solo tamaño).
//! - Reglas copiadas de la implementación de referencia: se rechaza cualquier
//!   ruta con `..` o que empiece por `/`; se omite lo ya verificado; descarga a
//!   `<fichero>.part` con renombrado atómico tras verificar; resume con
//!   `Range: bytes=<existente>-` si hay `.part`; `HF_TOKEN` o
//!   `%APPDATA%\LocalMind\hf.token` como `Authorization: Bearer`; jamás escribe
//!   fuera de `models/`.
//! - `GET /api/models/download`: estado del trabajo (`idle|resolving|
//!   downloading|verifying|done|error`, con `percent` entero 0-100, nunca NaN).
//!   Un solo trabajo a la vez; un segundo POST ocupado → 409.
//! - `POST /api/models/download/cancel`: cancela en el próximo trozo, limpia
//!   los `.part` creados (salvo que puedan resumirse) e informa
//!   `state:"error"`, `error:"cancelado"`.
//! - `POST /api/models/import_pick`: diálogo nativo `rfd` (`*.gguf`,
//!   multiselección, en su propio hilo) y copia a `models/` (sin sobreescribir
//!   salvo `{"overwrite":true}`). Corrige D-10 (antes pedía teclear la ruta).
//! - Sin `unwrap` en rutas de request/IO/JSON: todo error devuelve 4xx/5xx.
//! - Memoria acotada: streaming a fichero por trozos, jamás el modelo en RAM.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

// ---------------------------------------------------------------------------
// sha256 (crate `sha2`, una sola dependencia pequeña)
// ---------------------------------------------------------------------------

/// sha256 hex de unos bytes (vector conocido en tests).
#[allow(dead_code)]
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(data);
    hex_of(&h.finalize())
}

fn hex_of(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// sha256 hex de un fichero por streaming (trozo a trozo, sin cargarlo entero).
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path)
        .map_err(|e| format!("No se pudo leer {}: {}", path.display(), e))?;
    let mut h = sha2::Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("Error al leer {}: {}", path.display(), e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex_of(&h.finalize()))
}

// ---------------------------------------------------------------------------
// Registro de verificación: models/.verified/<fichero>.json
// ---------------------------------------------------------------------------

/// `{sha256,size,repo,revision,at}` por fichero verificado.
#[derive(Debug, Clone)]
pub struct VerifiedRecord {
    pub sha256: String,
    pub size: u64,
    pub repo: String,
    pub revision: String,
    pub at: i64,
}

fn verified_dir(models_dir: &Path) -> PathBuf {
    models_dir.join(".verified")
}

/// Nombre seguro para el registro (el nombre de fichero ya pasó `check_rel_path`).
fn record_path(models_dir: &Path, filename: &str) -> PathBuf {
    verified_dir(models_dir).join(format!("{}.json", filename))
}

pub fn read_verified(models_dir: &Path, filename: &str) -> Option<VerifiedRecord> {
    let raw = std::fs::read_to_string(record_path(models_dir, filename)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(VerifiedRecord {
        sha256: v
            .get("sha256")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        size: v.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
        repo: v
            .get("repo")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        revision: v
            .get("revision")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        at: v.get("at").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}

/// Escritura tmp+rename (nunca un `.json` a medias).
pub fn write_verified(models_dir: &Path, filename: &str, rec: &VerifiedRecord) {
    let dir = verified_dir(models_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let body = serde_json::json!({
        "sha256": rec.sha256,
        "size": rec.size,
        "repo": rec.repo,
        "revision": rec.revision,
        "at": rec.at,
    })
    .to_string();
    let tmp = dir.join(format!("{}.json.tmp", filename));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, record_path(models_dir, filename));
    }
}

// ---------------------------------------------------------------------------
// Listado del árbol de Hugging Face
// ---------------------------------------------------------------------------

/// Entrada útil del `tree`: ruta relativa, tamaño y `lfs.oid` (sha256) si lo hay.
#[derive(Debug, Clone)]
pub struct TreeFile {
    pub path: String,
    pub size: u64,
    pub oid: Option<String>,
}

/// ¿Ruta relativa segura dentro de `models/`? Rechaza `..`, `/` inicial y
/// separadores absolutos de Windows. Devuelve el motivo en español si no.
pub fn check_rel_path(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("Ruta vacía en el listado del repositorio".to_string());
    }
    if p.starts_with('/') || p.starts_with('\\') {
        return Err(format!("Ruta absoluta rechazada: {}", p));
    }
    // `..` como segmento (también con `\` de Windows) → fuera.
    let norm = p.replace('\\', "/");
    for seg in norm.split('/') {
        if seg == ".." {
            return Err(format!("Ruta con '..' rechazada: {}", p));
        }
    }
    if norm.contains(':') {
        return Err(format!("Ruta con unidad rechazada: {}", p));
    }
    Ok(())
}

/// Parsear el JSON del `tree` de HF (`[{path,size,lfs:{oid},type}]`) quedándose
/// con ficheros (`type=="file"`). Sin `lfs.oid` el fichero es pequeño: solo
/// tamaño (el llamador lo informa).
pub fn parse_tree(body: &serde_json::Value) -> Result<Vec<TreeFile>, String> {
    let arr = body
        .as_array()
        .ok_or_else(|| "Respuesta del árbol inesperada (no es una lista)".to_string())?;
    let mut out = Vec::new();
    for e in arr {
        if e.get("type").and_then(|t| t.as_str()) != Some("file") {
            continue;
        }
        let path = e
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            continue;
        }
        check_rel_path(&path)?;
        let size = e.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        let oid = e
            .get("lfs")
            .and_then(|l| l.get("oid"))
            .and_then(|o| o.as_str())
            .map(str::to_string);
        out.push(TreeFile { path, size, oid });
    }
    Ok(out)
}

/// ¿`repo` con forma `owner/name`? (sin espacios, una sola barra, sin `..`).
pub fn check_repo(repo: &str) -> Result<(), String> {
    let r = repo.trim();
    if r.is_empty() {
        return Err("Falta el campo 'repo' (forma owner/name)".to_string());
    }
    let parts: Vec<&str> = r.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!("Repo inválido '{}': se espera owner/name", repo));
    }
    if r.contains("..") || r.contains(' ') || r.contains('\\') {
        return Err(format!("Repo inválido '{}'", repo));
    }
    for c in r.chars() {
        if !(c.is_ascii_alphanumeric() || "_-./".contains(c)) {
            return Err(format!("Repo inválido '{}': carácter '{}'", repo, c));
        }
    }
    Ok(())
}

/// ¿SHA de 40 hex?
pub fn check_revision(rev: &str) -> Result<(), String> {
    if rev.len() != 40 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Revision inválida '{}': se espera un SHA de 40 hex",
            rev
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Token de HF: env `HF_TOKEN` o `%APPDATA%\LocalMind\hf.token`
// ---------------------------------------------------------------------------

/// Ruta del token persistido (`%APPDATA%\LocalMind\hf.token`).
/// `None` sin `APPDATA` (entorno mínimo): el token solo vive en `HF_TOKEN`.
pub fn hf_token_path() -> Option<PathBuf> {
    std::env::var("APPDATA")
        .map(|a| PathBuf::from(a).join("LocalMind").join("hf.token"))
        .ok()
}

/// Guardar el token de HF en disco con la misma ACL que `gateway.key`.
/// P1 producción: reutiliza `auth::restrict_key_file` tras escribir (antes
/// el fichero quedaba con la ACL heredada, legible para otros usuarios);
/// si `icacls` falla ya avisa por stderr. Núcleo testeable con ruta inyectada.
/// Hoy sin llamador en prod (el token solo se lee de `HF_TOKEN`/archivo):
/// primitiva lista para el endpoint de token de repos privados.
#[allow(dead_code)]
pub fn save_hf_token_at(path: &Path, token: &str) -> Result<(), String> {
    let content = format!("{}\n", token.trim());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("No se pudo crear {}: {}", parent.display(), e))?;
    }
    std::fs::write(path, &content)
        .map_err(|e| format!("No se pudo escribir {}: {}", path.display(), e))?;
    crate::auth::restrict_key_file(path);
    Ok(())
}

pub fn hf_token() -> Option<String> {
    if let Ok(t) = std::env::var("HF_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let p = hf_token_path()?;
    let raw = std::fs::read_to_string(p).ok()?;
    let t = raw.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn hf_get(url: &str) -> Result<ureq::Response, String> {
    let mut r = ureq::get(url).timeout(std::time::Duration::from_secs(30));
    if let Some(t) = hf_token() {
        r = r.set("Authorization", &format!("Bearer {}", t));
    }
    r.call().map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            let mut b = String::new();
            let _ = resp.into_reader().read_to_string(&mut b);
            format!("Hugging Face devolvió {}: {}", code, snippet(&b))
        }
        ureq::Error::Transport(t) => format!("Error de red con Hugging Face: {}", t),
    })
}

/// Resolver el SHA de `main` cuando no se fija revision.
pub fn resolve_main_sha(repo: &str) -> Result<String, String> {
    let url = format!("https://huggingface.co/api/models/{}", repo);
    let resp = hf_get(&url)?;
    let body = resp
        .into_string()
        .map_err(|e| format!("Error al leer metadatos de {}: {}", repo, e))?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("Metadatos inválidos de {}: {}", repo, e))?;
    let sha = v
        .get("sha")
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("El repo {} no informa su SHA actual", repo))?;
    check_revision(&sha)?;
    Ok(sha)
}

/// Listar el árbol en un revision ya fijado.
pub fn fetch_tree(repo: &str, rev: &str) -> Result<Vec<TreeFile>, String> {
    let url = format!(
        "https://huggingface.co/api/models/{}/tree/{}?recursive=1",
        repo, rev
    );
    let resp = hf_get(&url)?;
    let body = resp
        .into_string()
        .map_err(|e| format!("Error al leer el árbol de {}: {}", repo, e))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Árbol inválido de {}: {}", repo, e))?;
    parse_tree(&v)
}

/// Asesor preliminar a partir del árbol remoto de Hugging Face. Solo conoce el
/// tamaño del fichero: no puede leer metadatos GGUF, contexto ni VRAM libre del
/// dispositivo elegido. Devuelve `unknown` hasta que exista evidencia local.
/// `--cache-ram` limita la caché de prompts del servidor; no amplía el KV activo.
pub fn advise_fit(
    tree: &[TreeFile],
    only_file: Option<&str>,
    vram_total_mb: Option<u64>,
    _ram_total_mb: Option<u64>,
) -> serde_json::Value {
    let ggufs: Vec<&TreeFile> = tree
        .iter()
        .filter(|f| f.path.to_lowercase().ends_with(".gguf"))
        .collect();
    if ggufs.is_empty() {
        return serde_json::json!({
            "verdict": "no_gguf",
            "detail": "El repo no trae ningún .gguf en este revision.",
        });
    }
    let target: &TreeFile = match only_file {
        Some(f) => match ggufs.iter().find(|g| {
            g.path == f
                || g.path
                    .to_lowercase()
                    .ends_with(&format!("/{}", f.to_lowercase()))
                || g.path.eq_ignore_ascii_case(f)
        }) {
            Some(t) => t,
            None => {
                return serde_json::json!({
                    "verdict": "no_file",
                    "detail": format!("El repo no trae '{}' en este revision.", f),
                })
            }
        },
        None => ggufs.iter().max_by_key(|g| g.size).unwrap_or(&ggufs[0]),
    };
    let model_mb = target.size / (1024 * 1024);
    let weight_only_budget_mb = vram_total_mb.map(|v| v.saturating_mul(9) / 10);
    let weight_only_exceeds_budget = weight_only_budget_mb.is_some_and(|b| model_mb > b);
    serde_json::json!({
        "verdict": "unknown",
        "file": target.path,
        "size_mb": model_mb,
        "weight_only_budget_mb": weight_only_budget_mb,
        "weight_only_exceeds_budget": weight_only_exceeds_budget,
        "detail": match weight_only_budget_mb {
            Some(b) => format!("El repo solo informa el tamaño del GGUF ({} MB). Sin leer sus metadatos y conocer el contexto y dispositivo no se puede estimar el KV activo; los pesos solos {} el presupuesto de referencia de {} MB. --cache-ram limita la caché de prompts, no amplía el contexto.", model_mb, if weight_only_exceeds_budget { "superan" } else { "quedan bajo" }, b),
            None => format!("El repo informa un GGUF de {} MB, pero este equipo no reporta VRAM utilizable. También faltan los metadatos GGUF para estimar el KV activo.", model_mb),
        }
    })
}

fn snippet(s: &str) -> String {
    const MAX: usize = 300;
    let t = s.trim();
    // Por chars, no por bytes: `&t[..MAX]` parte un char multibyte y panica
    // (misma clase que el P0 de update.rs; ver `snippet` allí).
    if t.chars().count() <= MAX {
        t.to_string()
    } else {
        format!("{}…", t.chars().take(MAX).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Estado del trabajo (un solo job; global del proceso)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct JobState {
    pub state: String,
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub error: Option<String>,
}

impl JobState {
    pub fn idle() -> Self {
        Self {
            state: "idle".to_string(),
            repo: String::new(),
            revision: String::new(),
            file: String::new(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            error: None,
        }
    }

    /// `percent` entero 0-100, nunca NaN (0 si no hay total).
    pub fn percent(&self) -> u64 {
        if self.bytes_total == 0 {
            return if self.state == "done" { 100 } else { 0 };
        }
        (self.bytes_done.saturating_mul(100) / self.bytes_total).min(100)
    }

    pub fn json(&self) -> String {
        serde_json::json!({
            "state": self.state,
            "repo": self.repo,
            "revision": self.revision,
            "file": self.file,
            "files_done": self.files_done,
            "files_total": self.files_total,
            "bytes_done": self.bytes_done,
            "bytes_total": self.bytes_total,
            "percent": self.percent(),
            "error": self.error.clone().unwrap_or_default(),
        })
        .to_string()
    }
}

struct JobInner {
    state: Mutex<JobState>,
    busy: AtomicBool,
    cancel: AtomicBool,
    bytes_done: AtomicU64,
    /// Cuándo se tomó `busy` (epoch s). Watchdog (P1 producción, espejo de
    /// `update.rs`): si el worker muere sin liberarlo, el próximo `start_job`
    /// lo recupera tras 10 min en vez de dar 409 para siempre.
    busy_since_secs: AtomicU64,
}

static JOB: LazyLock<JobInner> = LazyLock::new(|| JobInner {
    state: Mutex::new(JobState::idle()),
    busy: AtomicBool::new(false),
    cancel: AtomicBool::new(false),
    bytes_done: AtomicU64::new(0),
    busy_since_secs: AtomicU64::new(0),
});

fn job() -> &'static JobInner {
    &JOB
}

pub fn job_snapshot() -> JobState {
    let mut s = job()
        .state
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|p| p.into_inner().clone());
    s.bytes_done = job().bytes_done.load(Ordering::Relaxed);
    s
}

fn job_set(f: impl FnOnce(&mut JobState)) {
    if let Ok(mut g) = job().state.lock() {
        f(&mut g);
    }
}

/// ¿Hay trabajo en curso? (para 409 ante un segundo POST).
/// Usada por la ruta `POST /api/models/download/cancel` (server.rs).
pub fn job_busy() -> bool {
    job().busy.load(Ordering::Relaxed)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Tomar `busy` o recuperarlo por watchdog (P1 producción, espejo de
/// `update.rs`): si estaba tomado hace más de 10 min, el worker murió sin
/// liberarlo y se recupera en vez de dar 409 para siempre. `true` = vía libre.
fn take_busy_or_recover() -> bool {
    if !job().busy.swap(true, Ordering::SeqCst) {
        job().busy_since_secs.store(now_secs(), Ordering::Relaxed);
        return true;
    }
    let since = job().busy_since_secs.load(Ordering::Relaxed);
    if since > 0 && now_secs().saturating_sub(since) >= 600 {
        job().busy_since_secs.store(now_secs(), Ordering::Relaxed);
        job().cancel.store(false, Ordering::Relaxed);
        return true;
    }
    false
}

/// Latido del worker: marca `busy` como vivo para que el watchdog no lo robe
/// a mitad de una descarga lenta. Llamar por chunk/no-op periódico.
fn heartbeat_busy() {
    job().busy_since_secs.store(now_secs(), Ordering::Relaxed);
}

fn release_busy() {
    job().busy.store(false, Ordering::SeqCst);
    job().busy_since_secs.store(0, Ordering::Relaxed);
}

pub fn job_cancel() -> JobState {
    if !job_busy() {
        let mut s = job_snapshot();
        s.state = "idle".to_string();
        s.error = None;
    }
    job().cancel.store(true, Ordering::Relaxed);
    job_snapshot()
}

// ---------------------------------------------------------------------------
// Descarga (hilo worker; streaming a `.part`, resume, verificación)
// ---------------------------------------------------------------------------

/// Destino dentro de `models/`: la descarga de UN solo `.gguf` queda PLANA en
/// `models/<fichero>` (aunque el repo lo guarde en subcarpetas), para que el
/// listado del motor (`models/*.gguf`) la vea y se pueda lanzar. Multi-fichero
/// → `models/<repo-name>/<ruta>`. Jamás sale de `models/`.
pub fn dest_for(models_dir: &Path, repo: &str, files: &[TreeFile]) -> Result<PathBuf, String> {
    if files.len() == 1 && files[0].path.to_lowercase().ends_with(".gguf") {
        let name = files[0]
            .path
            .replace('\\', "/")
            .split('/')
            .next_back()
            .unwrap_or("")
            .to_string();
        if name.is_empty() || name.contains("..") {
            return Err(format!("Nombre de fichero inválido: {}", files[0].path));
        }
        let p = models_dir.join(&name);
        check_inside(models_dir, &p)?;
        return Ok(p);
    }
    let repo_name = repo.split('/').next_back().unwrap_or(repo).to_string();
    let base = models_dir.join(&repo_name);
    check_inside(models_dir, &base)?;
    Ok(base)
}

fn check_inside(models_dir: &Path, p: &Path) -> Result<(), String> {
    // Contención por prefijo real: normalizar ambas rutas (sin `.`, `..`
    // replegado) y exigir `starts_with`. La vieja comparación de longitudes
    // aceptaba `C:/<largo>/x.gguf` (absoluta larga) como contenida.
    fn normalizar(base: &Path) -> PathBuf {
        use std::path::Component;
        let mut out = PathBuf::new();
        for c in base.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                _ => out.push(c.as_os_str()),
            }
        }
        out
    }
    // `..` explícito jamás se repliega: es escape, no normalización.
    for c in p.components() {
        use std::path::Component;
        if matches!(c, Component::ParentDir) {
            return Err(format!("Ruta fuera de models/: {}", p.display()));
        }
    }
    let root = normalizar(models_dir);
    let cur = normalizar(p);
    if !cur.starts_with(&root) {
        return Err(format!("Ruta fuera de models/: {}", p.display()));
    }
    Ok(())
}
/// Pre-chequeo de espacio (P1 producción): ¿cabe `need` bytes en el volumen
/// de `models_dir` con margen 2× (misma regla que `update::fits_download`)?
/// Reutiliza `update::free_bytes_for` + `update::fits_download` (ver
/// update.rs:359-403 como modelo). `None` = sin veredicto (sin dato medido el
/// llamador NO bloquea). Puro en decisión salvo la medición best-effort.
pub fn models_space_ok(models_dir: &Path, need: u64) -> Option<bool> {
    crate::update::fits_download(Some(need), crate::update::free_bytes_for(models_dir))
}

/// Mensaje 507 en español con el tamaño que no cabe (P1 producción).
pub fn models_no_space_msg(need: u64) -> String {
    format!(
        "Sin espacio en el disco de modelos para descargar {:.1} GB (se exigen el doble libres): libera espacio o cambia la carpeta de modelos.",
        need as f64 / (1024.0 * 1024.0 * 1024.0)
    )
}

/// Descargar un fichero con resume y verificación. Devuelve bytes finales.
fn download_one(
    models_dir: &Path,
    repo: &str,
    rev: &str,
    tf: &TreeFile,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<u64, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelado".to_string());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("No se pudo crear {}: {}", parent.display(), e))?;
    }
    // P1 producción: pre-chequeo por fichero (un resume a mitad de un .gguf
    // de 15GB también se queda sin disco). Error temprano con mensaje 507.
    if models_space_ok(models_dir, tf.size) == Some(false) {
        return Err(models_no_space_msg(tf.size));
    }
    // Omitir lo ya verificado (mismo tamaño + mismo sha registrado).
    if dest.exists() {
        if let Ok(meta) = std::fs::metadata(dest) {
            if meta.len() == tf.size {
                let name = dest
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Some(rec) = read_verified(models_dir, &name) {
                    let oid_ok = tf.oid.as_ref().map(|o| o == &rec.sha256).unwrap_or(true);
                    if oid_ok && rec.size == tf.size && rec.repo == repo && rec.revision == rev {
                        job().bytes_done.fetch_add(tf.size, Ordering::Relaxed);
                        return Ok(tf.size);
                    }
                } else if tf.oid.is_none() {
                    // Fichero pequeño sin lfs: el tamaño basta.
                    job().bytes_done.fetch_add(tf.size, Ordering::Relaxed);
                    return Ok(tf.size);
                }
            }
        }
    }
    let part = dest.with_extension("part");
    // Si quedó un `.part` de otra descarga, solo se resume si es del mismo
    // fichero (mismo destino); si no, se limpia al cancelar.
    let mut start: u64 = 0;
    if let Ok(meta) = std::fs::metadata(&part) {
        start = meta.len();
        if start > tf.size {
            let _ = std::fs::remove_file(&part);
            start = 0;
        }
    }
    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        repo, rev, tf.path
    );
    let mut req = ureq::get(&url).timeout(std::time::Duration::from_secs(3600));
    if let Some(t) = hf_token() {
        req = req.set("Authorization", &format!("Bearer {}", t));
    }
    if start > 0 {
        req = req.set("Range", &format!("bytes={}-", start));
    }
    let resp = req.call().map_err(|e| match e {
        ureq::Error::Status(code, r) => {
            let mut b = String::new();
            let _ = r.into_reader().read_to_string(&mut b);
            format!("Descarga devolvió {}: {}", code, snippet(&b))
        }
        ureq::Error::Transport(t) => format!("Error de red al descargar {}: {}", tf.path, t),
    })?;
    // 206 = resume aceptado; 200 con `start>0` = el servidor ignoró el Range:
    // se empieza de cero para no mezclar bytes.
    let resumed = resp.status() == 206;
    if !resumed {
        start = 0;
        let _ = std::fs::remove_file(&part);
    }
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(resumed)
        .write(!resumed)
        .truncate(!resumed)
        .open(&part)
        .map_err(|e| format!("No se pudo escribir {}: {}", part.display(), e))?;
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 65536];
    let mut written = start;
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(out);
            // El `.part` queda para resumir la próxima vez.
            return Err("cancelado".to_string());
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("Error al descargar {}: {}", tf.path, e))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("Error al guardar {}: {}", part.display(), e))?;
        written += n as u64;
        job().bytes_done.fetch_add(n as u64, Ordering::Relaxed);
        // Latido: la descarga sigue viva aunque sea lenta (watchdog 10 min).
        heartbeat_busy();
    }
    drop(out);
    let meta = std::fs::metadata(&part)
        .map_err(|e| format!("No se pudo medir {}: {}", part.display(), e))?;
    if meta.len() != tf.size {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "Tamaño inesperado en {}: {} bytes frente a {} esperados",
            tf.path,
            meta.len(),
            tf.size
        ));
    }
    if let Some(oid) = tf.oid.as_deref() {
        let got = sha256_file(&part)?;
        if got != oid.to_lowercase() {
            let _ = std::fs::remove_file(&part);
            return Err(format!(
                "SHA-256 no coincide en {} (descarga corrupta)",
                tf.path
            ));
        }
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        write_verified(
            models_dir,
            &name,
            &VerifiedRecord {
                sha256: got,
                size: tf.size,
                repo: repo.to_string(),
                revision: rev.to_string(),
                at: crate::usage::now_ts(),
            },
        );
    }
    std::fs::rename(&part, dest).map_err(|e| format!("No se pudo mover {}: {}", tf.path, e))?;
    Ok(written)
}

/// Lanzar el trabajo en un hilo. `models_dir` es `base_dir/models`.
/// Devuelve `Err` si hay otro trabajo en curso (el llamador responde 409).
pub fn start_job(
    models_dir: PathBuf,
    log: Arc<dyn Fn(String) + Send + Sync>,
    repo: String,
    revision: Option<String>,
    only_file: Option<String>,
) -> Result<(), String> {
    if !take_busy_or_recover() {
        return Err("Ya hay una descarga en curso".to_string());
    }
    job().cancel.store(false, Ordering::Relaxed);
    job().bytes_done.store(0, Ordering::Relaxed);
    job_set(|s| {
        *s = JobState {
            state: "resolving".to_string(),
            repo: repo.clone(),
            revision: revision.clone().unwrap_or_default(),
            file: only_file.clone().unwrap_or_default(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            error: None,
        }
    });
    std::thread::spawn(move || {
        // P0 producción (panic=abort): el worker libera `busy` aunque
        // `run_job` panique; si no, el canal quedaba en 409 para siempre.
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_job(
                &models_dir,
                &repo,
                revision.as_deref(),
                only_file.as_deref(),
                &log,
            )
        }))
        .unwrap_or_else(|_| Some("el worker de descarga abortó (pánico interno)".to_string()));
        match err {
            None => {
                job_set(|s| {
                    s.state = "done".to_string();
                    s.file = String::new();
                    s.error = None;
                });
                log("[LocalMind] Descarga de modelo completada y verificada.".to_string());
            }
            Some(e) => {
                // Al cancelar se limpian los `.part` que no puedan resumirse
                // (aquí: se dejan, porque el resume los reutiliza; solo se
                // informa el estado).
                job_set(|s| {
                    s.state = "error".to_string();
                    s.error = Some(e.clone());
                });
                log(format!("[LocalMind] Error al descargar modelo: {}", e));
            }
        }
        release_busy();
    });
    Ok(())
}

fn run_job(
    models_dir: &Path,
    repo: &str,
    revision: Option<&str>,
    only_file: Option<&str>,
    log: &Arc<dyn Fn(String) + Send + Sync>,
) -> Option<String> {
    // 1. Fijar revision.
    let rev = match revision {
        Some(r) => {
            if let Err(e) = check_revision(r) {
                return Some(e);
            }
            r.to_string()
        }
        None => match resolve_main_sha(repo) {
            Ok(s) => s,
            Err(e) => return Some(e),
        },
    };
    job_set(|s| {
        s.revision = rev.clone();
        s.state = "downloading".to_string();
    });
    log(format!(
        "[LocalMind] Descargando {} @ {} …",
        repo,
        &rev[..8.min(rev.len())]
    ));
    // 2. Árbol.
    let mut files = match fetch_tree(repo, &rev) {
        Ok(f) => f,
        Err(e) => return Some(e),
    };
    if let Some(one) = only_file {
        if let Err(e) = check_rel_path(one) {
            return Some(e);
        }
        let before = files.len();
        files.retain(|f| f.path == one);
        if files.is_empty() {
            return Some(format!(
                "El fichero '{}' no está en el repo {} @ {}",
                one,
                repo,
                &rev[..8.min(rev.len())]
            ));
        }
        let _ = before;
    }
    if files.is_empty() {
        return Some(format!("El repo {} no tiene ficheros descargables", repo));
    }
    let base = match dest_for(models_dir, repo, &files) {
        Ok(b) => b,
        Err(e) => return Some(e),
    };
    let total: u64 = files.iter().map(|f| f.size).sum();
    job_set(|s| {
        s.files_total = files.len();
        s.bytes_total = total;
    });
    // P1 producción: pre-chequeo de espacio ANTES de bajar ~15GB. Con dato
    // medido y sin 2× se aborta temprano con el mismo mensaje del 507 (el
    // handler lo mapea a 507). Sin dato no se bloquea.
    if models_space_ok(models_dir, total) == Some(false) {
        return Some(models_no_space_msg(total));
    }
    // 3. Descarga + verificación, fichero a fichero.
    // `single` = mismo criterio que `dest_for`: UN solo `.gguf` → plano.
    let single = files.len() == 1 && files[0].path.to_lowercase().ends_with(".gguf");
    for (i, tf) in files.iter().enumerate() {
        if job().cancel.load(Ordering::Relaxed) {
            return Some("cancelado".to_string());
        }
        job_set(|s| {
            s.file = tf.path.clone();
            s.files_done = i;
        });
        let dest = if single {
            match dest_for(models_dir, repo, &files) {
                Ok(d) => d,
                Err(e) => return Some(e),
            }
        } else {
            let p = base.join(&tf.path);
            if let Err(e) = check_inside(models_dir, &p) {
                return Some(e);
            }
            p
        };
        job_set(|s| {
            s.state = "downloading".to_string();
        });
        match download_one(models_dir, repo, &rev, tf, &dest, &job().cancel) {
            Ok(_) => {
                job_set(|s| {
                    s.files_done = i + 1;
                    s.state = "verifying".to_string();
                });
            }
            Err(e) => {
                if e == "cancelado" {
                    return Some("cancelado".to_string());
                }
                return Some(e);
            }
        }
    }
    job_set(|s| {
        s.files_done = files.len();
        s.state = "verifying".to_string();
    });
    None
}

// ---------------------------------------------------------------------------
// Listado utilizable de models/ (raíz + un nivel, sin .cache/.verified)
// ---------------------------------------------------------------------------
/// Listar `.gguf` utilizables en `models/`: raíz + UN nivel de subdir
/// (`models/<sub>/x.gguf`). Excluye `models/.cache/` y `models/.verified/`
/// (artefactos internos, no modelos). Devuelve `(nombre, ruta, bytes)`.
pub fn list_gguf_files(models_dir: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(models_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if path.is_file() {
            if name.to_lowercase().ends_with(".gguf") && !name.to_lowercase().contains("mmproj") {
                let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push((name, path, bytes));
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        // Un nivel de subdir; fuera `.cache` y `.verified`; sin recursión
        // profunda (memoria acotada, sin sorpresas).
        if name.starts_with('.') || name == ".cache" || name == ".verified" {
            continue;
        }
        let sub = match std::fs::read_dir(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for s in sub.flatten() {
            let sp = s.path();
            if !sp.is_file() {
                continue;
            }
            let sn = sp
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if sn.to_lowercase().ends_with(".gguf") && !sn.to_lowercase().contains("mmproj") {
                let bytes = s.metadata().map(|m| m.len()).unwrap_or(0);
                out.push((sn, sp, bytes));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// import: primitiva compartida (wave-4: `import_model_from_path` en process.rs
// y `copy_picked` debajo usan ESTO; antes cada vía tenía su propia copia con
// propiedades distintas — parcial visible, sin self-check — según qué camino
// se auditara).
/// Error tipado de importación (ABIERTO-1 wave-6): el 409 (`Exists`) se mapea
/// por VARIANTE, nunca por substring del mensaje. Sin esto, un fichero
/// llamado `ya existe.txt` (u otro error con esas palabras en la ruta)
/// producía un 409 espurio donde tocaba 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// El destino ya existe y `overwrite=false` → HTTP 409.
    Exists(String),
    /// Nombre/extensión inválidos → HTTP 400.
    Invalid(String),
    /// Fallo de I/O (crear dir, copiar, renombrar) → HTTP 400.
    Io(String),
}
impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Exists(m) | ImportError::Invalid(m) | ImportError::Io(m) => {
                write!(f, "{}", m)
            }
        }
    }
}
impl ImportError {
    /// ¿Es colisión con `overwrite=false`? Único caso que mapea a HTTP 409.
    pub fn is_exists(&self) -> bool {
        matches!(self, ImportError::Exists(_))
    }
}
/// Validar nombre + resolver destino de un `.gguf` para `models_dir`.
/// Pura salvo `dest.exists()`/`canonicalize` (lecturas, sin escritura).
/// - `Err(Invalid)` si no es `.gguf` (case-insensitive) o el nombre es hostil.
/// - Self-import (`src` canónico == `dest` canónico) → `Ok((nombre, dest, true))`.
/// - `exists && !overwrite` → `Err(Exists)` (el handler→409 por variante).
pub fn resolve_import_dest(
    models_dir: &Path,
    source_path: &Path,
    overwrite: bool,
) -> Result<(String, PathBuf, bool), ImportError> {
    let name = source_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ImportError::Invalid("Nombre de archivo inválido".to_string()))?;
    if !name.to_lowercase().ends_with(".gguf") {
        return Err(ImportError::Invalid(format!("'{}' no es un .gguf", name)));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(ImportError::Invalid(format!("Nombre rechazado: {}", name)));
    }
    let dest = models_dir.join(&name);
    if let (Ok(src_c), Ok(dst_c)) = (source_path.canonicalize(), dest.canonicalize()) {
        if src_c == dst_c {
            return Ok((name, dest, true));
        }
    }
    if dest.exists() && !overwrite {
        return Err(ImportError::Exists(format!(
            "'{}' ya existe (marque sobreescribir para reemplazarlo)",
            name
        )));
    }
    Ok((name, dest, false))
}

/// Copia atómica tmp+rename a un destino ya resuelto. Jamás deja un parcial
/// en `dest`: el parcial vive en `<dest>.tmp-<pid>` (hermano, mismo volumen
/// → `rename` atómico en Windows; `list_models` solo enumera `*.gguf`, así
/// que nunca es visible/lanzable) y en cualquier fallo se limpia.
pub fn copy_atomic_tmp(src: &Path, dest: &Path) -> Result<(), ImportError> {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "modelo".to_string());
    let tmp = dest.with_extension(format!("tmp-{}", std::process::id()));
    if let Err(e) = std::fs::copy(src, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ImportError::Io(format!("Error al copiar {}: {}", name, e)));
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ImportError::Io(format!("Error al copiar {}: {}", name, e)));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// import_pick: diálogo nativo + copia (corrección de D-10)
/// Copiar ficheros elegidos a `models/`. Sin sobreescribir salvo `overwrite`.
/// Vía `resolve_import_dest` + `copy_atomic_tmp`: mismas garantías que la vía
/// `POST /api/import_model` (self-import no-op, tmp+rename sin parciales).
/// Devuelve los nombres copiados. SIN lock: el llamador (`import_pick` en el
/// handler corre en su hilo; `import_model_from_path` aporta el suyo) — para
/// `copy_picked` multi-fichero el lock vive en el llamador si lo necesita.
pub fn copy_picked(
    models_dir: &Path,
    files: &[PathBuf],
    overwrite: bool,
) -> Result<Vec<String>, ImportError> {
    if let Err(e) = std::fs::create_dir_all(models_dir) {
        return Err(ImportError::Io(format!(
            "No se pudo crear {}: {}",
            models_dir.display(),
            e
        )));
    }
    let mut done = Vec::new();
    for src in files {
        let (name, dest, self_import) = resolve_import_dest(models_dir, src, overwrite)?;
        if self_import {
            done.push(name);
            continue;
        }
        copy_atomic_tmp(src, &dest)?;
        done.push(name);
    }
    Ok(done)
}

/// Valida y resuelve el destino de importación de un modelo desde ruta local.
///
/// Delega en `resolve_import_dest` (misma resolución que el interior,
/// incluido self-import no-op): previo e interior nunca divergen.
/// - Si el fichero destino ya existe y `overwrite` es `false`, devuelve
///   `Err((409, mensaje))`.
/// - En caso de nombre o extensión inválidos, devuelve `Err((400, mensaje))`.
/// - Si es válido, devuelve `Ok((nombre_limpio, ruta_destino))`.
pub fn check_import_dest(
    models_dir: &Path,
    source_path: &Path,
    overwrite: bool,
) -> Result<(String, PathBuf), (u16, String)> {
    // UX temprana del handler: misma resolución que el interior (incluido el
    // self-import no-op de wave-4), así que previo e interior nunca divergen
    // (BUG-1 wave-4: el previo decía 409 donde el interior decía ok).
    match resolve_import_dest(models_dir, source_path, overwrite) {
        Ok((name, dest, _)) => Ok((name, dest)),
        Err(ImportError::Exists(m)) => Err((409, m)),
        Err(e) => Err((400, e.to_string())),
    }
}

/// Decisión de timeout del diálogo `import_pick`: espera acotada (120 s) para
/// no dejar el HTTP colgado si el usuario deja el diálogo abierto.
/// `true` = el plazo venció → responder `cancelled/timeout`.
pub const IMPORT_PICK_TIMEOUT_SECS: u64 = 120;

/// ¿Venció la espera del diálogo? Testeable sin humanos ni diálogos.
/// (Solo la usan los tests; en release la ruta usa la constante.)
#[allow(dead_code)]
pub fn import_pick_timed_out(elapsed_secs: u64) -> bool {
    elapsed_secs >= IMPORT_PICK_TIMEOUT_SECS
}

/// Enriquecer una ficha `ModelInfo` serializada con `size_bytes`, `sha256`,
/// `verified`, `source` y `capabilities` (sin tocar las claves existentes).
/// `capabilities` es heurística HONESTA por nombre de archivo (NO se lee el
/// header GGUF: decisión explícita, sin parser binario):
/// `{family, ctx_native, thinking}` donde `ctx_native` es el contexto nativo
/// conocido de la familia (`null` si se desconoce) y `thinking` si la familia
/// suele traer plantilla con razonamiento. La UI lo usa para adaptar
/// opciones; el motor confirma el `n_ctx` real por `/props` al arrancar.
pub fn model_capabilities(filename: &str) -> serde_json::Value {
    let low = filename.to_lowercase();
    // Familia (mismo vocabulario que el sampler del gateway).
    let family = if low.contains("qwen") || low.contains("qwq") || low.contains("bonsai") {
        "qwen"
    } else if low.contains("llama") {
        "llama"
    } else if low.contains("mistral") || low.contains("mixtral") {
        "mistral"
    } else {
        // `phi`/`gemma`/desconocido comparten capacidades genéricas.
        "generic"
    };
    // Contexto nativo por familia/generación conocida (null = se desconoce,
    // la UI no limita y el motor manda por /props).
    let ctx_native: Option<usize> = if low.contains("qwen3") || low.contains("bonsai") {
        Some(262144)
    } else if low.contains("qwen2.5")
        || low.contains("llama-3.1")
        || low.contains("llama-3.2")
        || low.contains("llama3.1")
    {
        Some(131072)
    } else if low.contains("llama-3") || low.contains("llama3") {
        Some(8192)
    } else if low.contains("mistral") && (low.contains("v0.3") || low.contains("0.3")) {
        Some(131072)
    } else if low.contains("mistral") || low.contains("mixtral") {
        Some(32768)
    } else if low.contains("phi-4") || low.contains("gemma-3") {
        Some(131072)
    } else {
        None
    };
    // Thinking: familias con plantilla de razonamiento conocida.
    let thinking = low.contains("qwen3") || low.contains("qwq") || low.contains("bonsai");
    serde_json::json!({
        "family": family,
        "ctx_native": ctx_native,
        "thinking": thinking,
    })
}
pub fn enrich_model_entry(
    models_dir: &Path,
    mut v: serde_json::Value,
    size_bytes: u64,
) -> serde_json::Value {
    let filename = v
        .get("filename")
        .and_then(|f| f.as_str())
        .unwrap_or("")
        .to_string();
    let rec = if filename.is_empty() {
        None
    } else {
        read_verified(models_dir, &filename)
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "size_bytes".to_string(),
            serde_json::Value::from(size_bytes),
        );
        obj.insert(
            "sha256".to_string(),
            rec.as_ref()
                .map(|r| serde_json::Value::from(r.sha256.clone()))
                .unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "verified".to_string(),
            serde_json::Value::from(rec.is_some()),
        );
        obj.insert(
            "source".to_string(),
            rec.map(|r| {
                serde_json::Value::from(if r.repo.is_empty() {
                    "local".to_string()
                } else {
                    r.repo
                })
            })
            .unwrap_or_else(|| serde_json::Value::from("local".to_string())),
        );
        obj.insert("capabilities".to_string(), model_capabilities(&filename));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_vector_conocido() {
        // Vector FIPS 180-4: sha256("abc").
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn arbol_parse_y_rutas_rechazadas() {
        let body = serde_json::json!([
            {"type": "file", "path": "model.gguf", "size": 100,
             "lfs": {"oid": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}},
            {"type": "file", "path": "config.json", "size": 50},
            {"type": "directory", "path": "snapshots", "size": 0}
        ]);
        let files = parse_tree(&body).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].oid.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert!(files[1].oid.is_none());
        // Rutas peligrosas: el parse las rechaza.
        let evil = serde_json::json!([{"type": "file", "path": "../fuera.gguf", "size": 1}]);
        assert!(parse_tree(&evil).is_err());
        let abs = serde_json::json!([{"type": "file", "path": "/abs.gguf", "size": 1}]);
        assert!(parse_tree(&abs).is_err());
        assert!(check_repo("owner/name").is_ok());
        assert!(check_repo("sin-barra").is_err());
        assert!(check_revision(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad0123"
        )
        .is_err());
        assert!(check_revision("abc").is_err());
        assert!(check_revision("0000000000000000000000000000000000000000").is_ok());
    }

    #[test]
    fn registro_verificado_roundtrip() {
        let dir = std::env::temp_dir().join(format!("lm-verify-{}", std::process::id()));
        let models = dir.join("models");
        let _ = std::fs::create_dir_all(&models);
        let rec = VerifiedRecord {
            sha256: "abc123".to_string(),
            size: 42,
            repo: "owner/name".to_string(),
            revision: "0000000000000000000000000000000000000000".to_string(),
            at: 123,
        };
        write_verified(&models, "m.gguf", &rec);
        let back = read_verified(&models, "m.gguf").unwrap();
        assert_eq!(back.sha256, "abc123");
        assert_eq!(back.size, 42);
        assert_eq!(back.repo, "owner/name");
        // Ficha enriquecida: conserva claves y añade las nuevas.
        let base =
            serde_json::json!({"filename": "m.gguf", "name": "m", "size_gb": 0.0, "path": "x"});
        let enriched = enrich_model_entry(&models, base, 42);
        assert_eq!(enriched["filename"], serde_json::json!("m.gguf"));
        assert_eq!(enriched["size_bytes"], serde_json::json!(42));
        assert_eq!(enriched["verified"], serde_json::json!(true));
        assert_eq!(enriched["source"], serde_json::json!("owner/name"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Capacidades por nombre: Qwen3→262K+thinking, Llama3.1→128K sin
    /// thinking, desconocido→null sin thinking (la UI no limita, el motor
    /// confirma por /props).
    #[test]
    fn capacidades_por_nombre_honestas() {
        let q = model_capabilities("Qwen3.8-27B-IQ4_XS_4BPW.gguf");
        assert_eq!(q["family"], serde_json::json!("qwen"));
        assert_eq!(q["ctx_native"], serde_json::json!(262144));
        assert_eq!(q["thinking"], serde_json::json!(true));
        let l = model_capabilities("Llama-3.1-8B-Q4_K_M.gguf");
        assert_eq!(l["family"], serde_json::json!("llama"));
        assert_eq!(l["ctx_native"], serde_json::json!(131072));
        assert_eq!(l["thinking"], serde_json::json!(false));
        let g = model_capabilities("Algo-Raro-13B-Q5.gguf");
        assert_eq!(g["family"], serde_json::json!("generic"));
        assert!(g["ctx_native"].is_null());
        assert_eq!(g["thinking"], serde_json::json!(false));
    }

    /// El asesor remoto informa tamaño, no concluye ajuste sin metadatos GGUF.
    #[test]
    fn asesor_remoto_no_finge_estimacion_de_kv() {
        let tree = vec![
            TreeFile {
                path: "grande.gguf".to_string(),
                size: 14336 * 1024 * 1024,
                oid: None,
            },
            TreeFile {
                path: "config.json".to_string(),
                size: 1000,
                oid: None,
            },
        ];
        let v = advise_fit(&tree, None, Some(16384), Some(32768));
        assert_eq!(v["verdict"], serde_json::json!("unknown"));
        assert_eq!(v["size_mb"], serde_json::json!(14336));
        assert_eq!(v["weight_only_budget_mb"], serde_json::json!(14745));
        assert_eq!(v["weight_only_exceeds_budget"], serde_json::json!(false));
        assert!(v["detail"].as_str().unwrap().contains("KV activo"));
        assert!(v["detail"].as_str().unwrap().contains("caché de prompts"));

        let large = vec![TreeFile {
            path: "large.gguf".to_string(),
            size: 9000 * 1024 * 1024,
            oid: None,
        }];
        let v = advise_fit(&large, None, Some(8192), Some(32768));
        assert_eq!(v["verdict"], serde_json::json!("unknown"));
        assert_eq!(v["weight_only_exceeds_budget"], serde_json::json!(true));

        let v = advise_fit(&tree, None, None, Some(32768));
        assert_eq!(v["verdict"], serde_json::json!("unknown"));
        assert!(v["weight_only_budget_mb"].is_null());

        let v = advise_fit(
            &[TreeFile {
                path: "a.json".to_string(),
                size: 1,
                oid: None,
            }],
            None,
            Some(16384),
            None,
        );
        assert_eq!(v["verdict"], serde_json::json!("no_gguf"));
        let v = advise_fit(&tree, Some("otro.gguf"), Some(16384), None);
        assert_eq!(v["verdict"], serde_json::json!("no_file"));
    }
    #[test]
    fn estado_sin_nan_y_destino_dentro() {
        let idle = JobState::idle();
        assert_eq!(idle.state, "idle");
        assert_eq!(idle.percent(), 0);
        let json: serde_json::Value = serde_json::from_str(&idle.json()).unwrap();
        assert_eq!(json["percent"], serde_json::json!(0));
        assert!(!json.to_string().contains("NaN"));
        // Destino: UN solo .gguf (aunque venga en subcarpeta) → plano en
        // models/<fichero>; multi → models/<repo>/.
        let root = PathBuf::from("C:/m/models");
        let one = vec![TreeFile {
            path: "a.gguf".to_string(),
            size: 1,
            oid: None,
        }];
        assert_eq!(dest_for(&root, "o/r", &one).unwrap(), root.join("a.gguf"));
        let nested_one = vec![TreeFile {
            path: "tinyllamas/stories260K.gguf".to_string(),
            size: 1,
            oid: None,
        }];
        assert_eq!(
            dest_for(&root, "ggml-org/models", &nested_one).unwrap(),
            root.join("stories260K.gguf")
        );
        let multi = vec![
            TreeFile {
                path: "a.gguf".to_string(),
                size: 1,
                oid: None,
            },
            TreeFile {
                path: "b.json".to_string(),
                size: 1,
                oid: None,
            },
        ];
        assert_eq!(dest_for(&root, "o/r", &multi).unwrap(), root.join("r"));
        // Escape sigue rechazado.
        assert!(check_inside(&root, &PathBuf::from("C:/m/models/../fuera")).is_err());
    }

    /// Misma clase que el ZipSlip de `stage_zip` (2026-10-10): la vieja
    /// `check_inside` comparaba longitudes, así que una ruta fuera del root
    /// pero igual o más larga pasaba como "contenida". Hoy se exige prefijo
    /// real (`starts_with` sobre rutas normalizadas).
    #[test]
    fn check_inside_exige_prefijo() {
        let root = PathBuf::from("C:/m/models");
        // Fuera del root pero larga: el chequeo por longitud la aceptaba.
        assert!(check_inside(&root, &PathBuf::from("C:/m/models-vecina/x.gguf")).is_err());
        assert!(check_inside(&root, &PathBuf::from("C:/m/otro/camino/largo/x.gguf")).is_err());
        // Dentro: pasa (plano y anidado).
        assert!(check_inside(&root, &root.join("a.gguf")).is_ok());
        assert!(check_inside(&root, &root.join("sub").join("b.gguf")).is_ok());
    }

    /// P1 producción: el token persistido queda con la ACL del dueño (vía
    /// `auth::restrict_key_file`, mismo endurecer que `gateway.key`) y el
    /// contenido coincide. En Linux `icacls` no existe: el aviso va por
    /// stderr pero el guardado sigue válido.
    #[test]
    fn save_hf_token_escribe_y_relee() {
        let dir = std::env::temp_dir().join(format!("lm-hftok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("LocalMind").join("hf.token");
        save_hf_token_at(&path, "  tok-secreto-1  ").expect("guarda");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tok-secreto-1\n");
        // Re-guardar idempotente: mismo contenido, sin error.
        save_hf_token_at(&path, "tok-secreto-1").expect("re-guarda");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tok-secreto-1\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listado_anidado_sin_cache_ni_verified() {
        let dir = std::env::temp_dir().join(format!("lm-list-{}", std::process::id()));
        let models = dir.join("models");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(models.join("sub"));
        let _ = std::fs::create_dir_all(models.join(".cache"));
        let _ = std::fs::create_dir_all(models.join(".verified"));
        let _ = std::fs::write(models.join("plano.gguf"), b"a");
        let _ = std::fs::write(models.join("sub").join("anidado.gguf"), b"bb");
        let _ = std::fs::write(models.join(".cache").join("oculto.gguf"), b"ccc");
        let _ = std::fs::write(models.join(".verified").join("registro.gguf"), b"dddd");
        let _ = std::fs::write(models.join("nota.txt"), b"no es modelo");
        let found = list_gguf_files(&models);
        let names: Vec<&str> = found.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"plano.gguf"), "raíz: {:?}", names);
        assert!(names.contains(&"anidado.gguf"), "un nivel: {:?}", names);
        assert!(
            !names.contains(&"oculto.gguf"),
            ".cache excluido: {:?}",
            names
        );
        assert!(
            !names.contains(&"registro.gguf"),
            ".verified excluido: {:?}",
            names
        );
        assert!(!names.contains(&"nota.txt"), "solo .gguf: {:?}", names);
        assert_eq!(found.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_pick_timeout_decide() {
        assert!(!import_pick_timed_out(0));
        assert!(!import_pick_timed_out(IMPORT_PICK_TIMEOUT_SECS - 1));
        assert!(import_pick_timed_out(IMPORT_PICK_TIMEOUT_SECS));
        assert!(import_pick_timed_out(IMPORT_PICK_TIMEOUT_SECS + 999));
        assert_eq!(IMPORT_PICK_TIMEOUT_SECS, 120);
    }

    #[test]
    fn check_import_dest_overwrite_y_case_insensitive() {
        let dir = std::env::temp_dir().join(format!("lm-dest-{}", std::process::id()));
        let models = dir.join("models");
        let _ = std::fs::create_dir_all(&models);

        // 1. Caso insensitive pasa (.GGUF mayúsculas).
        let src_upper = PathBuf::from("C:/downloads/Model-Q4.GGUF");
        let (name, dest) = check_import_dest(&models, &src_upper, false).unwrap();
        assert_eq!(name, "Model-Q4.GGUF");
        assert_eq!(dest, models.join("Model-Q4.GGUF"));

        // 2. Extensión no gguf da 400.
        let src_bad = PathBuf::from("C:/downloads/Model.bin");
        let err_bad = check_import_dest(&models, &src_bad, false).unwrap_err();
        assert_eq!(err_bad.0, 400);
        assert!(err_bad.1.contains("no es un .gguf"));

        // 3. Destino existente sin overwrite da 409.
        let target = models.join("existente.gguf");
        let _ = std::fs::write(&target, b"dummy");
        let src_exist = PathBuf::from("D:/otra/existente.gguf");
        let err_exist = check_import_dest(&models, &src_exist, false).unwrap_err();
        assert_eq!(err_exist.0, 409);
        assert!(err_exist.1.contains("ya existe"));

        // 4. Con overwrite=true pasa aunque exista.
        let ok_overwrite = check_import_dest(&models, &src_exist, true).unwrap();
        assert_eq!(ok_overwrite.0, "existente.gguf");

        // 5. ABIERTO-1 wave-6: un nombre con "ya existe" en OTRO error no da
        // 409 espurio — el mapeo es por variante, no por substring.
        let src_trampa = PathBuf::from("C:/downloads/ya existe.txt");
        let err_trampa = check_import_dest(&models, &src_trampa, false).unwrap_err();
        assert_eq!(err_trampa.0, 400, "falso positivo 409: {:?}", err_trampa);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wave-4 B1: `copy_picked` (vía `import_pick`) con las mismas garantías
    /// que `import_model_from_path`: self-import no-op, 409 sin overwrite,
    /// tmp+rename sin parciales visibles.
    #[test]
    fn copy_picked_atomico_sin_parciales() {
        let dir = std::env::temp_dir().join(format!("lm-picked-{}", std::process::id()));
        let models = dir.join("models");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&models);
        let f1 = dir.join("a.gguf");
        let f2 = dir.join("b.gguf");
        std::fs::write(&f1, b"gguf-a").unwrap();
        std::fs::write(&f2, b"gguf-b").unwrap();
        let names = copy_picked(&models, &[f1.clone(), f2], false).unwrap();
        assert_eq!(names, vec!["a.gguf".to_string(), "b.gguf".to_string()]);
        // Sin overwrite, el segundo intento falla con "ya existe".
        let err = copy_picked(&models, std::slice::from_ref(&f1), false).unwrap_err();
        assert!(err.is_exists(), "{}", err);
        // Con overwrite, pisa sin dejar tmps.
        std::fs::write(&f1, b"gguf-a2").unwrap();
        assert_eq!(
            copy_picked(&models, std::slice::from_ref(&f1), true).unwrap(),
            vec!["a.gguf".to_string()]
        );
        assert_eq!(std::fs::read(models.join("a.gguf")).unwrap(), b"gguf-a2");
        // Self-import: el fichero que ya vive en models/ es no-op.
        let ya = models.join("b.gguf");
        assert_eq!(
            copy_picked(&models, std::slice::from_ref(&ya), false).unwrap(),
            vec!["b.gguf".to_string()]
        );
        // Sin parciales: ningún `.tmp-*` en models/.
        let restos: Vec<_> = std::fs::read_dir(&models)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .filter(|n| n.contains("tmp-"))
            .collect();
        assert!(restos.is_empty(), "tmps sin limpiar: {:?}", restos);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Candado para los tests que tocan `busy`/`busy_since_secs` globales:
    /// `cargo test` corre en hilos y sin esto se pisan entre sí.
    static TEST_JOB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Watchdog (P1 producción, espejo de `update.rs`): clavado hace 11 min
    /// se recupera; vivo se respeta; `release_busy` deja libre.
    #[test]
    fn job_watchdog_recupera_clavado_y_respeta_vivo() {
        let _g = TEST_JOB_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        job().busy.store(false, Ordering::SeqCst);
        assert!(take_busy_or_recover());
        assert!(!take_busy_or_recover());
        job()
            .busy_since_secs
            .store(now_secs().saturating_sub(660), Ordering::Relaxed);
        assert!(take_busy_or_recover());
        heartbeat_busy();
        assert!(!take_busy_or_recover(), "tras latido el busy está vivo");
        release_busy();
        assert!(!job_busy());
    }

    /// Gemelo del P0 de update.rs: `snippet` no debe partir un char multibyte
    /// (tildes/emoji en errores HF) aunque el corte caiga a medias.
    #[test]
    fn snippet_no_corta_utf8_a_medias() {
        let s = format!("{}é", "a".repeat(299));
        let out = snippet(&s);
        assert!(out.chars().count() <= 301, "{}", out.chars().count());
        let e = "😀".repeat(400);
        let out2 = snippet(&e);
        assert_eq!(out2.chars().count(), 301, "300 + …");
    }
    /// P1 producción: el mensaje 507 nombra los GB y la regla 2× se hereda
    /// de `update::fits_download` (sin dato no se bloquea).
    #[test]
    fn espacio_mensaje_y_regla_doble() {
        let msg = models_no_space_msg(15 * 1024 * 1024 * 1024);
        assert!(msg.contains("Sin espacio"), "{}", msg);
        assert!(msg.contains("15.0 GB"), "{}", msg);
        assert!(msg.contains("doble"), "{}", msg);
        assert_eq!(
            crate::update::fits_download(Some(100), Some(199)),
            Some(false)
        );
        assert!(models_space_ok(&std::env::temp_dir(), u64::MAX) != Some(true));
    }
}
