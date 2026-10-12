//! Servidor HTTP LocalMind: UI + `/api/*` + proxy OpenAI/Anthropic/Responses.
//!
//! Cambios de esta fase (ver reporte):
//! - D-7: clave local en `%APPDATA%\LocalMind\gateway.key`; `/v1/*` y `/api/*`
//!   exigen Bearer/x-api-key/cookie; `/`, `/index.html`, iconos siguen públicos
//!   y fijan cookie `lm_key` para la WebView.
//! - D-5/D-6: lanzadores pi/omp escriben configs privadas en
//!   `%APPDATA%\LocalMind\agents\<agent>\` desde el estado VIVO del motor;
//!   jamás tocan `%USERPROFILE%\.pi` ni `%USERPROFILE%\.omp`.
//! - D-2: alias `localmind`/`qwen3.8-27b` + `GET /v1/models`; cualquier `model`
//!   pedido se reescribe al id servido.
//! - Claude Code / Codex: `POST /v1/messages` y `/v1/responses` se reenvían
//!   al motor nativo (auditoría: la traducción perdía thinking).
//! - Telemetría base: `usage.jsonl` por request proxyeado completado.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tiny_http::{Header, Response, Server, StatusCode};

use crate::config::ConfigStore;
use crate::process::{LogEvent, ProcessManager, ServerStatus, StartRequest};

pub struct HttpServer {
    port: u16,
}

/// Cabeceras CORS fijas (sin `Origin`): métodos y headers permitidos.
/// `Access-Control-Allow-Origin` ya NO se envía con `*` (SRS P29): solo se
/// refleja el `Origin` permitido por la allowlist (`cors_origin_header`:
/// loopback siempre, red privada solo en modo LAN). Las lecturas de la
/// WebView (mismo origen) no necesitan CORS en absoluto.
fn cors_fixed_headers() -> [Header; 2] {
    [
        Header::from_bytes(
            &b"Access-Control-Allow-Methods"[..],
            &b"GET, POST, OPTIONS, PUT, DELETE"[..],
        )
        .unwrap(),
        Header::from_bytes(
            &b"Access-Control-Allow-Headers"[..],
            &b"Content-Type, Authorization, x-api-key"[..],
        )
        .unwrap(),
    ]
}

/// `Access-Control-Allow-Origin: <origen>` según la allowlist vigente (Fase B2,
/// diseño B3): loopback como siempre (P29), más red privada cuando el proceso
/// arrancó en modo LAN. El flag se lee del interruptor global que fijó
/// `HttpServer::start` (ver `meta::set_lan_mode`): con `lan.enabled=false`
/// esta función es idéntica a la histórica.
fn cors_origin_header(origin: Option<&str>) -> Option<Header> {
    let o = origin?;
    if !crate::meta::origen_permitido(o, crate::meta::lan_mode()) {
        return None;
    }
    Header::from_bytes(&b"Access-Control-Allow-Origin"[..], o.as_bytes()).ok()
}

/// `Set-Cookie` de sesión. El valor sale de la clave local (64 hex de
/// `generate_key()` o del `gateway.key` de disco), así que es DINÁMICO: una
/// clave alterada con un byte de control haría fallar el parseo. Se devuelve
/// `Option` —igual que `cors_origin_header`— para que el llamante responda 500
/// en vez de entrar en `unwrap` con `panic = "abort"`.
fn session_cookie_header(gateway_key: &str) -> Option<Header> {
    Header::from_bytes(
        &b"Set-Cookie"[..],
        crate::auth::set_cookie_value(gateway_key).as_bytes(),
    )
    .ok()
}

/// Carpeta de datos para `POST /api/open_data_dir` (P1-7 producción).
/// Allowlist cerrada: `appdata` y `temp`. Cualquier otro valor → `None`
/// (el handler responde 400). Pura y testeable.
fn data_dir_for(which: &str) -> Option<std::path::PathBuf> {
    match which {
        // Misma resolución que `filelog::logs_dir` (APPDATA, si no temp).
        "appdata" => Some(
            std::env::var("APPDATA")
                .map(|p| std::path::Path::new(&p).join("LocalMind"))
                .unwrap_or_else(|_| std::env::temp_dir().join("LocalMind")),
        ),
        "temp" => Some(std::env::temp_dir().join("omni-update")),
        _ => None,
    }
}

fn json_response_for_origin(
    status_code: u16,
    body: String,
    origin: Option<&str>,
) -> Response<Cursor<Vec<u8>>> {
    let mut resp = Response::from_string(body).with_status_code(StatusCode(status_code));
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    resp.add_header(ct);
    for h in cors_fixed_headers() {
        resp.add_header(h);
    }
    if let Some(o) = cors_origin_header(origin) {
        resp.add_header(o);
    }
    resp
}

fn unauthorized_json_for_origin(origin: Option<&str>) -> Response<Cursor<Vec<u8>>> {
    json_response_for_origin(401, r#"{"error":"unauthorized"}"#.to_string(), origin)
}

// ---------------------------------------------------------------------------
// Parseo y validación de cuerpos POST (puros, testeables sin socket)
// ---------------------------------------------------------------------------
// conventions: `Result<_, String>` con mensajes en español, sin I/O y sin
// `unwrap`: todo fallo del cliente es un 4xx que el handler convierte en
// respuesta.

/// Tope para POST pequeños con JSON (`/api/unlock`, `/api/import_model`):
/// 64 KiB. Puro y testeable: `bytes` ya leídos con `Take(max+1)` para detectar
/// el exceso sin cargar más; `Err` en español → el handler responde 413.
///
/// El proxy `/v1/*` usa `MAX_V1_BODY` (25 MB), no este tope: necesita el
/// cuerpo completo (prompts de MB) pero con cota anti-OOM; el límite fino es
/// el del motor (contexto/modelo). Todo exige clave por la puerta D-7.
pub(crate) const MAX_SMALL_JSON_BODY: usize = 64 * 1024;
///
/// Tope del cuerpo `/v1/*` (P1 producción): 25 MB. El proxy necesita el
/// cuerpo completo (prompts de MB), pero sin cota un cliente LAN puede
/// OOMear el gateway thread-per-request. 25 MB cubre cualquier prompt real
/// (el contexto máximo es 262K tokens ≈ 1 MB de texto); el exceso → 413.
pub(crate) const MAX_V1_BODY: usize = 25 * 1024 * 1024;
///
/// Lee hasta `max + 1` bytes crudos y valida con `limit_body`.
/// `Ok(bytes)` si cabe; `Err((413, msg))` si excede; `Err((400, msg))` en I/O.
fn read_capped_bytes(reader: &mut dyn Read, max: usize) -> Result<Vec<u8>, (u16, String)> {
    let mut buf = Vec::new();
    reader
        .take((max as u64) + 1)
        .read_to_end(&mut buf)
        .map_err(|e| (400, format!("error de lectura: {}", e)))?;
    if let Err(e) = limit_body(&buf, max) {
        return Err((413, e));
    }
    Ok(buf)
}
/// ¿Cabe `bytes` en `max`? `Err` si excede (mensaje en español, sin I/O).
pub(crate) fn limit_body(bytes: &[u8], max: usize) -> Result<(), String> {
    if bytes.len() > max {
        return Err(format!(
            "cuerpo demasiado grande: {} bytes (máximo {} bytes)",
            bytes.len(),
            max
        ));
    }
    Ok(())
}
/// Lee hasta `max + 1` bytes de un reader y valida con `limit_body`.
/// `Ok(cadena)` si cabe y es UTF-8; `Err(413, msg)` si excede;
/// `Err(400, msg)` si no es UTF-8 o hay error de I/O.
fn read_bounded_body(reader: &mut dyn Read, max: usize) -> Result<String, (u16, String)> {
    let mut buf = Vec::new();
    reader
        .take((max as u64) + 1)
        .read_to_end(&mut buf)
        .map_err(|e| (400, format!("error de lectura: {}", e)))?;
    if let Err(e) = limit_body(&buf, max) {
        return Err((413, e));
    }
    String::from_utf8(buf).map_err(|_| (400, "el cuerpo no es UTF-8 válido".to_string()))
}
///
/// `POST /api/start`: un cuerpo MAL FORMADO no puede degradarse a un arranque
fn parse_start_body(body: &str) -> Result<StartRequest, String> {
    if body.trim().is_empty() {
        return Ok(StartRequest {
            model: None,
            profile: None,
            context: None,
            threads: None,
            priority: None,
        });
    }
    serde_json::from_str::<StartRequest>(body).map_err(|e| format!("JSON inválido: {}", e))
}

/// `POST /api/profiles/import`: el import NO puede saltarse los validadores que
/// `/api/profiles/save` sí aplica. Reusa los mismos predicados y nombra el
/// campo culpable, en el mismo formato `campo inválido: <campo>`.
fn parse_profiles_import(body: &str) -> Result<Vec<crate::config::HardwareProfile>, String> {
    let imported: Vec<crate::config::HardwareProfile> =
        serde_json::from_str(body).map_err(|e| format!("JSON inválido: {}", e))?;
    if imported.is_empty() {
        return Err("La lista de perfiles está vacía".to_string());
    }
    for p in &imported {
        let bad = |campo: &str| format!("campo inválido: {}", campo);
        if !crate::config::profile_id_ok(&p.id) {
            return Err(bad("id"));
        }
        if !crate::config::profile_context_ok(p.context) {
            return Err(bad("context"));
        }
        if !crate::config::profile_cache_ram_ok(p.cache_ram) {
            return Err(bad("cache_ram"));
        }
        if !crate::config::profile_flags_ok(&p.extra_flags) {
            return Err(bad("extra_flags"));
        }
        if let Some(err) = crate::config::psu_unsafe_flag(&p.extra_flags) {
            return Err(err);
        }
    }
    Ok(imported)
}

/// Parámetros de `POST /api/import_model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportModelRequest {
    pub path: String,
    pub overwrite: bool,
}

/// Parseo de `POST /api/import_model`. Exige objeto con `path` no vacío.
/// `overwrite` es opcional (default `false`).
pub(crate) fn parse_import_model_body(body: &str) -> Result<ImportModelRequest, String> {
    let val: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("JSON inválido: {}", e))?;
    let obj = val
        .as_object()
        .ok_or_else(|| "cuerpo inválido: se esperaba un objeto JSON".to_string())?;
    let path = obj
        .get("path")
        .and_then(|p| p.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Falta el parámetro 'path'".to_string())?
        .to_string();
    let overwrite = obj
        .get("overwrite")
        .and_then(|o| o.as_bool())
        .unwrap_or(false);
    Ok(ImportModelRequest { path, overwrite })
}

/// Añadir CORS a una respuesta ya construida: fijas + `Origin` si es loopback.
fn add_cors_for<R: Read>(resp: &mut Response<R>, origin: Option<&str>) {
    if let Some(o) = cors_origin_header(origin) {
        resp.add_header(o);
    }
    for h in cors_fixed_headers() {
        resp.add_header(h);
    }
}

fn status_json(st: ServerStatus) -> String {
    serde_json::to_string(&st).unwrap_or_else(|_| "{}".to_string())
}

fn app_config_json(c: &crate::config::AppConfig) -> String {
    // Forma que la UI espera (`GET /api/config`, `POST` devuelve lo mismo), con
    // datos de solo lectura que no se aceptan en el POST, como ubatch/device/puertos.
    // El fingerprint usa una tupla ordenada de opciones que cambian el argv o
    // el trabajo del motor. Expone solo el hash, nunca los valores de extra_flags.
    let mut profile_runtime_settings: Vec<_> = c
        .profiles
        .iter()
        .map(|p| (&p.id, p.context, p.cache_ram, &p.extra_flags))
        .collect();
    profile_runtime_settings.sort_by(|a, b| a.0.cmp(b.0));
    let runtime_settings = (
        (
            &c.engine.device,
            c.engine.threads,
            c.engine.threads_batch,
            &c.engine.priority,
            &c.engine.priority_batch,
            c.engine.batch,
            &c.engine.process_priority,
            c.engine.poll,
            c.engine.limit_threads_batch,
            c.engine.power_safe,
            &c.engine.extra_flags,
            &c.engine.speculation,
        ),
        (
            c.engine.flash_attention,
            c.engine.metrics,
            c.engine.reasoning_preserve,
            c.mmproj.auto,
            &c.mmproj.files,
            &c.last.model,
            &c.last.profile,
            c.last.context,
        ),
        profile_runtime_settings,
    );
    let runtime_fingerprint_sha256 =
        crate::models::sha256_hex(&serde_json::to_vec(&runtime_settings).unwrap_or_default());
    serde_json::json!({
        "engine": {
            "idle_timeout_secs": c.engine.idle_timeout_secs,
            "threads": c.engine.threads,
            "priority": c.engine.priority,
            "speculation_enabled": c.engine.speculation.as_ref().is_some_and(|s| s.enabled),
            "batch": c.engine.batch,
            "ubatch": 512,
            "device": c.engine.device,
            "runtime_fingerprint_sha256": runtime_fingerprint_sha256,
            "extra_flags_count": c.engine.extra_flags.len(),
            "llama_port": c.engine.llama_port,
            "http_port": c.engine.http_port,
        },
        "generation": {
            "temperature": c.generation.temperature,
            "top_p": c.generation.top_p,
            "max_tokens": c.generation.max_tokens,
            "seed": c.generation.seed,
        },
        "notifications": {
            "enabled": c.notifications.enabled,
            "on_ready": c.notifications.on_ready,
            "on_failure": c.notifications.on_failure,
            "on_autostop": c.notifications.on_autostop,
        },
        // Fase C: rol de la app. `remote.key` jamás sale por acá (diseño B6).
        "lan": {
            "enabled": c.lan.enabled,
        },
        "client": {
            "enabled": c.client.enabled,
        },
        "remote": {
            "url": c.remote.url,
        },
        "update": {
            "feed": c.update.feed,
            "check_on_startup": c.update.check_on_startup,
            "auto_download": c.update.auto_download,
        },
    })
    .to_string()
}

fn settings_json(
    cfg: &crate::config::AppConfig,
    config_path: &std::path::Path,
    http_port: u16,
) -> String {
    let body = serde_json::json!({
        "last": cfg.last,
        "config_path": config_path.to_string_lossy(),
        "http_port": http_port,
        "llama_port": cfg.engine.llama_port,
    });
    body.to_string()
}
struct Metrics {
    gen_tps: Option<f64>,
    prompt_tps: Option<f64>,
    tokens_pred: Option<f64>,
    prompt_tot: Option<f64>,
    cached_tot: Option<f64>,
    requests_processing: Option<f64>,
    draft_tot: Option<f64>,
    accepted_tot: Option<f64>,
}

fn parse_metrics(text: &str) -> Metrics {
    let mut m = Metrics {
        gen_tps: None,
        prompt_tps: None,
        tokens_pred: None,
        prompt_tot: None,
        cached_tot: None,
        requests_processing: None,
        draft_tot: None,
        accepted_tot: None,
    };
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let (key, val) = match line.rsplit_once(' ') {
            Some(kv) => kv,
            None => continue,
        };
        let key = key.split('{').next().unwrap_or(key).trim();
        let val: f64 = match val.trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        macro_rules! set {
            ($field:ident, $name:literal) => {
                if key.starts_with($name) && m.$field.is_none() {
                    m.$field = Some(val);
                }
            };
        }
        set!(gen_tps, "llamacpp:predicted_tokens_seconds");
        set!(prompt_tps, "llamacpp:prompt_tokens_seconds");
        set!(tokens_pred, "llamacpp:tokens_predicted_total");
        set!(prompt_tot, "llamacpp:prompt_tokens_total");
        set!(cached_tot, "llamacpp:prompt_tokens_cached_total");
        set!(requests_processing, "llamacpp:requests_processing");
        set!(draft_tot, "llamacpp:spec_decode_num_draft_tokens_total");
        set!(
            accepted_tot,
            "llamacpp:spec_decode_num_accepted_tokens_total"
        );
    }
    m
}

/// Cargar un recurso estático (`localmind.ico`, `localmind.png`).
/// `None` → el handler responde 404 `{"error":"not_found"}`.
fn load_asset(base_dir: &std::path::Path, name: &str) -> Option<Vec<u8>> {
    std::fs::read(base_dir.join(name)).ok()
}

/// Mapea un `reasoning_effort` al vocabulario de la plantilla Qwen
/// (`low`/`medium`/`xhigh`) más `none` (valor documentado del servidor para
/// desactivar el thinking; `off` se acepta como alias). `None` = desconocido
/// → el llamador elimina el campo.
fn normalize_reasoning_effort(v: &str) -> Option<String> {
    if v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("off") {
        Some("none".to_string())
    } else if v.eq_ignore_ascii_case("minimal") || v.eq_ignore_ascii_case("low") {
        Some("low".to_string())
    } else if v.eq_ignore_ascii_case("high")
        || v.eq_ignore_ascii_case("max")
        || v.eq_ignore_ascii_case("xhigh")
    {
        Some("xhigh".to_string())
    } else if v.eq_ignore_ascii_case("medium") {
        Some("medium".to_string())
    } else {
        None
    }
}

/// Familia del modelo servido por su nombre de archivo (portabilidad): el
/// sampler de respaldo debe parecerse a la model card del modelo, no imponer
/// la de Qwen a un Llama/Mistral. Qwen es el fallback (compat con lo medido:
/// el 100% de las evals de esta máquina son Qwen/Bonsai).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFamily {
    Qwen,
    Llama,
    Mistral,
    Generic,
}

/// Puro y testeable: `qwen|qwq` → Qwen; `llama` → Llama; `mistral|mixtral` →
/// Mistral; resto/vacío → Generic (= Qwen como fallback medido, documentado).
fn model_family(served: &str) -> ModelFamily {
    let low = served.to_lowercase();
    if low.contains("qwen") || low.contains("qwq") || low.contains("bonsai") {
        ModelFamily::Qwen
    } else if low.contains("llama") {
        ModelFamily::Llama
    } else if low.contains("mistral") || low.contains("mixtral") {
        ModelFamily::Mistral
    } else {
        ModelFamily::Generic
    }
}

/// Sampler de respaldo por familia y modo (solo rellena claves AUSENTES: lo
/// explícito siempre gana). Qwen = model card `Qwen/Qwen3.8-27B` (medido
/// aquí); Llama = defaults Meta (temp 0.6/0.6, top_p 0.9, sin presence);
/// Mistral = guía Mistral (temp 0.7, top_p 1.0); Generic = Qwen (fallback).
fn sampler_defaults(family: ModelFamily, thinking: bool) -> [(&'static str, serde_json::Value); 5] {
    match family {
        ModelFamily::Qwen | ModelFamily::Generic => qwen_sampler_defaults(thinking),
        // Llama/Mistral: sus guías no varían por modo thinking/instruct, así
        // que una sola tabla por familia (sin ramas idénticas).
        ModelFamily::Llama => [
            ("temperature", serde_json::json!(0.6)),
            ("top_k", serde_json::json!(40)),
            ("top_p", serde_json::json!(0.9)),
            ("min_p", serde_json::json!(0.0)),
            ("presence_penalty", serde_json::json!(0.0)),
        ],
        ModelFamily::Mistral => [
            ("temperature", serde_json::json!(0.7)),
            ("top_k", serde_json::json!(40)),
            ("top_p", serde_json::json!(1.0)),
            ("min_p", serde_json::json!(0.0)),
            ("presence_penalty", serde_json::json!(0.0)),
        ],
    }
}

/// ¿El effort pedido es incompatible con el modelo servido? (puro, testeable).
/// Las familias con thinking conocido son Qwen/Bonsai (= `Qwen` en
/// `model_family`); Llama/Mistral/Generic responden directo: pedirles
/// low/medium/high/max no cambia nada en el motor. `off`/`none`/ausente =
/// sin pedido = sin mismatch. El llamador lo loguea (visible en Logs), no lo
/// rechaza: el motor responde igual, solo sin razonar.
pub(crate) fn effort_mismatch(served: &str, effort: Option<&str>) -> bool {
    let e = match effort {
        Some(s) => s.trim().to_lowercase(),
        None => return false,
    };
    if e.is_empty() || e == "off" || e == "none" {
        return false;
    }
    !matches!(model_family(served), ModelFamily::Qwen)
}

/// Sampler oficial de Qwen3.8-27B (model card `Qwen/Qwen3.8-27B`) según el modo
/// resuelto: thinking → temp 1.0 / top_p 0.95 / top_k 20 / min_p 0.0 /
fn qwen_sampler_defaults(thinking: bool) -> [(&'static str, serde_json::Value); 5] {
    if thinking {
        [
            ("temperature", serde_json::json!(1.0)),
            ("top_k", serde_json::json!(20)),
            ("top_p", serde_json::json!(0.95)),
            ("min_p", serde_json::json!(0.0)),
            ("presence_penalty", serde_json::json!(0.0)),
        ]
    } else {
        [
            ("temperature", serde_json::json!(0.7)),
            ("top_k", serde_json::json!(20)),
            ("top_p", serde_json::json!(0.80)),
            ("min_p", serde_json::json!(0.0)),
            ("presence_penalty", serde_json::json!(1.5)),
        ]
    }
}
/// Preparar el payload de `POST /v1/chat/completions` en UN solo parseo
/// (auditoría de rendimiento: antes parseaba/serializaba el cuerpo 5 veces
/// por request). Devuelve `(bytes_a_enviar, modelo_pedido, stream_pedido)`.
///
/// Además alinea con Qwen3.8: `reasoning_effort` ausente → `"medium"` (el
/// default `xhigh` de la plantilla sobre-piensa: medido en esta máquina,
/// 7687 chars de thinking y respuesta VACÍA con `max_tokens` 2000, frente a
/// 209 + respuesta correcta en `medium`), y el sampler oficial del modo
/// resuelto en las claves que el cliente no fijó.
fn prepare_chat_payload(body: &[u8], served: &str) -> (Vec<u8>, String, bool) {
    let mut v: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let req_model = v
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| served.to_string());
    let stream_req = v
        .as_ref()
        .and_then(|v| v.get("stream"))
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if let Some(obj) = v.as_mut().and_then(|v| v.as_object_mut()) {
        // Ausente → `medium`. Presente se normaliza como antes (`off` es alias
        // de `none`; desconocido o no-string se elimina y la plantilla usa su
        // default, como siempre: así `reasoning_effort:null` de pi `--thinking
        // max` sigue cayendo al default thinking de la plantilla).
        let effort = if !obj.contains_key("reasoning_effort") {
            obj.insert(
                "reasoning_effort".to_string(),
                serde_json::Value::String("medium".to_string()),
            );
            "medium".to_string()
        } else {
            match obj
                .get("reasoning_effort")
                .and_then(|x| x.as_str())
                .and_then(normalize_reasoning_effort)
            {
                Some(mapped) => {
                    obj.insert(
                        "reasoning_effort".to_string(),
                        serde_json::Value::String(mapped.clone()),
                    );
                    mapped
                }
                None => {
                    obj.remove("reasoning_effort");
                    String::new()
                }
            }
        };
        // Sin thinking (effort `none` o `enable_thinking:false` explícito) rige
        // el sampler instruct; en cualquier otro caso el thinking.
        let kwargs_off = obj
            .get("chat_template_kwargs")
            .and_then(|k| k.get("enable_thinking"))
            .and_then(|b| b.as_bool())
            == Some(false);
        let thinking = effort != "none" && !kwargs_off;
        // Sampler por familia del SERVIDO (no del pedido: el pedido se reescribe
        // al servido justo debajo). Qwen/Bonsai/desconocido = Qwen (medido).
        for (k, val) in sampler_defaults(model_family(served), thinking) {
            if !obj.contains_key(k) {
                obj.insert(k.to_string(), val);
            }
        }
        if !served.is_empty() && obj.get("model").and_then(|m| m.as_str()).is_some() {
            obj.insert(
                "model".to_string(),
                serde_json::Value::String(served.to_string()),
            );
        }
        if stream_req && obj.get("stream_options").is_none() {
            obj.insert(
                "stream_options".to_string(),
                serde_json::json!({"include_usage": true}),
            );
        }
    }
    match v {
        Some(v) => (
            serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec()),
            req_model,
            stream_req,
        ),
        None => (body.to_vec(), req_model, stream_req),
    }
}

/// Id servido por el motor para las superficies HTTP (`/v1/models` y el
/// rewrite del proxy). Delega en `agents::served_model_id`, la MISMA función
/// que escriben los lanzadores en la config de cada agente: si divergieran,
/// el agente anunciaría un modelo y el proxy respondería por otro.
///
/// El fallback (motor sin modelo) mantiene el alias estable para que
/// `/v1/models` SIEMPRE conteste y el proxy siga teniendo un id al que
/// reescribir aunque el motor esté apagado (D-2).
fn served_model_id(st: &ServerStatus, cfg: &ConfigStore) -> String {
    crate::agents::served_model_id(st).unwrap_or_else(|| {
        cfg.get()
            .engine
            .aliases
            .first()
            .cloned()
            .unwrap_or_else(|| "localmind".to_string())
    })
}

/// Cuerpo `GET /v1/models` (forma OpenAI): id servido + alias estables.
fn models_list_json(st: &ServerStatus, cfg: &ConfigStore) -> String {
    let served = served_model_id(st, cfg);
    let mut ids = vec![served.clone()];
    for a in &cfg.get().engine.aliases {
        if !ids.iter().any(|x| x == a) {
            ids.push(a.clone());
        }
    }
    let data: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "owned_by": "localmind",
            })
        })
        .collect();
    serde_json::json!({"object": "list", "data": data}).to_string()
}

impl HttpServer {
    pub fn start(
        process_mgr: Arc<ProcessManager>,
        config: Arc<ConfigStore>,
        base_dir: PathBuf,
    ) -> Result<Self, String> {
        // Clave local: crearla en el primer arranque para que las rutas
        // protegidas y la cookie de la WebView funcionen desde el inicio.
        let gateway_key = crate::auth::load_or_create_key();
        // Fase B2: bind según `[lan].enabled` (default false = loopback como
        // siempre). El motor (llama-server) queda en `127.0.0.1` en todos los
        // modos: solo este gateway sale a la LAN. Sin hot-swap: cambiarlo
        // exige reiniciar (diseño B11).
        let lan_enabled = config.get().lan.enabled;
        crate::meta::set_lan_mode(lan_enabled);
        let bind_host = if lan_enabled { "0.0.0.0" } else { "127.0.0.1" };
        let preferred = config.get().engine.http_port;
        let mut server = None;
        let mut port = preferred;

        for p in preferred..preferred + 10 {
            if let Ok(s) = Server::http((bind_host, p)) {
                server = Some(s);
                port = p;
                break;
            }
        }

        let server = server.ok_or_else(|| {
            format!(
                "No se pudo iniciar el servidor HTTP en {} en el rango {}-{}",
                bind_host,
                preferred,
                preferred + 10
            )
        })?;
        let server = Arc::new(server);
        let srv_clone = Arc::clone(&server);
        // Puerto ligado de verdad: los CLIs hablan con el GATEWAY (no con el
        // motor) para pasar por clave/aliasing/usage. Se propaga a cada
        // request junto con la clave (ambos se clonan por request).
        let bound_port = port;
        process_mgr.log(&format!(
            "[LocalMind] Gateway HTTP en {}:{} (modo LAN {}).",
            bind_host,
            port,
            if lan_enabled {
                "activado"
            } else {
                "desactivado"
            }
        ));
        thread::spawn(move || {
            for req in srv_clone.incoming_requests() {
                let url = req.url().to_string();
                let method = req.method().to_string();
                let mgr = Arc::clone(&process_mgr);
                let cfg = Arc::clone(&config);
                let base = base_dir.clone();
                let key = gateway_key.clone();

                thread::spawn(move || {
                    // P0 producción (panic=abort): un pánico en un request NO
                    // puede matar gateway+UI. Se captura y solo muere ese hilo.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle_request(req, method, url, mgr, cfg, base, key, bound_port);
                    }))
                    .is_err()
                    {
                        eprintln!("[LocalMind] request abortado por pánico (contenido)");
                    }
                });
            }
        });
        Ok(Self { port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Rutas públicas (sin clave): UI + iconos. Todo lo demás exige clave.
/// Fase C (OMNI): se sirven los iconos nuevos y los históricos en transición.
fn is_public_path(method: &str, url: &str) -> bool {
    if method == "OPTIONS" {
        return true;
    }
    if method != "GET" {
        return false;
    }
    let path = url.split('?').next().unwrap_or(url);
    matches!(
        path,
        "/" | "/index.html" | "/localmind.ico" | "/localmind.png" | "/omni.ico" | "/omni.png"
    )
}

/// Icono a servir para una ruta pública (Fase C): nombre de fichero en
/// `base_dir` + Content-Type. Los históricos conviven en transición.
fn icon_asset_for(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path {
        "/localmind.ico" => Some(("localmind.ico", b"image/x-icon")),
        "/omni.ico" => Some(("omni.ico", b"image/x-icon")),
        "/localmind.png" => Some(("localmind.png", b"image/png")),
        "/omni.png" => Some(("omni.png", b"image/png")),
        _ => None,
    }
}

/// Decisión pura de la puerta D-7 (testeable sin socket): ¿esta petición
/// necesita credencial? Falso SOLO para rutas públicas (`is_public_path`: UI,
/// iconos, OPTIONS) y para el login (`POST /api/unlock`). Todo lo demás —
/// incluido `GET /api/version` por decisión documentada arriba — exige clave.
fn puerta_auth_exige_clave(method: &str, url: &str) -> bool {
    if is_public_path(method, url) {
        return false;
    }
    if method == "POST" && url == "/api/unlock" {
        return false;
    }
    true
}

/// Puerta de auth D-7: todo lo no público exige clave.
///
/// Orden en `handle_request`: peer-gate B2 (403) → OPTIONS → UI/iconos
/// públicos → `POST /api/unlock` (login) → ESTA puerta → resto de rutas.
///
/// Regla: si `!is_public_path(&method, &url)` y la ruta no es el login
/// (`POST /api/unlock`), se exige `crate::auth::is_authorized` (Bearer,
/// `x-api-key` o cookie `lm_key`) y sin credencial válida se responde 401
/// (`unauthorized_json_for_origin`). Excepción documentada: NINGUNA otra —
/// `GET /api/version` también exige clave (decisión de esta ronda: antes era
/// pública de facto; el smoke la llama ya con clave como el resto).
///
/// Riesgo conocido (diseño B4 vigente, NO tocado aquí): `GET /` y los iconos
/// siguen fijando la cookie `lm_key` a loopback SIN pedir clave antes. En la
/// WebView local eso es auto-login por diseño; en un loopback compartido otro
/// proceso local podría recogerla. Endurecerlo (pedir clave también en `/`)
/// rompería el arranque sin fricción y queda como decisión pendiente.
/// CI (`-D warnings`): 8 args de contexto por request; struct por request no compensa.
#[allow(clippy::too_many_arguments)]
fn handle_request(
    mut req: tiny_http::Request,
    method: String,
    url: String,
    mgr: Arc<ProcessManager>,
    cfg: Arc<ConfigStore>,
    base_dir: PathBuf,
    gateway_key: String,
    http_port: u16,
) {
    // Origen de esta petición para CORS estricto (P29/B3): allowlist según
    // el modo LAN vigente (ver `cors_origin_header`).
    let origin = crate::meta::request_origin(req.headers());
    let origin_ref = origin.as_deref();
    let fixed = cors_fixed_headers();

    // Fase B2: regla del peer ANTES de auth (diseño B2). Fail-closed: sin
    // dirección de peer no hay de dónde fiarse y se deniega. Con
    // `lan.enabled=false` esto deja pasar exactamente lo mismo que antes
    // (solo loopback).
    let peer_ip = req.remote_addr().map(|a| a.ip());
    if !peer_ip.is_some_and(|ip| crate::meta::peer_permitido(ip, crate::meta::lan_mode())) {
        // Trazabilidad de sondeos: IP y origen denegados al log (sin cuerpo).
        let peer_txt = peer_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "<sin-peer>".to_string());
        mgr.log_warn(&format!(
            "[LocalMind] Conexión denegada de {} (origen {:?}, {} {}) en modo {}.",
            peer_txt,
            origin_ref.unwrap_or("-"),
            method,
            url.split('?').next().unwrap_or(&url),
            if crate::meta::lan_mode() {
                "LAN"
            } else {
                "local"
            },
        ));
        let _ = req.respond(json_response_for_origin(
            403,
            r#"{"error":"red_no_permitida"}"#.into(),
            origin_ref,
        ));
        return;
    }
    // La cookie de sesión (`lm_key`) solo se fija a loopback (diseño B4,
    // mínimo de esta ronda): en LAN la UI llega sin auto-autenticar y la API
    // sigue exigiendo la clave. La pantalla de desbloqueo es Fase B4/C.
    let peer_loopback = peer_ip.is_some_and(|ip| ip.is_loopback());

    if method == "OPTIONS" {
        let mut resp = Response::empty(200);
        if let Some(o) = cors_origin_header(origin_ref) {
            resp.add_header(o);
        }
        resp.add_header(fixed[0].clone());
        resp.add_header(fixed[1].clone());
        let _ = req.respond(resp);
        return;
    }

    if method == "GET" && (url == "/" || url == "/index.html") {
        let ui = base_dir.join("ui.html");
        let html = if ui.exists() {
            std::fs::read_to_string(&ui)
                .unwrap_or_else(|_| include_str!("../ui_fallback.html").to_string())
        } else {
            include_str!("../ui_fallback.html").to_string()
        };
        let ct =
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
        let mut resp = Response::from_string(html);
        resp.add_header(ct);
        if peer_loopback {
            let Some(cookie) = session_cookie_header(&gateway_key) else {
                let _ = req.respond(json_response_for_origin(
                    500,
                    r#"{"error":"clave de sesión inválida"}"#.into(),
                    origin_ref,
                ));
                return;
            };
            resp.add_header(cookie);
        }
        add_cors_for(&mut resp, origin_ref);
        let _ = req.respond(resp);
        return;
    }

    // Iconos (Fase C): un solo bloque para los cuatro (ver `icon_asset_for`).
    if method == "GET" {
        let path = url.split('?').next().unwrap_or(&url);
        if let Some((file, mime)) = icon_asset_for(path) {
            let bytes = match load_asset(&base_dir, file) {
                Some(b) => b,
                None => {
                    let _ = req.respond(json_response_for_origin(
                        404,
                        "{\"error\":\"not_found\"}".into(),
                        origin_ref,
                    ));
                    return;
                }
            };
            let ct = Header::from_bytes(&b"Content-Type"[..], mime).unwrap();
            let mut resp = Response::from_data(bytes);
            resp.add_header(ct);
            if peer_loopback {
                let Some(cookie) = session_cookie_header(&gateway_key) else {
                    let _ = req.respond(json_response_for_origin(
                        500,
                        r#"{"error":"clave de sesión inválida"}"#.into(),
                        origin_ref,
                    ));
                    return;
                };
                resp.add_header(cookie);
            }
            add_cors_for(&mut resp, origin_ref);
            let _ = req.respond(resp);
            return;
        }
    }
    // Desbloqueo LAN (Fase B4): emite la cookie de sesión a quien presente la
    // clave. Es login, así que va ANTES de la puerta D-7 pero DESPUÉS del gate
    // del peer (red privada o loopback; internet directa ya cayó con 403).
    // La clave viaja en el cuerpo (nunca en URL/logs); el error no distingue
    // motivos.
    if method == "POST" && url == "/api/unlock" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin_ref,
                ));
                return;
            }
        };
        let candidate = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("key").and_then(|k| k.as_str()).map(str::to_string))
            .unwrap_or_default();
        if !crate::auth::unlock_allowed() {
            // 429 con el mismo cuerpo opaco: no distingue «bloqueado» de
            // «clave mala» más que por el código (el atacante ya sabe que
            // existe el endpoint; lo que se frena es la cadencia).
            let _ = req.respond(json_response_for_origin(
                429,
                r#"{"error":"unauthorized"}"#.into(),
                origin_ref,
            ));
            return;
        }
        if !crate::auth::key_matches(&candidate, &gateway_key) {
            crate::auth::unlock_failed();
            // Retardo fijo: la comparación ya es constante, pero el 401
            // inmediato permite ~miles de intentos/s en LAN; 500 ms lo baja
            // a ~2/s por conexión sin molestar al dueño (un intento real).
            std::thread::sleep(std::time::Duration::from_millis(500));
            let _ = req.respond(unauthorized_json_for_origin(origin_ref));
            return;
        }
        crate::auth::unlock_ok();
        let Some(cookie) = session_cookie_header(&gateway_key) else {
            let _ = req.respond(json_response_for_origin(
                500,
                r#"{"error":"clave de sesión inválida"}"#.into(),
                origin_ref,
            ));
            return;
        };
        let mut resp = json_response_for_origin(200, r#"{"status":"ok"}"#.to_string(), origin_ref);
        resp.add_header(cookie);
        let _ = req.respond(resp);
        return;
    }
    // Puerta D-7: tras peer-gate (403), OPTIONS, UI/iconos públicos y el login
    // `POST /api/unlock`. Todo lo demás exige credencial (Bearer, `x-api-key`
    // o cookie `lm_key`); sin ella → 401. `GET /api/version` también la exige
    // (decisión documentada en el docstring: antes era pública de facto).
    // Las rutas públicas ya retornaron arriba, pero se re-chequea
    // `is_public_path` por defensa en profundidad (si mañana se añade una
    // pública nueva, nace abierta sin tocar la puerta).
    if puerta_auth_exige_clave(&method, &url)
        && !crate::auth::is_authorized(req.headers(), &gateway_key)
    {
        let _ = req.respond(unauthorized_json_for_origin(origin_ref));
        return;
    }

    if method == "GET" && url == "/api/models" {
        // Listado utilizable: raíz + un nivel (`models/<sub>/x.gguf`), sin
        // `.cache`/`.verified`. `filename` sigue siendo el fichero (lo que la
        // UI usa como `option.value` y `/api/start` como `model`); `path`
        // absoluta se conserva; se añade `rel` (relativa a `models/`).
        let models_dir = mgr.models_dir().to_path_buf();
        let list: Vec<serde_json::Value> = crate::models::list_gguf_files(&models_dir)
            .iter()
            .map(|(name, path, bytes)| {
                let size_gb = (*bytes as f64) / (1024.0 * 1024.0 * 1024.0);
                let rel = path
                    .strip_prefix(&models_dir)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| name.clone());
                let v = serde_json::json!({
                    "filename": name,
                    "name": name.trim_end_matches(".gguf"),
                    "size_gb": (size_gb * 100.0).round() / 100.0,
                    "path": path.to_string_lossy(),
                    "rel": rel,
                });
                crate::models::enrich_model_entry(&models_dir, v, *bytes)
            })
            .collect();
        let json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
        let _ = req.respond(json_response_for_origin(200, json, origin_ref));
        return;
    }

    // Metadatos del gateway: versión de la app + build/commit del motor.
    // La ruta nunca falla por el motor: ante cualquier problema los campos
    // del motor salen `null` y la respuesta sigue siendo 200.
    if method == "GET" && url == "/api/version" {
        let info = crate::meta::engine_info(&base_dir);
        let upd = crate::update::update_snapshot();
        let body = serde_json::json!({
            "app": env!("CARGO_PKG_VERSION"),
            "engine_build": info.build,
            "engine_commit": info.commit,
            "engine_path": info.path,
            "log_ok": crate::filelog::log_ok(),
            "update": {
                "state": upd.state,
                "latest": upd.latest,
                "pending": crate::update::pending_update().is_some(),
            },
        });
        let _ = req.respond(json_response_for_origin(200, body.to_string(), origin_ref));
        return;
    }

    // Actualización automática (Fase Prod): estado + chequeo + descarga +
    // cancelación. La instalación ocurre al reiniciar (main.rs aplica el
    // staging pendiente al salir). Todas exigen clave (no son públicas).
    if method == "GET" && (url == "/api/update" || url.starts_with("/api/update?")) {
        let _ = req.respond(json_response_for_origin(
            200,
            crate::update::update_snapshot().json(),
            origin_ref,
        ));
        return;
    }

    if method == "POST" && url == "/api/update/check" {
        let _ = read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY);
        if crate::update::update_busy() {
            let _ = req.respond(json_response_for_origin(
                409,
                serde_json::json!({ "error": "Ya hay una operación de actualización en curso" })
                    .to_string(),
                origin_ref,
            ));
            return;
        }
        let feed = cfg.get().update.feed.clone();
        // Logger con niveles: `[ERROR]`/`[WARN]` a disco con nivel (G5), el
        // resto como INFO. Misma forma en `/api/update/download` abajo.
        let logger: std::sync::Arc<dyn Fn(String) + Send + Sync> = {
            let m = Arc::clone(&mgr);
            std::sync::Arc::new(move |line: String| {
                if line.contains("[ERROR]") {
                    m.log_error(&line);
                } else if line.contains("[WARN]") {
                    m.log_warn(&line);
                } else {
                    m.log(&line);
                }
            })
        };
        crate::update::spawn_check(feed, false, logger);
        let _ = req.respond(json_response_for_origin(
            200,
            crate::update::update_snapshot().json(),
            origin_ref,
        ));
        return;
    }

    if method == "POST" && url == "/api/update/download" {
        let _ = read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY);
        if crate::update::update_busy() {
            let _ = req.respond(json_response_for_origin(
                409,
                serde_json::json!({ "error": "Ya hay una operación de actualización en curso" })
                    .to_string(),
                origin_ref,
            ));
            return;
        }
        let snap = crate::update::update_snapshot();
        if snap.state != "available" || snap.latest.is_empty() {
            let _ = req.respond(json_response_for_origin(409, serde_json::json!({ "error": "No hay actualización disponible: comprueba primero" }).to_string(), origin_ref));
            return;
        }
        // Re-resolver el asset contra el canal (el estado `available` no
        // guarda la URL: evita persistir URLs firmadas y revalida el checksum).
        let feed = cfg.get().update.feed.clone();
        let repo = crate::update::feed_repo(&feed)
            .unwrap_or_else(|| crate::update::DEFAULT_FEED_REPO.to_string());
        let api_base = crate::update::releases_api_base();
        let current = env!("CARGO_PKG_VERSION").to_string();
        let url = crate::update::latest_url(&repo, &api_base);
        let latest_raw = match ureq::get(&url)
            .set("User-Agent", &format!("OMNI/{}", current))
            .set("Accept", "application/vnd.github+json")
            .timeout(std::time::Duration::from_secs(20))
            .call()
        {
            Ok(r) => match r.into_string() {
                Ok(t) => t,
                Err(e) => {
                    let _ = req.respond(json_response_for_origin(502, serde_json::json!({ "error": format!("Error al leer el canal de actualizaciones: {}", e) }).to_string(), origin_ref));
                    return;
                }
            },
            Err(ureq::Error::Status(code, _)) => {
                let _ = req.respond(json_response_for_origin(502, serde_json::json!({ "error": format!("El canal de actualizaciones devolvió {}", code) }).to_string(), origin_ref));
                return;
            }
            Err(e) => {
                let _ = req.respond(json_response_for_origin(502, serde_json::json!({ "error": format!("Error de red con el canal de actualizaciones: {}", e) }).to_string(), origin_ref));
                return;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&latest_raw) {
            Ok(v) => v,
            Err(_) => {
                let _ = req.respond(json_response_for_origin(
                    502,
                    r#"{"error":"El canal de actualizaciones devolvió datos inválidos"}"#
                        .to_string(),
                    origin_ref,
                ));
                return;
            }
        };
        let body_text = v.get("body").and_then(|b| b.as_str()).unwrap_or("");
        let assets = v
            .get("assets")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        let mut chosen: Option<(String, String, Option<u64>, String)> = None;
        for a in &assets {
            let name = a.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let durl = a
                .get("browser_download_url")
                .and_then(|u| u.as_str())
                .unwrap_or("");
            if crate::update::parse_asset_name(name)
                .map(|(ver, _)| ver)
                .as_deref()
                != Some(snap.latest.as_str())
            {
                continue;
            }
            if durl.is_empty() {
                continue;
            }
            let sum = crate::update::find_checksum(body_text, name).unwrap_or_default();
            if sum.is_empty() {
                continue;
            }
            let size = a.get("size").and_then(|s| s.as_u64()).filter(|n| *n > 0);
            chosen = Some((name.to_string(), durl.to_string(), size, sum));
            break;
        }
        let Some((aname, aurl, asize, asum)) = chosen else {
            let _ = req.respond(json_response_for_origin(
                502,
                r#"{"error":"La actualización ya no ofrece paquete verificado"}"#.to_string(),
                origin_ref,
            ));
            return;
        };
        // Espacio: el ZIP (~cientos de MB) + staging duplican en %TEMP%. Sin
        // dato medido no se bloquea; con dato y sin 2× se responde 507.
        let work = crate::update::update_work_dir(&snap.latest);
        if crate::update::fits_download(asize, crate::update::free_bytes_for(&work)) == Some(false)
        {
            let _ = req.respond(json_response_for_origin(
                507,
                r#"{"error":"Sin espacio en el disco temporal para la actualización"}"#.to_string(),
                origin_ref,
            ));
            return;
        }
        let notes = v
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .lines()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n");
        let logger: std::sync::Arc<dyn Fn(String) + Send + Sync> = {
            let m = Arc::clone(&mgr);
            std::sync::Arc::new(move |line: String| {
                if line.contains("[ERROR]") {
                    m.log_error(&line);
                } else if line.contains("[WARN]") {
                    m.log_warn(&line);
                } else {
                    m.log(&line);
                }
            })
        };
        crate::update::spawn_download(aname, aurl, snap.latest.clone(), asize, asum, notes, logger);
        let _ = req.respond(json_response_for_origin(
            200,
            crate::update::update_snapshot().json(),
            origin_ref,
        ));
        return;
    }

    if method == "POST" && url == "/api/update/cancel" {
        let _ = read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY);
        let _ = req.respond(json_response_for_origin(
            200,
            crate::update::update_cancel().json(),
            origin_ref,
        ));
        return;
    }

    // Reiniciar e instalar (P0 2026-10-09): con staging `ready`, arma la
    // salida ordenada; el tick del loop ejecuta stop+swap+Exit en ≤5 s y el
    // script desacoplado relanza la app ya actualizada. Sin `ready` → 409.
    if method == "POST" && url == "/api/update/restart" {
        let _ = read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY);
        let snap = crate::update::update_snapshot();
        // Invariante P0-2: `ready` exige pending + staging VÁLIDO en disco
        // (con OMNI.exe+ui.html). Si el staging se perdió (limpieza de %TEMP%),
        // se auto-repara a error con motivo en vez de reiniciar en falso.
        let staged_ok = crate::update::pending_update().is_some_and(|p| {
            let st = PathBuf::from(&p.staging_dir);
            st.is_dir() && st.join("OMNI.exe").is_file() && st.join("ui.html").is_file()
        });
        if snap.state != "ready" || !staged_ok {
            if snap.state == "ready" {
                // Auto-repair: el estado mentía (staging perdido) → error honesto.
                crate::update::mark_staging_lost();
            }
            let _ = req.respond(json_response_for_origin(
                409,
                serde_json::json!({ "error": "No hay actualización lista: descarga primero" })
                    .to_string(),
                origin_ref,
            ));
            return;
        }
        mgr.log(&format!(
            "[LocalMind] Reinicio para instalar OMNI {}: la app se cerrará y reabrirá sola.",
            snap.latest
        ));
        crate::update::arm_restart();
        let _ = req.respond(json_response_for_origin(
            200,
            serde_json::json!({ "status": "restarting", "version": snap.latest }).to_string(),
            origin_ref,
        ));
        return;
    }
    if method == "GET" && url == "/api/logs/export" {
        let lines = mgr.get_recent_logs();
        let mut resp = Response::from_string(lines.join("\n")).with_status_code(StatusCode(200));
        let ct =
            Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..]).unwrap();
        resp.add_header(ct);
        let stamp = crate::meta::utc_stamp_now();
        let cd = Header::from_bytes(
            &b"Content-Disposition"[..],
            format!("attachment; filename=\"localmind-logs-{}.txt\"", stamp).as_bytes(),
        )
        .unwrap();
        resp.add_header(cd);
        add_cors_for(&mut resp, origin_ref);
        let _ = req.respond(resp);
        return;
    }

    if method == "GET" && url == "/api/hardware" {
        let hw = mgr.detect_hardware();
        let json = serde_json::to_string(&hw).unwrap_or_else(|_| "{}".to_string());
        let _ = req.respond(json_response_for_origin(200, json, origin_ref));
        return;
    }

    if method == "GET" && url == "/api/profiles" {
        let profiles = crate::config::get_hardware_profiles(&cfg.get());
        let json = serde_json::to_string(&profiles).unwrap_or_else(|_| "[]".to_string());
        let _ = req.respond(json_response_for_origin(200, json, origin_ref));
        return;
    }

    if method == "GET" && url == "/api/profiles/export" {
        let profiles = crate::config::get_hardware_profiles(&cfg.get());
        let json = serde_json::to_string_pretty(&profiles).unwrap_or_else(|_| "[]".to_string());
        let mut resp = json_response_for_origin(200, json, origin_ref);
        let cd = Header::from_bytes(
            &b"Content-Disposition"[..],
            &b"attachment; filename=\"localmind_profiles.json\""[..],
        )
        .unwrap();
        resp.add_header(cd);
        let _ = req.respond(resp);
        return;
    }

    if method == "POST" && url == "/api/profiles/import" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin_ref,
                ));
                return;
            }
        };
        match parse_profiles_import(&body) {
            Ok(imported) => {
                cfg.update(|c| {
                    c.profiles = imported;
                });
                // Sec producción: el error de `save` se propaga al handler
                // (500 + log) en vez de tragarse con `let _ =`. Antes la UI
                // mostraba "Perfiles actualizados" aunque nada se persistió.
                if let Err(e) = cfg.save() {
                    mgr.log_error(&format!(
                        "[LocalMind] No se pudo guardar la importación de perfiles: {}",
                        e
                    ));
                    let _ = req.respond(json_response_for_origin(
                        500,
                        serde_json::json!({ "error": format!("no se pudo guardar la importación de perfiles: {}", e) }).to_string(),
                        origin_ref,
                    ));
                    return;
                }
                let _ = req.respond(json_response_for_origin(
                    200,
                    r#"{"status":"ok","message":"Perfiles actualizados"}"#.into(),
                    origin_ref,
                ));
            }
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin_ref,
                ));
            }
        }
        return;
    }
    // Alta/edición de un perfil (`upsert`): valida id/contexto/cache/flags y
    // devuelve la lista completa en la forma de `GET /api/profiles`. Nada se
    // persiste si algún campo es inválido.
    if method == "POST" && url == "/api/profiles/save" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin_ref,
                ));
                return;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": format!("JSON inválido: {}", e) }).to_string(),
                    origin_ref,
                ));
                return;
            }
        };
        let bad = |campo: &str| {
            serde_json::json!({ "error": format!("campo inválido: {}", campo) }).to_string()
        };
        let m = match v.as_object() {
            Some(m) => m,
            None => {
                let _ = req.respond(json_response_for_origin(400, bad("perfil"), origin_ref));
                return;
            }
        };
        let id = match m.get("id").and_then(|j| j.as_str()) {
            Some(id) if crate::config::profile_id_ok(id) => id.to_string(),
            _ => {
                let _ = req.respond(json_response_for_origin(400, bad("id"), origin_ref));
                return;
            }
        };
        let name = match m.get("name").and_then(|j| j.as_str()) {
            Some(s) if !s.trim().is_empty() && s.len() <= 120 => s.to_string(),
            _ => {
                let _ = req.respond(json_response_for_origin(400, bad("name"), origin_ref));
                return;
            }
        };
        let description = match m.get("description") {
            None => String::new(),
            Some(j) => match j.as_str() {
                Some(s) if s.len() <= 2000 => s.to_string(),
                _ => {
                    let _ = req.respond(json_response_for_origin(
                        400,
                        bad("description"),
                        origin_ref,
                    ));
                    return;
                }
            },
        };
        let context = match m
            .get("context")
            .and_then(|j| j.as_u64())
            .and_then(|n| usize::try_from(n).ok())
        {
            Some(n) if crate::config::profile_context_ok(n) => n,
            _ => {
                let _ = req.respond(json_response_for_origin(400, bad("context"), origin_ref));
                return;
            }
        };
        let cache_ram = match m.get("cache_ram") {
            None => 0,
            Some(j) => match j.as_u64().and_then(|n| usize::try_from(n).ok()) {
                Some(n) if crate::config::profile_cache_ram_ok(n) => n,
                _ => {
                    let _ =
                        req.respond(json_response_for_origin(400, bad("cache_ram"), origin_ref));
                    return;
                }
            },
        };
        let extra_flags: Vec<String> = match m.get("extra_flags") {
            None => Vec::new(),
            Some(j) => match j.as_array() {
                Some(arr) => {
                    let mut out = Vec::with_capacity(arr.len());
                    for f in arr {
                        match f.as_str() {
                            Some(s) => out.push(s.to_string()),
                            None => {
                                let _ = req.respond(json_response_for_origin(
                                    400,
                                    bad("extra_flags"),
                                    origin_ref,
                                ));
                                return;
                            }
                        }
                    }
                    out
                }
                None => {
                    let _ = req.respond(json_response_for_origin(
                        400,
                        bad("extra_flags"),
                        origin_ref,
                    ));
                    return;
                }
            },
        };
        if !crate::config::profile_flags_ok(&extra_flags) {
            let _ = req.respond(json_response_for_origin(
                400,
                bad("extra_flags"),
                origin_ref,
            ));
            return;
        }
        if let Some(err) = crate::config::psu_unsafe_flag(&extra_flags) {
            let _ = req.respond(json_response_for_origin(
                400,
                serde_json::json!({ "error": err }).to_string(),
                origin_ref,
            ));
            return;
        }
        let mut next = cfg.get();
        let prof = crate::config::HardwareProfile {
            id: id.clone(),
            name,
            description,
            context,
            cache_ram,
            extra_flags,
        };
        match next.profiles.iter().position(|p| p.id == id) {
            Some(i) => next.profiles[i] = prof,
            None => next.profiles.push(prof),
        }
        let prev = cfg.get();
        cfg.update(|c| *c = next.clone());
        if let Err(e) = cfg.save() {
            cfg.update(|c| *c = prev);
            let _ = req.respond(json_response_for_origin(
                500,
                serde_json::json!({ "error": format!("no se pudo guardar el perfil: {}", e) })
                    .to_string(),
                origin_ref,
            ));
            return;
        }
        mgr.log(&format!("[LocalMind] Perfil '{}' guardado.", id));
        let list = crate::config::get_hardware_profiles(&next);
        let json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
        let _ = req.respond(json_response_for_origin(200, json, origin_ref));
        return;
    }

    // Borrado de un perfil: 400 si no existe, si es el último o si está en
    // uso (perfil del motor en curso o `last.profile` de arranque).
    if method == "POST" && url == "/api/profiles/delete" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin_ref,
                ));
                return;
            }
        };
        let id = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("id").and_then(|j| j.as_str().map(str::to_string)))
            .unwrap_or_default();
        if !crate::config::profile_id_ok(&id) {
            let _ = req.respond(json_response_for_origin(
                400,
                r#"{"error":"campo inválido: id"}"#.into(),
                origin_ref,
            ));
            return;
        }
        let cur = cfg.get();
        if !cur.profiles.iter().any(|p| p.id == id) {
            let _ = req.respond(json_response_for_origin(
                400,
                serde_json::json!({ "error": format!("perfil desconocido: '{}'", id) }).to_string(),
                origin_ref,
            ));
            return;
        }
        if cur.profiles.len() <= 1 {
            let _ = req.respond(json_response_for_origin(
                400,
                r#"{"error":"no se puede eliminar el último perfil"}"#.into(),
                origin_ref,
            ));
            return;
        }
        // En uso = motor en curso con ese perfil, o arranque futuro (`last`).
        let live = mgr.get_status().profile;
        let boot = cur.last.profile.clone().unwrap_or_default();
        if live == id || boot == id {
            let _ = req.respond(json_response_for_origin(
                400,
                serde_json::json!({ "error": format!("el perfil '{}' está en uso", id) })
                    .to_string(),
                origin_ref,
            ));
            return;
        }
        let mut next = cur.clone();
        next.profiles.retain(|p| p.id != id);
        cfg.update(|c| *c = next.clone());
        if let Err(e) = cfg.save() {
            cfg.update(|c| *c = cur);
            let _ = req.respond(json_response_for_origin(
                500,
                serde_json::json!({ "error": format!("no se pudo eliminar el perfil: {}", e) })
                    .to_string(),
                origin_ref,
            ));
            return;
        }
        mgr.log(&format!("[LocalMind] Perfil '{}' eliminado.", id));
        let list = crate::config::get_hardware_profiles(&next);
        let json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
        let _ = req.respond(json_response_for_origin(200, json, origin_ref));
        return;
    }

    if method == "POST" && url == "/api/import_model" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let parsed = match parse_import_model_body(&body) {
            Ok(p) => p,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let src = std::path::Path::new(&parsed.path);
        // P1-IO + TOCTOU (2026-10-10, wave-4): el 409 previo es solo UX
        // temprana (misma resolución que el interior, sin divergencias); la
        // protección real vive DENTRO de `import_model_from_path` (lock +
        // tmp+rename atómico), así que dos imports concurrentes no se pisan.
        if let Err((code, msg)) =
            crate::models::check_import_dest(mgr.models_dir(), src, parsed.overwrite)
        {
            let _ = req.respond(json_response_for_origin(
                code,
                serde_json::json!({ "error": msg }).to_string(),
                origin.as_deref(),
            ));
            return;
        }
        match mgr.import_model_from_path(src, parsed.overwrite) {
            Ok(filename) => {
                let _ = req.respond(json_response_for_origin(
                    200,
                    serde_json::json!({ "status": "ok", "filename": filename }).to_string(),
                    origin.as_deref(),
                ));
            }
            Err(e) => {
                // 409 por VARIANTE (ABIERTO-1 wave-6): `Exists` → 409, resto
                // → 400. Nunca por substring: un nombre con "ya existe" en
                // otro error no produce un 409 espurio.
                let code = if e.is_exists() { 409 } else { 400 };
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": e.to_string() }).to_string(),
                    origin.as_deref(),
                ));
            }
        }
        return;
    }

    if method == "POST" && url == "/api/open_models_dir" {
        let p = mgr.models_dir();
        let _ = std::process::Command::new("explorer.exe").arg(p).spawn();
        let _ = req.respond(json_response_for_origin(
            200,
            r#"{"status":"ok"}"#.into(),
            origin.as_deref(),
        ));
        return;
    }

    // Abrir carpeta de datos (P1-7 producción): allowlist cerrada
    // (`appdata` = %APPDATA%\LocalMind, `temp` = %TEMP%\omni-update). Sin
    // esto la tarjeta Datos solo podía copiar la ruta. Cuerpo acotado 64 KiB.
    if method == "POST" && url == "/api/open_data_dir" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let which = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("which").and_then(|w| w.as_str()).map(str::to_string))
            .unwrap_or_default();
        let dir: Option<std::path::PathBuf> = data_dir_for(&which);
        match dir {
            Some(p) => {
                let _ = std::fs::create_dir_all(&p);
                let _ = std::process::Command::new("explorer.exe").arg(&p).spawn();
                let _ = req.respond(json_response_for_origin(
                    200,
                    r#"{"status":"ok"}"#.into(),
                    origin.as_deref(),
                ));
            }
            None => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    r#"{"error":"campo inválido: which (appdata|temp)"}"#.into(),
                    origin.as_deref(),
                ));
            }
        }
        return;
    }
    if method == "GET" && url == "/api/status" {
        let _ = req.respond(json_response_for_origin(
            200,
            status_json(mgr.get_status()),
            origin.as_deref(),
        ));
        return;
    }

    // Ajustes de la UI (Settings): `GET` devuelve solo las secciones
    // editables; el POST acepta un cuerpo parcial, rechaza claves
    // desconocidas, valida rangos y persiste de forma atómica. Los cambios
    // que afectan al motor (engine) se aplican en el PRÓXIMO arranque: el
    // motor en curso nunca se reconfigura ni se reinicia desde aquí.
    if method == "GET" && url == "/api/config" {
        let c = cfg.get();
        let _ = req.respond(json_response_for_origin(
            200,
            app_config_json(&c),
            origin.as_deref(),
        ));
        return;
    }

    // Última sesión (la UI restaura modelo/perfil/contexto al arrancar).
    // Ruta viva: `api.getSettings()` la usa en el boot (ui.html) y sin ella
    // responde 404 y la UI nunca restaura (dead_code en `settings_json`).
    if method == "GET" && url == "/api/settings" {
        let c = cfg.get();
        let _ = req.respond(json_response_for_origin(
            200,
            settings_json(&c, cfg.path(), http_port),
            origin.as_deref(),
        ));
        return;
    }

    if method == "POST" && url == "/api/config" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": format!("JSON inválido: {}", e) }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        // Validación semántica en `config::apply_config_patch` (Fase A4): el
        // handler solo traduce `Err` a 400 con forma `{"error": ...}`.
        let next = match crate::config::apply_config_patch(&cfg.get(), &v) {
            Ok(n) => n,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        // Todo válido: persistencia atómica (memoria + TOML). Si el disco
        // falla, se revierte la memoria para no mentir a la UI.
        let prev = cfg.get();
        cfg.update(|c| *c = next.clone());
        if let Err(e) = cfg.save() {
            cfg.update(|c| *c = prev);
            let _ = req.respond(json_response_for_origin(500, serde_json::json!({ "error": format!("no se pudo guardar la configuración: {}", e) }).to_string(), origin.as_deref()));
            return;
        }
        mgr.log("[LocalMind] Configuración actualizada (aplica al próximo arranque del motor).");
        let _ = req.respond(json_response_for_origin(
            200,
            app_config_json(&next),
            origin.as_deref(),
        ));
        return;
    }

    if method == "GET" && url == "/api/metrics" {
        let st = mgr.get_status();
        if (st.status != "running" && !st.is_healthy) || st.port == 0 {
            let _ = req.respond(json_response_for_origin(
                502,
                r#"{"error":"engine_down"}"#.into(),
                origin.as_deref(),
            ));
            return;
        }
        match ureq::get(&format!("http://127.0.0.1:{}/metrics", st.port))
            // El motor exige `--api-key` (D-45): sin la cabecera responde 401.
            .set("Authorization", &crate::auth::bearer(&gateway_key))
            .timeout(Duration::from_secs(2))
            .call()
        {
            Ok(resp) => {
                let mut body = String::new();
                if resp.into_reader().read_to_string(&mut body).is_ok() {
                    let m = parse_metrics(&body);
                    let json = serde_json::json!({
                        "gen_tps": m.gen_tps,
                        "prompt_tps": m.prompt_tps,
                        "tokens_predicted": m.tokens_pred,
                        "prompt_tokens_total": m.prompt_tot,
                        "prompt_tokens_cached": m.cached_tot,
                        "requests_processing": m.requests_processing,
                        "spec_draft_tokens": m.draft_tot,
                        "spec_accepted_tokens": m.accepted_tot,
                    });
                    let _ = req.respond(json_response_for_origin(
                        200,
                        json.to_string(),
                        origin.as_deref(),
                    ));
                } else {
                    let _ = req.respond(json_response_for_origin(
                        502,
                        r#"{"error":"engine_read_failed"}"#.into(),
                        origin.as_deref(),
                    ));
                }
            }
            Err(_) => {
                let _ = req.respond(json_response_for_origin(
                    502,
                    r#"{"error":"engine_down"}"#.into(),
                    origin.as_deref(),
                ));
            }
        }
        return;
    }

    if method == "POST" && url == "/api/start" {
        // Fase B3: en modo Cliente esta PC no computa: arrancar el motor acá
        // sería mentirle a la UI (el cómputo vive en el remoto).
        if cfg.get().client.enabled {
            let _ = req.respond(json_response_for_origin(
                409,
                r#"{"error":"modo_cliente_sin_motor"}"#.into(),
                origin.as_deref(),
            ));
            return;
        }
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let start_req: StartRequest = match parse_start_body(&body) {
            Ok(r) => r,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        match mgr.start(start_req) {
            Ok(pid) => {
                let _ = req.respond(json_response_for_origin(
                    200,
                    format!(r#"{{"status":"starting","pid":{}}}"#, pid),
                    origin.as_deref(),
                ));
            }
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
            }
        }
        return;
    }

    if method == "POST" && url == "/api/stop" {
        mgr.stop();
        let _ = req.respond(json_response_for_origin(
            200,
            r#"{"status":"stopped"}"#.into(),
            origin.as_deref(),
        ));
        return;
    }

    // ---- Fase B3: descubrimiento LAN, pairing y remoto (diseño B5/B6/B9) ----
    // Todas bajo la puerta de auth D-7 de arriba: el payload de pairing lleva
    // la clave vigente y solo lo ve el dueño autenticado.

    // Descubrimiento + QR: IPs privadas de esta PC, puerto ligado y payloads
    // `omni://ip:puerto#k=clave` (uno por IP).
    if method == "GET" && url == "/api/lan" {
        let c = cfg.get();
        let ips = crate::meta::descubrir_ips_locales();
        let pairings: Vec<String> = ips
            .iter()
            .map(|ip| crate::meta::pairing_url(ip, http_port, &gateway_key))
            .collect();
        let body = serde_json::json!({
            "enabled": c.lan.enabled,
            "client_mode": c.client.enabled,
            "ips": ips,
            "port": http_port,
            "pairings": pairings,
            // El gateway no termina TLS en v1 (tiny_http plano): la LAN va en
            // HTTP sin cifrar. La UI lo avisa; la API lo declara.
            "plaintext_http": true,
        });
        let _ = req.respond(json_response_for_origin(
            200,
            body.to_string(),
            origin.as_deref(),
        ));
        return;
    }

    // Rotación de clave: persiste la nueva e invalida clientes viejos al
    // reiniciar (gateway y motor usan la instantánea del arranque, ver
    // `auth::rotate_key`). Nunca devuelve la clave por esta vía: la nueva se
    // recoge por pairing (`GET /api/lan`) tras reiniciar.
    if method == "POST" && url == "/api/key/rotate" {
        match crate::auth::rotate_key() {
            Ok(_) => {
                mgr.log("[LocalMind] Clave del gateway rotada: reiniciar para que tome efecto en gateway y motor.");
                let _ = req.respond(json_response_for_origin(
                    200,
                    r#"{"status":"ok","restart_required":true}"#.into(),
                    origin.as_deref(),
                ));
            }
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    500,
                    serde_json::json!({ "error": format!("no se pudo rotar la clave: {}", e) })
                        .to_string(),
                    origin.as_deref(),
                ));
            }
        }
        return;
    }

    // Probar remoto sin persistir: valida la forma y hace `GET /v1/models`
    // contra el candidato (8 s). La clave viaja solo en el header Bearer del
    // chequeo; el error nunca la incluye.
    if method == "POST" && url == "/api/remote/test" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": format!("JSON inválido: {}", e) }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let url_in = v
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let key_in = v
            .get("key")
            .and_then(|k| k.as_str())
            .unwrap_or("")
            .to_string();
        if !crate::config::remote_url_ok(&url_in) {
            let _ = req.respond(json_response_for_origin(
                400,
                r#"{"error":"url remota inválida"}"#.into(),
                origin.as_deref(),
            ));
            return;
        }
        let base = url_in.trim_end_matches('/').to_string();
        let models_url = if base.ends_with("/v1/models") {
            base
        } else if base.ends_with("/v1") {
            format!("{}/models", base)
        } else {
            format!("{}/v1/models", base)
        };
        match ureq::get(&models_url)
            .set("Authorization", &crate::auth::bearer(&key_in))
            .timeout(Duration::from_secs(8))
            .call()
        {
            Ok(resp) => {
                let text = resp.into_string().unwrap_or_default();
                let ids: Vec<String> = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|j| j.get("data").and_then(|d| d.as_array()).cloned())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                    .collect();
                let _ = req.respond(json_response_for_origin(
                    200,
                    serde_json::json!({ "ok": true, "models": ids }).to_string(),
                    origin.as_deref(),
                ));
            }
            Err(ureq::Error::Status(code, _)) => {
                let _ = req.respond(json_response_for_origin(200, serde_json::json!({ "ok": false, "error": format!("el remoto respondió {}", code) }).to_string(), origin.as_deref()));
            }
            Err(e) => {
                let _ = req.respond(json_response_for_origin(200, serde_json::json!({ "ok": false, "error": format!("no se pudo contactar al remoto: {}", e) }).to_string(), origin.as_deref()));
            }
        }
        return;
    }

    // Persistir remoto: valida la URL y guarda sin devolver la clave (el GET
    // de estado nunca la expone completa; el log no la nombra).
    if method == "POST" && url == "/api/remote" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": format!("JSON inválido: {}", e) }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let url_in = v
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !crate::config::remote_url_ok(&url_in) {
            let _ = req.respond(json_response_for_origin(
                400,
                r#"{"error":"url remota inválida"}"#.into(),
                origin.as_deref(),
            ));
            return;
        }
        let prev = cfg.get();
        cfg.update(|c| {
            c.remote.url = url_in.clone();
            if let Some(k) = v.get("key").and_then(|k| k.as_str()) {
                c.remote.key = k.to_string();
            }
        });
        if let Err(e) = cfg.save() {
            cfg.update(|c| *c = prev);
            let _ = req.respond(json_response_for_origin(
                500,
                serde_json::json!({ "error": format!("no se pudo guardar el remoto: {}", e) })
                    .to_string(),
                origin.as_deref(),
            ));
            return;
        }
        mgr.log("[LocalMind] Remoto actualizado (aplica al modo Cliente).");
        let _ = req.respond(json_response_for_origin(
            200,
            serde_json::json!({ "status": "ok", "url": url_in }).to_string(),
            origin.as_deref(),
        ));
        return;
    }

    if method == "GET" && url == "/api/logs" {
        let json =
            serde_json::to_string(&mgr.get_recent_logs()).unwrap_or_else(|_| "[]".to_string());
        let _ = req.respond(json_response_for_origin(200, json, origin.as_deref()));
        return;
    }

    // SSE: history bufferizada con seq + live. Reader bloqueante sobre mpsc.
    if method == "GET" && url == "/api/events" {
        let rx = mgr.subscribe_logs();
        let initial: Vec<LogEvent> = mgr.get_log_events();
        let reader: Box<dyn Read + Send + Sync> =
            Box::new(crate::sse::EventReader::new(rx, initial));
        let mut resp = Response::new(StatusCode(200), Vec::new(), reader, None, None);
        for h in sse_headers() {
            resp.add_header(h);
        }
        if let Some(o) = cors_origin_header(origin_ref) {
            resp.add_header(o);
        }
        let _ = req.respond(resp);
        return;
    }

    if method == "POST" && url == "/api/select_folder" {
        let folder = rfd::FileDialog::new()
            .set_title("Selecciona la carpeta de tu proyecto")
            .pick_folder();

        if let Some(path) = folder {
            let p_str = path.to_string_lossy().to_string();
            let _ = req.respond(json_response_for_origin(
                200,
                serde_json::json!({ "status": "ok", "path": p_str }).to_string(),
                origin.as_deref(),
            ));
        } else {
            let _ = req.respond(json_response_for_origin(
                200,
                serde_json::json!({ "status": "cancelled" }).to_string(),
                origin.as_deref(),
            ));
        }
        return;
    }
    // Lanzador genérico: un selector en la UI (`pi`/`omp`/`web`/`deepseek`).
    // `/api/launch_omp` y `/api/launch_pi` siguen como reenvíos finos hasta
    // que la nueva UI esté en producción; entonces podrán eliminarse.
    if method == "POST" && url == "/api/launch" {
        handle_launch_generic(req, &mgr, &cfg, &gateway_key, http_port, origin.clone());
        return;
    }
    if method == "POST" && url == "/api/launch_omp" {
        handle_launch_compat(
            req,
            &mgr,
            &cfg,
            "omp",
            &gateway_key,
            http_port,
            origin.clone(),
        );
        return;
    }

    if method == "POST" && url == "/api/launch_pi" {
        handle_launch_compat(
            req,
            &mgr,
            &cfg,
            "pi",
            &gateway_key,
            http_port,
            origin.clone(),
        );
        return;
    }

    if method == "GET" && url == "/api/agents" {
        let _ = req.respond(json_response_for_origin(
            200,
            crate::launcher::agents_list_json(),
            origin.as_deref(),
        ));
        return;
    }

    if method == "POST" && url == "/api/open_browser" {
        let st = mgr.get_status();
        let port = st.port;
        let _ = std::process::Command::new("cmd.exe")
            .args(["/c", &format!("start http://127.0.0.1:{}", port)])
            .spawn();
        mgr.log(&format!(
            "[LocalMind] Abierto navegador web en http://127.0.0.1:{}",
            port
        ));
        let _ = req.respond(json_response_for_origin(
            200,
            r#"{"status":"ok"}"#.into(),
            origin.as_deref(),
        ));
        return;
    }

    // Asesor de compatibilidad (portabilidad): `POST /api/models/advise
    // {"repo","revision?","files?"}` resuelve el árbol HF, estima si el peso
    // cabe en el HW del host y devuelve `{verdict, file, size_mb, profile?,
    // detail, hint_quant?}`. Sin descargas, sin estado: solo lectura + red HF.
    if method == "POST" && url == "/api/models/advise" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": format!("JSON inválido: {}", e) }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let repo = v
            .get("repo")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if let Err(e) = crate::models::check_repo(&repo) {
            let _ = req.respond(json_response_for_origin(
                400,
                serde_json::json!({ "error": e }).to_string(),
                origin.as_deref(),
            ));
            return;
        }
        let revision = v
            .get("revision")
            .and_then(|r| r.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(r) = revision.as_deref() {
            if let Err(e) = crate::models::check_revision(r) {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        }
        let only_file = v
            .get("files")
            .and_then(|f| f.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(f) = only_file.as_deref() {
            if let Err(e) = crate::models::check_rel_path(f) {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        }
        let rev = match revision {
            Some(r) => r,
            None => match crate::models::resolve_main_sha(&repo) {
                Ok(s) => s,
                Err(e) => {
                    let _ = req.respond(json_response_for_origin(
                        502,
                        serde_json::json!({ "error": e }).to_string(),
                        origin.as_deref(),
                    ));
                    return;
                }
            },
        };
        let tree = match crate::models::fetch_tree(&repo, &rev) {
            Ok(t) => t,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    502,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let hw = mgr.detect_hardware();
        let ans = crate::models::advise_fit(
            &tree,
            only_file.as_deref(),
            hw.vram_total_mb,
            hw.ram_total_mb,
        );
        let _ = req.respond(json_response_for_origin(
            200,
            ans.to_string(),
            origin.as_deref(),
        ));
        return;
    }
    if method == "POST" && url == "/api/models/download" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": format!("JSON inválido: {}", e) }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let repo = v
            .get("repo")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if let Err(e) = crate::models::check_repo(&repo) {
            let _ = req.respond(json_response_for_origin(
                400,
                serde_json::json!({ "error": e }).to_string(),
                origin.as_deref(),
            ));
            return;
        }
        let revision = v
            .get("revision")
            .and_then(|r| r.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(r) = revision.as_deref() {
            if let Err(e) = crate::models::check_revision(r) {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        }
        let only_file = v
            .get("files")
            .and_then(|f| f.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(f) = only_file.as_deref() {
            if let Err(e) = crate::models::check_rel_path(f) {
                let _ = req.respond(json_response_for_origin(
                    400,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        }
        let models_dir = mgr.models_dir().to_path_buf();
        // P1 producción (espacio, ver models.rs `run_job`/`download_one` como
        // modelo + `fits_download` 2×): el pre-chequeo fino vive en el worker
        // (ahí se conoce el total real del árbol, ~15GB); aquí NO se bloquea
        // sin dato. El worker devuelve el mensaje 507 y el estado del job lo
        // expone; este POST solo rechaza el 409 de trabajo en curso.
        let logger: std::sync::Arc<dyn Fn(String) + Send + Sync> = {
            let m = Arc::clone(&mgr);
            std::sync::Arc::new(move |line: String| m.log(&line))
        };
        match crate::models::start_job(models_dir, logger, repo, revision, only_file) {
            Ok(()) => {
                let _ = req.respond(json_response_for_origin(
                    200,
                    crate::models::job_snapshot().json(),
                    origin.as_deref(),
                ));
            }
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    409,
                    serde_json::json!({ "error": e }).to_string(),
                    origin.as_deref(),
                ));
            }
        }
        return;
    }
    // P1 producción: estado del job de descarga con 507 temprano. El worker
    // (`models.rs run_job`/`download_one`) aborta sin bajar nada si no cabe
    // el total con margen 2×; el GET lo traduce a 507 con el mismo mensaje
    // para que la UI no espere un `downloading` que nunca avanza. Sin error
    // de espacio → 200 con el snapshot.
    if method == "GET" && url == "/api/models/download" {
        let snap = crate::models::job_snapshot();
        let body = snap.json();
        if snap.state == "error" && body.contains("Sin espacio") {
            let _ = req.respond(json_response_for_origin(507, body, origin.as_deref()));
        } else {
            let _ = req.respond(json_response_for_origin(200, body, origin.as_deref()));
        }
        return;
    }

    if method == "POST" && url == "/api/models/download/cancel" {
        let _ = read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY);
        let _ = req.respond(json_response_for_origin(
            200,
            crate::models::job_cancel().json(),
            origin.as_deref(),
        ));
        return;
    }

    if method == "POST" && url == "/api/models/import_pick" {
        let body = match read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY) {
            Ok(b) => b,
            Err((code, msg)) => {
                let _ = req.respond(json_response_for_origin(
                    code,
                    serde_json::json!({ "error": msg }).to_string(),
                    origin.as_deref(),
                ));
                return;
            }
        };
        let overwrite = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("overwrite").and_then(|o| o.as_bool()))
            .unwrap_or(false);
        // El diálogo nativo bloquea: hilo propio, nunca el hilo HTTP; espera
        // acotada (120 s) para no dejar la conexión colgada. Al vencer se
        // responde `cancelled/timeout` y el hilo del diálogo queda inofensivo
        // (su resultado ya nadie lo lee).
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Elige uno o varios .gguf para importar a models/")
                .add_filter("GGUF", &["gguf"])
                .pick_files();
            let _ = tx.send(picked);
        });
        let picked = rx.recv_timeout(Duration::from_secs(crate::models::IMPORT_PICK_TIMEOUT_SECS));
        let picked = match picked {
            Ok(p) => p,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = req.respond(json_response_for_origin(
                    200,
                    r#"{"status":"cancelled","reason":"timeout"}"#.into(),
                    origin.as_deref(),
                ));
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        };
        // 409 por VARIANTE (ABIERTO-1 wave-6): se mapea junto al error, no
        // por substring del JSON (un nombre con "ya existe" en otro error no
        // produce un 409 espurio).
        let (code, body_out) = match picked {
            None => (200, r#"{"status":"cancelled"}"#.to_string()),
            Some(files) if files.is_empty() => (200, r#"{"status":"cancelled"}"#.to_string()),
            // ABIERTO-1 wave-5: vía `mgr.import_picked` (mismo `import_lock`
            // que `import_model_from_path`); pick-vs-pick y pick-vs-import ya
            // no violan el 409.
            Some(files) => match mgr.import_picked(&files, overwrite) {
                Ok(names) => {
                    mgr.log(&format!(
                        "[LocalMind] Modelos importados: {}",
                        names.join(", ")
                    ));
                    (
                        200,
                        serde_json::json!({ "status": "ok", "files": names }).to_string(),
                    )
                }
                Err(e) => {
                    let code = if e.is_exists() { 409 } else { 400 };
                    (
                        code,
                        serde_json::json!({ "error": e.to_string() }).to_string(),
                    )
                }
            },
        };
        let _ = req.respond(json_response_for_origin(code, body_out, origin.as_deref()));
        return;
    }

    // Telemetría visible: resumen incremental + últimas líneas (LM-TEL-3).
    if method == "GET" && url == "/api/usage" {
        let _ = req.respond(json_response_for_origin(
            200,
            crate::usage::usage_summary(),
            origin.as_deref(),
        ));
        return;
    }

    if method == "GET" && (url == "/api/usage/raw" || url.starts_with("/api/usage/raw?")) {
        let limit = url
            .split('?')
            .nth(1)
            .unwrap_or("")
            .split('&')
            .filter_map(|kv| {
                let mut it = kv.splitn(2, '=');
                match (it.next(), it.next()) {
                    (Some("limit"), Some(val)) => val.parse::<usize>().ok(),
                    _ => None,
                }
            })
            .next()
            .unwrap_or(100)
            .min(500);
        let _ = req.respond(json_response_for_origin(
            200,
            crate::usage::usage_raw(limit),
            origin.as_deref(),
        ));
        return;
    }

    // Superficie OpenAI: lista de modelos (id servido + alias estables).
    if method == "GET" && (url == "/v1/models" || url == "/v1/models/") {
        // Fase B3b: en modo Cliente el catálogo vive en el Servidor.
        if cfg.get().client.enabled {
            handle_remote_forward(req, &cfg, "GET", "/v1/models", origin.clone());
            return;
        }
        let st = mgr.get_status();
        let json = models_list_json(&st, &cfg);
        let _ = req.respond(json_response_for_origin(200, json, origin.as_deref()));
        return;
    }

    // Proxy SSE LIVE: reader de ureq directo en el body de tiny_http.
    // Fase B3b: en modo Cliente se reenvía al Servidor remoto.
    if method == "POST" && url == "/v1/chat/completions" {
        if cfg.get().client.enabled {
            handle_remote_forward(req, &cfg, "POST", "/v1/chat/completions", origin.clone());
            return;
        }
        handle_chat_completions(req, &mgr, &cfg, origin.clone());
        return;
    }

    // Claude Code: Anthropic Messages (en Cliente, reenvío al remoto).
    if method == "POST" && url == "/v1/messages" {
        if cfg.get().client.enabled {
            handle_remote_forward(req, &cfg, "POST", "/v1/messages", origin.clone());
            return;
        }
        handle_anthropic_messages(req, &mgr, &cfg, origin.clone());
        return;
    }

    // Codex: OpenAI Responses (en Cliente, reenvío al remoto).
    if method == "POST" && url == "/v1/responses" {
        if cfg.get().client.enabled {
            handle_remote_forward(req, &cfg, "POST", "/v1/responses", origin.clone());
            return;
        }
        handle_responses(req, &mgr, &cfg, origin.clone());
        return;
    }

    let _ = req.respond(json_response_for_origin(
        404,
        "{\"error\":\"not_found\"}".into(),
        origin.as_deref(),
    ));
}
/// Acción del navegador (misma que `/api/open_browser`): abre la interfaz
/// web externa en el puerto del motor. El llamador responde `{"status":"ok"}`.
fn open_browser_action(mgr: &Arc<ProcessManager>) {
    let st = mgr.get_status();
    let port = st.port;
    let _ = std::process::Command::new("cmd.exe")
        .args(["/c", &format!("start http://127.0.0.1:{}", port)])
        .spawn();
    mgr.log(&format!(
        "[LocalMind] Abierto navegador web en http://127.0.0.1:{}",
        port
    ));
}

/// Contexto compartido de los lanzadores (Fase UX-Guest): lo que todo
/// lanzamiento necesita del request + estado vivo, sin firmas de 8-9
/// parámetros. `gateway_key`/`http_port` solo se usan en la rama Oráculo
/// (Guest resuelve el remoto desde `cfg`).
struct LaunchCtx<'a> {
    mgr: &'a Arc<ProcessManager>,
    cfg: &'a Arc<ConfigStore>,
    gateway_key: &'a str,
    http_port: u16,
}

/// Lanzador genérico (`POST /api/launch {"agent":"pi"|"omp"|"opencode"|"web"|"deepseek"}`).
///
/// - `pi`/`omp`: escribe `%APPDATA%\LocalMind\agents\<agent>\` con el puerto,
///   contexto y clave VIVOS, lanza con `PI_CODING_AGENT_DIR` por Windows
///   Terminal → `cmd start`. Sin motor vivo: 409 con el mensaje existente.
///   `effort` (`off`|`low`|`medium`|`high`|`max`): nivel `--thinking` del CLI;
///   omitido => `low` (la fase de razonamiento domina la latencia al primer
///   token, así que el default arranca en el modo más rápido).
/// - `opencode`: `OPENCODE_CONFIG_CONTENT` por env + XDG aislados (409 sin motor).
/// - `web`: abre la interfaz web externa (misma acción que
///   `/api/open_browser`: el navegador en el puerto del motor).
/// - `deepseek`: harness `dsh --profile headless` (501 sin binario).
fn handle_launch_generic(
    mut req: tiny_http::Request,
    mgr: &Arc<ProcessManager>,
    cfg: &Arc<ConfigStore>,
    gateway_key: &str,
    http_port: u16,
    origin: Option<String>,
) {
    // Contexto compartido (Fase UX-Guest): los lanzadores ya no arrastran
    // 8-9 parámetros sueltos.
    let ctx = LaunchCtx {
        mgr,
        cfg,
        gateway_key,
        http_port,
    };
    // Drenaje acotado: el cuerpo es opcional (`{}` si falta/rebosa); el parse
    // ya tolera vacío con `unwrap_or(Null)`, así que un exceso no es 413.
    let body = read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY).unwrap_or_default();
    let body_val =
        serde_json::from_str::<serde_json::Value>(&body).unwrap_or(serde_json::Value::Null);
    let agent_raw = body_val
        .get("agent")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string();
    let req_dir = body_val
        .get("cwd")
        .and_then(|d| d.as_str().map(str::to_string));
    let req_effort = body_val
        .get("effort")
        .and_then(|e| e.as_str().map(str::to_string));
    // `task` lo usan `deepseek`/`opencode` (one-shot); para pi/omp/web se
    // ignora. Sin `task` se abre el harness en modo interactivo.
    let req_task = body_val
        .get("task")
        .and_then(|t| t.as_str().map(str::to_string));
    let id = match crate::launcher::parse_agent_id(&agent_raw) {
        Some(id) => id,
        None => {
            let _ = req.respond(json_response_for_origin(
                400,
                serde_json::json!({ "error": format!("Agente desconocido: '{}'. Use pi, omp, opencode, web o deepseek.", agent_raw) }).to_string(),
                origin.as_deref(),
            ));
            return;
        }
    };
    // `web` = misma acción que `/api/open_browser` (navegador al motor).
    // En Guest no hay motor local que mostrar: 409 honesto.
    if id == crate::launcher::AgentId::Web {
        if cfg.get().client.enabled {
            let _ = req.respond(json_response_for_origin(409, r#"{"error":"modo_guest_sin_motor_local","message":"En modo Guest no hay motor local: usa el Chat contra el Oráculo."}"#.into(), origin.as_deref()));
            return;
        }
        open_browser_action(mgr);
        let _ = req.respond(json_response_for_origin(
            200,
            r#"{"status":"ok"}"#.into(),
            origin.as_deref(),
        ));
        return;
    }

    // `deepseek` = harness oficial `@deepseek-ai/dsh` (verificado en
    // `docs/agents/deepseek-harness.md`). Sin binario instalado: 501 para
    // mostrarlo deshabilitado en la UI. Con `task` = one-shot headless, sin
    // ella = terminal interactiva con el modelo local.
    // `web` = navegador al motor; `deepseek` sin binario = 501 (el mapping
    // vive en `launcher`, testeado). `task` solo la usa `deepseek`.
    match crate::launcher::launch_action_for(id, crate::launcher::deepseek_installed()) {
        crate::launcher::LaunchAction::Web => {
            open_browser_action(mgr);
            let _ = req.respond(json_response_for_origin(
                200,
                r#"{"status":"ok"}"#.into(),
                origin.as_deref(),
            ));
            return;
        }
        crate::launcher::LaunchAction::PendingHarness => {
            let _ = req.respond(json_response_for_origin(
                501,
                serde_json::json!({ "error": crate::launcher::deepseek_not_configured_msg() })
                    .to_string(),
                origin.as_deref(),
            ));
            return;
        }
        crate::launcher::LaunchAction::Cli(_) => {}
    }
    // Guardia de motor con el estado del MANAGER QUE NOS LLAMÓ (mismo `mgr`):
    // `launch_cli`/`launch_opencode` revalidan con su propio `mgr`, así que el
    // 409 siempre refleja el estado real del motor de esta instancia.
    // `deepseek`/`opencode` = harnesses reales; `pi`/`omp` = núcleo CLI.
    if id == crate::launcher::AgentId::DeepSeek {
        launch_deepseek(
            req,
            &ctx,
            req_dir.as_deref(),
            req_task.as_deref(),
            origin.as_deref(),
        );
        return;
    }
    if id == crate::launcher::AgentId::OpenCode {
        launch_opencode(
            req,
            &ctx,
            req_dir.as_deref(),
            req_task.as_deref(),
            origin.as_deref(),
        );
        return;
    }
    // `pi`/`omp` llegan aquí (el `match` ya resolvió `web` y el 501).
    launch_cli(
        req,
        &ctx,
        id,
        req_dir.as_deref(),
        req_effort.as_deref(),
        origin.as_deref(),
    );
}

/// Reenvíos finos de `/api/launch_omp` y `/api/launch_pi` (los usa la UI).
fn handle_launch_compat(
    mut req: tiny_http::Request,
    mgr: &Arc<ProcessManager>,
    cfg: &Arc<ConfigStore>,
    agent: &str,
    gateway_key: &str,
    http_port: u16,
    origin: Option<String>,
) {
    // Drenaje acotado: igual que el genérico, cuerpo opcional.
    let body = read_bounded_body(&mut req.as_reader(), MAX_SMALL_JSON_BODY).unwrap_or_default();
    let body_val =
        serde_json::from_str::<serde_json::Value>(&body).unwrap_or(serde_json::Value::Null);
    let req_dir = body_val
        .get("cwd")
        .and_then(|d| d.as_str().map(str::to_string));
    let req_effort = body_val
        .get("effort")
        .and_then(|e| e.as_str().map(str::to_string));
    let id = match crate::launcher::parse_agent_id(agent) {
        Some(id) => id,
        None => {
            let _ = req.respond(json_response_for_origin(
                400,
                serde_json::json!({ "error": format!("Agente desconocido: '{}'.", agent) })
                    .to_string(),
                origin.as_deref(),
            ));
            return;
        }
    };
    launch_cli(
        req,
        &LaunchCtx {
            mgr,
            cfg,
            gateway_key,
            http_port,
        },
        id,
        req_dir.as_deref(),
        req_effort.as_deref(),
        origin.as_deref(),
    );
}

/// Helpers de spawn de terminal con verificación + fallback (`wt` → `cmd`).
/// `wt.exe` en `%LOCALAPPDATA%\Microsoft\WindowsApps` es un alias de ejecución
/// de 0 bytes: `p.exists()` lo selecciona aunque el spawn falle desde nuestro
/// contexto. Por eso la política es "try + verify" (`spawn_terminal`): si el
/// spawn de WT devuelve `Err`, se reintenta con `cmd start` y se registra qué
/// rama se usó; si ambas fallan el endpoint responde 500 en español.
fn wt_command(cmd_str: &str) -> Option<std::process::Command> {
    let wt_path = std::env::var("LOCALAPPDATA")
        .map(|l| PathBuf::from(l).join("Microsoft/WindowsApps/wt.exe"))
        .ok()?;
    if !wt_path.exists() {
        return None;
    }
    let mut c = std::process::Command::new(wt_path);
    c.args(["-w", "0", "new-tab", "cmd.exe", "/k", cmd_str]);
    Some(c)
}

fn cmd_start_command(cmd_str: &str) -> std::process::Command {
    let mut c = std::process::Command::new("cmd.exe");
    c.args(["/c", &format!("start cmd.exe /k \"{}\"", cmd_str)]);
    c
}

/// Modelo + contexto del Oráculo desde su `/api/status` (Fase UX-Guest, puro):
/// `model` con stem sin `.gguf` (alias `localmind` si vacío) y `context` > 0
/// (default 32768 si ausente o cero). Nunca falla: el llamador ya validó que
/// el remoto responde.
fn remote_status_target(body: &str) -> (String, usize) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("").trim();
    let stem = model
        .strip_suffix(".gguf")
        .or_else(|| model.strip_suffix(".GGUF"))
        .unwrap_or(model);
    let model_id = if stem.trim().is_empty() {
        "localmind".to_string()
    } else {
        stem.to_string()
    };
    let context = v
        .get("context")
        .and_then(|c| c.as_u64())
        .and_then(|c| usize::try_from(c).ok())
        .filter(|c| *c > 0)
        .unwrap_or(32768);
    (model_id, context)
}

/// Destino LLM de un lanzamiento (Fase UX-Guest): a QUÉ gateway habla el CLI.
/// En Oráculo (`client` apagado) es el gateway local con motor vivo, con los
/// mensajes 409 históricos intactos. En Guest es el Oráculo remoto, que
/// requiere remoto configurado y alcanzable (modelo/contexto del
/// `/api/status` remoto con fallbacks honestos).
/// `Err` = cuerpo JSON español listo para el 409.
fn resolve_launch_target(
    cfg: &crate::config::AppConfig,
    st: &ServerStatus,
    gateway_key: &str,
    http_port: u16,
) -> Result<crate::launcher::LlmTarget, String> {
    if cfg.client.enabled {
        let Some((root, key)) = remote_target(cfg) else {
            return Err(r#"{"error":"remoto_no_configurado","message":"Modo Guest sin Oráculo: configura el Servidor en Conexión."}"#.to_string());
        };
        let status_url = format!("{}/api/status", root);
        let body = ureq::get(&status_url)
            .set("Authorization", &crate::auth::bearer(&key))
            .timeout(Duration::from_secs(8))
            .call()
            .map(|r| r.into_string().unwrap_or_default())
            .map_err(|e| {
                format!(
                    r#"{{"error":"remoto_no_alcanzado","message":"No se pudo contactar al Oráculo: {}"}}"#,
                    e
                )
            })?;
        let (model_id, context) = remote_status_target(&body);
        return Ok(crate::launcher::LlmTarget::remote(
            &root, &key, &model_id, context,
        ));
    }
    if !crate::agents::engine_live(st) {
        return Err(crate::agents::engine_down_error(st));
    }
    let served = crate::agents::served_model_id(st).ok_or_else(|| {
        r#"{"error":"modelo_desconocido","message":"El motor está en marcha pero no reporta qué modelo sirve. Reinícielo con /api/start."}"#.to_string()
    })?;
    Ok(crate::launcher::LlmTarget::local(
        http_port,
        gateway_key,
        &served,
        st.context,
    ))
}

/// Los CLIs hablan con el GATEWAY ligado (`http://127.0.0.1:<http_port>/v1`,
/// propagado desde `HttpServer::start` por cada request) para pasar por
/// clave/aliasing/usage. El puerto del motor (`st.port`) es solo interno.
/// Núcleo CLI compartido (`pi`/`omp`; `opencode`/`deepseek` tienen lanzador
/// propio): 409 sin motor, dir privado con estado vivo, spawn verificado.
fn launch_cli(
    req: tiny_http::Request,
    ctx: &LaunchCtx,
    id: crate::launcher::AgentId,
    req_dir: Option<&str>,
    req_effort: Option<&str>,
    origin: Option<&str>,
) {
    let mgr = ctx.mgr;
    // Destino LLM (Fase UX-Guest): Oráculo = gateway local con motor vivo;
    // Guest = Oráculo remoto. Los 409 históricos salen del resolver.
    let target = match resolve_launch_target(
        &ctx.cfg.get(),
        &mgr.get_status(),
        ctx.gateway_key,
        ctx.http_port,
    ) {
        Ok(t) => t,
        Err(e) => {
            let _ = req.respond(json_response_for_origin(409, e, origin));
            return;
        }
    };
    // `served` es el id que atiende (local vivo o remoto): va al config
    // privado del agente, al `--model` y a la respuesta, los tres iguales.
    let served = target.model_id.clone();
    let context = target.context;
    let agent = match id {
        crate::launcher::AgentId::Pi => "pi",
        crate::launcher::AgentId::Omp => "omp",
        crate::launcher::AgentId::DeepSeek => "deepseek",
        crate::launcher::AgentId::OpenCode => "opencode",
        crate::launcher::AgentId::Web => {
            let _ = req.respond(json_response_for_origin(
                400,
                serde_json::json!({ "error": "El agente web no usa este lanzamiento." })
                    .to_string(),
                origin,
            ));
            return;
        }
    };
    let cli_model = crate::launcher::cli_model(id, &served);
    let label = crate::launcher::agent_label(id);
    let agent_dir = match crate::agents::write_agent_dir(
        agent,
        &target.base_url,
        context,
        &target.key,
        &served,
    ) {
        Ok(d) => d,
        Err(e) => {
            let _ = req.respond(json_response_for_origin(
                500,
                serde_json::json!({ "error": e }).to_string(),
                origin,
            ));
            return;
        }
    };

    // omp se niega a arrancar en el home sin --allow-home.
    let mut allow_home = String::new();
    if let Some(dir) = req_dir {
        if crate::agents::is_home_dir(dir) {
            allow_home = " --allow-home".to_string();
        }
    }

    let cd_prefix = match req_dir {
        Some(dir) if !dir.trim().is_empty() => format!("cd /d \"{}\" && ", dir),
        _ => String::new(),
    };
    let agent_path = agent_dir.to_string_lossy().to_string();
    let inner_cmd = crate::launcher::cli_inner_cmd(
        id,
        &cd_prefix,
        &target.base_url,
        &target.key,
        &agent_path,
        req_effort,
        &allow_home,
        &served,
    );

    // Terminal del sistema con verificación + fallback (`wt` → `cmd start`):
    // el spawn de WT puede devolver `Err` (alias reparse de 0 bytes) y antes
    // se tragaba con `let _`. Ahora se registra el pid o el error, y si ambas
    // ramas fallan el endpoint responde 500 en vez de un falso 200.
    let cmd_str = inner_cmd.clone();
    let wt = wt_command(&cmd_str);
    let (ok, branch, detail) =
        crate::launcher::spawn_terminal(wt, &|| cmd_start_command(&cmd_str), req_dir);
    let via = match branch {
        crate::launcher::SpawnBranch::Wt => "wt",
        crate::launcher::SpawnBranch::CmdStart => "cmd-start",
    };
    if !ok {
        mgr.log(&format!(
            "[LocalMind] ERROR al lanzar Terminal {} en '{}': {} (vía {}).",
            label,
            req_dir.unwrap_or("directorio default"),
            detail,
            via
        ));
        let _ = req.respond(json_response_for_origin(500, serde_json::json!({ "error": format!("No se pudo abrir la terminal ({}): {}", via, detail) }).to_string(), origin));
        return;
    }
    let via_donde = if target.remote {
        format!("remoto {}", target.base_url)
    } else {
        format!("gateway local :{}", ctx.http_port)
    };
    mgr.log(&format!(
        "[LocalMind] Terminal {} lanzada ({} {}) en '{}' ({}; ctx: {}) conectada a {}.",
        label,
        via,
        detail,
        req_dir.unwrap_or("directorio default"),
        via_donde,
        context,
        cli_model
    ));
    let _ = req.respond(json_response_for_origin(
        200,
        format!(
            r#"{{"status":"ok","model":"{}","port":{},"context":{},"remote":{}}}"#,
            cli_model,
            if target.remote { 0 } else { ctx.http_port },
            context,
            target.remote
        ),
        origin,
    ));
}

/// Lanzador DeepSeek (`dsh --profile headless ["<tarea>"]`, verificado en
/// `docs/agents/deepseek-harness.md`): env aislado (`DSH_HOME` propio,
/// telemetría off, `workspace-write`) y clave por `set /p` (nunca en argv).
/// Sin `task` = terminal interactiva con el modelo local; con `task` =
/// one-shot headless. La clave se lee aquí mismo de `gateway.key`.
fn launch_deepseek(
    req: tiny_http::Request,
    ctx: &LaunchCtx,
    req_dir: Option<&str>,
    req_task: Option<&str>,
    origin: Option<&str>,
) {
    let mgr = ctx.mgr;
    let inner = crate::launcher::deepseek_inner_cmd(req_task);
    let cd_prefix = match req_dir {
        Some(dir) if !dir.trim().is_empty() => format!("cd /d \"{}\" && ", dir),
        _ => String::new(),
    };
    // Patch Cordis generado con el destino VIVO (Oráculo local o remoto):
    // solo se reescribe si cambia (mtime estable). Sin destino no se escribe
    // ninguna config ni se abre terminal contra la nada (409 del resolver).
    let target = match resolve_launch_target(
        &ctx.cfg.get(),
        &ctx.mgr.get_status(),
        ctx.gateway_key,
        ctx.http_port,
    ) {
        Ok(t) => t,
        Err(e) => {
            let _ = req.respond(json_response_for_origin(409, e, origin));
            return;
        }
    };
    let home =
        crate::launcher::deepseek_home().unwrap_or_else(|| crate::agents::agent_dir("deepseek"));
    let patch =
        crate::launcher::deepseek_profile_patch(&target.base_url, target.context, &target.model_id);
    match crate::launcher::write_deepseek_patch(&home, &patch) {
        Ok(_) => {}
        Err(e) => {
            let _ = req.respond(json_response_for_origin(
                500,
                serde_json::json!({ "error": e }).to_string(),
                origin,
            ));
            return;
        }
    }
    let key_file = if target.remote {
        // Guest: la clave remota vive en `remote.key` del dir privado (nunca
        // en argv; el lanzador la lee con `set /p` igual que `gateway.key`).
        match crate::launcher::write_remote_key_file(&home, &target.key) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    500,
                    serde_json::json!({ "error": e }).to_string(),
                    origin,
                ));
                return;
            }
        }
    } else {
        crate::auth::gateway_key_path()
            .to_string_lossy()
            .to_string()
    };
    let cmd_str = crate::launcher::deepseek_cmdline(&cd_prefix, &inner, &key_file);
    let wt = wt_command(&cmd_str);
    let (ok, branch, detail) =
        crate::launcher::spawn_terminal(wt, &|| cmd_start_command(&cmd_str), req_dir);
    let via = match branch {
        crate::launcher::SpawnBranch::Wt => "wt",
        crate::launcher::SpawnBranch::CmdStart => "cmd-start",
    };
    let what = match req_task.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("tarea '{}'", t.chars().take(60).collect::<String>()),
        None => "sesión interactiva".to_string(),
    };
    if !ok {
        mgr.log(&format!(
            "[LocalMind] ERROR al lanzar Terminal DeepSeek harness en '{}' ({}): {} (vía {}).",
            req_dir.unwrap_or("directorio default"),
            what,
            detail,
            via
        ));
        let _ = req.respond(json_response_for_origin(500, serde_json::json!({ "error": format!("No se pudo abrir la terminal ({}): {}", via, detail) }).to_string(), origin));
        return;
    }
    mgr.log(&format!(
        "[LocalMind] Terminal DeepSeek harness lanzada ({} {}) en '{}' ({}).",
        via,
        detail,
        req_dir.unwrap_or("directorio default"),
        what
    ));
    let _ = req.respond(json_response_for_origin(
        200,
        r#"{"status":"ok","agent":"deepseek"}"#.into(),
        origin,
    ));
}

/// Lanzador OpenCode (`opencode --model localmind/<id> [run "<tarea>"]`,
/// verificado v1.18.10): `OPENCODE_CONFIG_CONTENT` inyectado por env con el
/// puerto/contexto VIVOS (clave por `{env:LOCALMIND_API_KEY}`, nunca cruda) y
/// dirs XDG aislados en el dir privado (nunca `~/.config/opencode`). Sin
/// motor vivo: 409; sin binario: 501 en español. Spawn verificado + fallback.
fn launch_opencode(
    req: tiny_http::Request,
    ctx: &LaunchCtx,
    req_dir: Option<&str>,
    req_task: Option<&str>,
    origin: Option<&str>,
) {
    let mgr = ctx.mgr;
    if !crate::launcher::opencode_installed() {
        let _ = req.respond(json_response_for_origin(
            501,
            serde_json::json!({ "error": "El harness de OpenCode todavía no está configurado" })
                .to_string(),
            origin,
        ));
        return;
    }
    // Destino LLM (Fase UX-Guest): Oráculo local con motor vivo o remoto.
    let target = match resolve_launch_target(
        &ctx.cfg.get(),
        &ctx.mgr.get_status(),
        ctx.gateway_key,
        ctx.http_port,
    ) {
        Ok(t) => t,
        Err(e) => {
            let _ = req.respond(json_response_for_origin(409, e, origin));
            return;
        }
    };
    // Id corto = el que atiende (local servido o remoto anunciado); el flag
    // lleva el prefijo de provider. baseURL = GATEWAY que atiende, no el motor.
    // La config se escribe como FICHERO en el dir privado (no por env: el
    // `set "VAR=<json>"` de cmd.exe corrompía el JSON con `\"` literales y
    // opencode ignoraba el provider → `ProviderModelNotFoundError`).
    let model_id = target.model_id.clone();
    let full = crate::launcher::cli_model(crate::launcher::AgentId::OpenCode, &model_id);
    let home = crate::agents::agent_dir("opencode");
    if let Err(e) = std::fs::create_dir_all(&home) {
        let _ = req.respond(json_response_for_origin(
            500,
            serde_json::json!({ "error": format!("No se pudo crear {}: {}", home.display(), e) })
                .to_string(),
            origin,
        ));
        return;
    }
    let content = crate::launcher::opencode_config_json(
        &target.base_url,
        target.context,
        &model_id,
        Some(&target.key),
    );
    match crate::launcher::write_opencode_config(&home, &content) {
        Ok(_) => {}
        Err(e) => {
            let _ = req.respond(json_response_for_origin(
                500,
                serde_json::json!({ "error": e }).to_string(),
                origin,
            ));
            return;
        }
    }
    let home_s = home.to_string_lossy().to_string();
    let key_file = if target.remote {
        // Guest: clave remota en `remote.key` del dir privado (nunca en argv).
        match crate::launcher::write_remote_key_file(&home, &target.key) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(e) => {
                let _ = req.respond(json_response_for_origin(
                    500,
                    serde_json::json!({ "error": e }).to_string(),
                    origin,
                ));
                return;
            }
        }
    } else {
        crate::auth::gateway_key_path()
            .to_string_lossy()
            .to_string()
    };
    let inner = crate::launcher::opencode_inner_cmd(&model_id, req_task);
    let cd_prefix = match req_dir {
        Some(dir) if !dir.trim().is_empty() => format!("cd /d \"{}\" && ", dir),
        _ => String::new(),
    };
    let cmd_str = crate::launcher::opencode_cmdline(&cd_prefix, &home_s, &key_file, &inner);
    let wt = wt_command(&cmd_str);
    let (ok, branch, detail) =
        crate::launcher::spawn_terminal(wt, &|| cmd_start_command(&cmd_str), req_dir);
    let via = match branch {
        crate::launcher::SpawnBranch::Wt => "wt",
        crate::launcher::SpawnBranch::CmdStart => "cmd-start",
    };
    let what = match req_task.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("tarea '{}'", t.chars().take(60).collect::<String>()),
        None => "sesión interactiva".to_string(),
    };
    if !ok {
        mgr.log(&format!(
            "[LocalMind] ERROR al lanzar Terminal OpenCode en '{}' ({}): {} (vía {}).",
            req_dir.unwrap_or("directorio default"),
            what,
            detail,
            via
        ));
        let _ = req.respond(json_response_for_origin(500, serde_json::json!({ "error": format!("No se pudo abrir la terminal ({}): {}", via, detail) }).to_string(), origin));
        return;
    }
    mgr.log(&format!(
        "[LocalMind] Terminal OpenCode lanzada ({} {}) en '{}' ({}) con modelo {} ({}; ctx: {}).",
        via,
        detail,
        req_dir.unwrap_or("directorio default"),
        what,
        full,
        if target.remote {
            format!("remoto {}", target.base_url)
        } else {
            format!("gateway local :{}", ctx.http_port)
        },
        target.context
    ));
    let _ = req.respond(json_response_for_origin(200, format!(r#"{{"status":"ok","agent":"opencode","model":"{}","port":{},"context":{},"remote":{}}}"#, full, if target.remote { 0 } else { ctx.http_port }, target.context, target.remote), origin));
}

/// Raíz + clave del remoto configurado (Fase B3b): la URL guardada puede traer
/// o no el sufijo `/v1`; se normaliza a raíz para componer rutas. `None` =
/// sin remoto o con URL inválida (p. ej. TOML editado a mano: el POST la
/// valida, pero el disco manda).
fn remote_target(cfg: &crate::config::AppConfig) -> Option<(String, String)> {
    let url = cfg.remote.url.trim().trim_end_matches('/').to_string();
    if url.is_empty() || !crate::config::remote_url_ok(&url) {
        return None;
    }
    let root = url.strip_suffix("/v1").unwrap_or(&url).to_string();
    Some((root, cfg.remote.key.clone()))
}

/// Error de transporte hacia el remoto con la forma de cada dialecto (Fase
/// B3b): el chat habla OpenAI, `/v1/messages` Anthropic y `/v1/responses`
/// Responses. Los errores CON respuesta del remoto se reenvían tal cual
/// (ya vienen con su forma); esto es solo para "no se pudo contactar".
fn remote_transport_error(path: &str) -> (u16, String) {
    if path == "/v1/messages" {
        anthropic_error(502, "api_error", "Remoto no alcanzado (remote_unreachable)")
    } else if path == "/v1/responses" {
        responses_error(502, "remoto no alcanzado")
    } else {
        (502, r#"{"error":"Error al contactar remoto"}"#.to_string())
    }
}

/// Reenvío Cliente→Servidor (Fase B3b, diseño B6): el remoto es OTRO gateway
/// OMNI con la misma superficie `/v1/*`, así que se reenvían ruta y bytes sin
/// re-traducir (los lanzadores y sus dialectos siguen funcionando a través).
/// Streaming directo sin Tee local: la contabilidad vive en el Servidor; acá
/// solo se registra `*.remoto` sin tokens para no duplicar. La clave remota
/// solo viaja en el header Bearer; ningún error la incluye.
fn handle_remote_forward(
    mut req: tiny_http::Request,
    cfg: &Arc<ConfigStore>,
    method: &str,
    path: &str,
    origin: Option<String>,
) {
    let snapshot = cfg.get();
    let Some((root, key)) = remote_target(&snapshot) else {
        let _ = req.respond(json_response_for_origin(
            409,
            r#"{"error":"remoto_no_configurado"}"#.into(),
            origin.as_deref(),
        ));
        return;
    };
    let target = format!("{}{}", root, path);
    let body_bytes = match read_capped_bytes(&mut req.as_reader(), MAX_V1_BODY) {
        Ok(b) => b,
        Err((code, msg)) => {
            let _ = req.respond(json_response_for_origin(
                code,
                serde_json::json!({ "error": msg }).to_string(),
                origin.as_deref(),
            ));
            return;
        }
    };
    let req_model = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_else(|| "remoto".to_string());
    let endpoint_log = format!(
        "{}.remoto",
        path.trim_start_matches("/v1/").replace('/', ".")
    );
    let t0 = Instant::now();
    // El chat puede tardar minutos: sin timeout (igual que el proxy local).
    // `/v1/models` es control: 15 s para no colgar la UI.
    let call = if method == "GET" {
        ureq::get(&target)
            .set("Authorization", &crate::auth::bearer(&key))
            .timeout(Duration::from_secs(15))
            .call()
    } else {
        ureq::post(&target)
            .set("Content-Type", "application/json")
            .set("Authorization", &crate::auth::bearer(&key))
            .send_bytes(&body_bytes)
    };
    match call {
        Ok(resp) => {
            let status = resp.status();
            let ct = resp
                .header("content-type")
                .unwrap_or("application/json")
                .to_string();
            if status == 200 && ct.contains("text/event-stream") {
                // Streaming directo: cada chunk al cliente en cuanto llega.
                let reader = resp.into_reader();
                let sse_ct =
                    Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
                let mut hdrs = vec![sse_ct];
                if let Some(o) = cors_origin_header(origin.as_deref()) {
                    hdrs.push(o);
                }
                hdrs.extend(cors_fixed_headers());
                let proxy_resp = Response::new(StatusCode(200), hdrs, reader, None, None);
                let _ = req.respond(proxy_resp);
                crate::usage::log_usage(
                    &endpoint_log,
                    &req_model,
                    None,
                    None,
                    t0.elapsed().as_millis() as u64,
                    true,
                );
            } else {
                let mut raw = String::new();
                let _ = resp.into_reader().read_to_string(&mut raw);
                crate::usage::log_usage(
                    &endpoint_log,
                    &req_model,
                    None,
                    None,
                    t0.elapsed().as_millis() as u64,
                    false,
                );
                // Cuerpo tal cual (el remoto ya le dio forma); vacío → error
                // genérico con el código para no responder 200/4xx sin cuerpo.
                let body_out = if raw.is_empty() {
                    serde_json::json!({ "error": format!("el remoto respondió {}", status) })
                        .to_string()
                } else {
                    raw
                };
                let _ = req.respond(json_response_for_origin(
                    status,
                    body_out,
                    origin.as_deref(),
                ));
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut body = String::new();
            let _ = resp.into_reader().read_to_string(&mut body);
            let body_out = if body.is_empty() {
                serde_json::json!({ "error": format!("el remoto respondió {}", code) }).to_string()
            } else {
                body
            };
            let _ = req.respond(json_response_for_origin(code, body_out, origin.as_deref()));
        }
        Err(_) => {
            let (c, text) = remote_transport_error(path);
            let _ = req.respond(json_response_for_origin(c, text, origin.as_deref()));
        }
    }
}

/// `POST /v1/chat/completions`: alias rewriting + usage + registro.
fn handle_chat_completions(
    mut req: tiny_http::Request,
    mgr: &Arc<ProcessManager>,
    cfg: &Arc<ConfigStore>,
    origin: Option<String>,
) {
    // P1 producción: cap 25 MB anti-OOM (413 si excede).
    let body_bytes = match read_capped_bytes(&mut req.as_reader(), MAX_V1_BODY) {
        Ok(b) => b,
        Err((code, msg)) => {
            let _ = req.respond(json_response_for_origin(
                code,
                serde_json::json!({ "error": msg }).to_string(),
                origin.as_deref(),
            ));
            return;
        }
    };
    mgr.touch_activity();
    let st = mgr.get_status();
    // Fase A6: `stopped`/`error` con puerto placeholder no es motor (el 8080
    // lo puede ocupar cualquiera). `starting` sí proxyea: ya responde.
    if !crate::agents::engine_reachable(&st) {
        let _ = req.respond(json_response_for_origin(
            502,
            r#"{"error":"engine_down"}"#.into(),
            origin.as_deref(),
        ));
        return;
    }
    let served = served_model_id(&st, cfg);
    // Un solo parseo/serializado (auditoría de rendimiento): equivale al chain
    // de 5 pasadas que había acá (2× from_slice + sanitize + rewrite + ensure).
    let (payload_bytes, req_model, stream_req) = prepare_chat_payload(&body_bytes, &served);
    // Effort pedido a modelo sin thinking (portabilidad): se loguea una vez
    // por request (visible en Logs), no se rechaza —el motor responde igual,
    // solo sin razonar. La UI ya avisa antes de enviar.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) {
        let eff = v.get("reasoning_effort").and_then(|e| e.as_str());
        if effort_mismatch(&served, eff) {
            mgr.log(&format!(
                "[LocalMind] El modelo «{}» no trae razonamiento: el nivel '{}' se ignora y responde directo.",
                served,
                eff.unwrap_or("?"),
            ));
        }
    }
    let t0 = Instant::now();
    match ureq::post(&format!("http://127.0.0.1:{}/v1/chat/completions", st.port))
        .set("Content-Type", "application/json")
        // El motor exige `--api-key` (D-45): sin la cabecera responde 401 y el
        // proxy devolvería el error del motor al cliente en vez de traducirlo.
        .set(
            "Authorization",
            &crate::auth::bearer(crate::auth::gateway_key()),
        )
        .send_bytes(&payload_bytes)
    {
        Ok(resp) => {
            if stream_req {
                // Streaming incremental (LM-PXY-1/LM-UI-5): el tee cede cada
                // chunk al cliente en cuanto llega y registra el uso al EOF
                // desde la cola de 64 KiB.
                let reader = proxy::TeeLogReader::new(
                    resp.into_reader(),
                    "chat.completions".to_string(),
                    req_model,
                    t0,
                );
                let ct =
                    Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
                let mut hdrs = vec![ct];
                if let Some(o) = cors_origin_header(origin.as_deref()) {
                    hdrs.push(o);
                }
                hdrs.extend(cors_fixed_headers());
                let proxy_resp = Response::new(StatusCode(200), hdrs, reader, None, None);
                let _ = req.respond(proxy_resp);
            } else {
                let mut raw = String::new();
                let mut reader = resp.into_reader();
                if reader.read_to_string(&mut raw).is_err() {
                    let _ = req.respond(json_response_for_origin(
                        502,
                        r#"{"error":"Error al contactar motor"}"#.into(),
                        origin.as_deref(),
                    ));
                    return;
                }
                let (p, c) = crate::usage::extract_usage_from_sse(&raw);
                crate::usage::log_usage(
                    "chat.completions",
                    &req_model,
                    p,
                    c,
                    t0.elapsed().as_millis() as u64,
                    false,
                );
                // Reenviar el JSON tal cual con Content-Type json.
                let _ = req.respond(json_response_for_origin(200, raw, origin.as_deref()));
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut body = String::new();
            let _ = resp.into_reader().read_to_string(&mut body);
            let _ = req.respond(json_response_for_origin(code, body, origin.as_deref()));
        }
        Err(_) => {
            let _ = req.respond(json_response_for_origin(
                502,
                r#"{"error":"Error al contactar motor"}"#.into(),
                origin.as_deref(),
            ));
        }
    }
}

/// Error forma Anthropic (lo exigen los clientes).
fn anthropic_error(status: u16, err_type: &str, message: &str) -> (u16, String) {
    (
        status,
        serde_json::json!({"type": "error", "error": {"type": err_type, "message": message}})
            .to_string(),
    )
}

/// Error forma Responses/Codex.
fn responses_error(status: u16, message: &str) -> (u16, String) {
    (
        status,
        serde_json::json!({"error": {"message": message, "type": "api_error"}}).to_string(),
    )
}

/// Uso nativo (Anthropic y Responses) desde texto SSE o JSON, puro y testeable:
/// recorre líneas/eventos y toma el ÚLTIMO `usage` con input/output_tokens,
/// esté en la raíz o en `message.usage` / `response.usage`. Sin red, sin estado.
fn extract_native_usage(text: &str) -> (Option<u64>, Option<u64>) {
    fn usage_of(v: &serde_json::Value) -> Option<(Option<u64>, Option<u64>)> {
        let u = v.get("usage")?;
        let p = u.get("input_tokens").and_then(|x| x.as_u64());
        let c = u.get("output_tokens").and_then(|x| x.as_u64());
        if p.is_none() && c.is_none() {
            return None;
        }
        Some((p, c))
    }
    let mut p = None;
    let mut c = None;
    for line in text.lines() {
        let trimmed = line.trim();
        let payload = trimmed
            .strip_prefix("data:")
            .map(|s| s.trim())
            .unwrap_or(trimmed);
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        for cand in [&v, &v["message"], &v["response"]] {
            if let Some((pp, cc)) = usage_of(cand) {
                if pp.is_some() {
                    p = pp;
                }
                if cc.is_some() {
                    c = cc;
                }
            }
        }
    }
    (p, c)
}

/// Lector pasante con contabilidad nativa (auditoría: el motor ya habla los
/// dialectos, no se traduce nada): reenvía bytes intactos, acumula cola de
/// 64 KiB y al EOF registra el `usage` nativo. Misma cota que el Tee.
struct NativeUsageTee<R: Read + Send> {
    inner: R,
    buf: [u8; 8192],
    out: Vec<u8>,
    tail: Vec<u8>,
    done: bool,
    logged: bool,
    endpoint: String,
    model: String,
    t0: Instant,
}

impl<R: Read + Send> NativeUsageTee<R> {
    fn new(inner: R, endpoint: String, model: String, t0: Instant) -> Self {
        Self {
            inner,
            buf: [0u8; 8192],
            out: Vec::new(),
            tail: Vec::new(),
            done: false,
            logged: false,
            endpoint,
            model,
            t0,
        }
    }

    fn log_once(&mut self) {
        if self.logged {
            return;
        }
        self.logged = true;
        let text = String::from_utf8_lossy(&self.tail).to_string();
        let (p, c) = extract_native_usage(&text);
        crate::usage::log_usage(
            &self.endpoint,
            &self.model,
            p,
            c,
            self.t0.elapsed().as_millis() as u64,
            true,
        );
    }
}

impl<R: Read + Send> Read for NativeUsageTee<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if !self.out.is_empty() {
                let n = self.out.len().min(buf.len());
                buf[..n].copy_from_slice(&self.out[..n]);
                self.out.drain(..n);
                return Ok(n);
            }
            if self.done {
                self.log_once();
                return Ok(0);
            }
            match self.inner.read(&mut self.buf) {
                Ok(0) => {
                    self.done = true;
                }
                Ok(n) => {
                    crate::usage::push_tail(&mut self.tail, &self.buf[..n]);
                    self.out.extend_from_slice(&self.buf[..n]);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// `POST /v1/messages` (Anthropic): reenvío directo al motor nativo
/// (auditoría: la traducción perdía los bloques de thinking y dejaba
/// `content` vacío). Auth/clave ya validadas arriba.
fn handle_anthropic_messages(
    mut req: tiny_http::Request,
    mgr: &Arc<ProcessManager>,
    cfg: &Arc<ConfigStore>,
    origin: Option<String>,
) {
    // P1 producción: cap 25 MB anti-OOM (413 si excede).
    let body_bytes = match read_capped_bytes(&mut req.as_reader(), MAX_V1_BODY) {
        Ok(b) => b,
        Err((code, msg)) => {
            let _ = req.respond(json_response_for_origin(
                code,
                serde_json::json!({ "error": msg }).to_string(),
                origin.as_deref(),
            ));
            return;
        }
    };
    mgr.touch_activity();

    let st = mgr.get_status();
    // Fase A6: ver chat (el 8080 placeholder no es motor).
    if !crate::agents::engine_reachable(&st) {
        let (code, text) = anthropic_error(502, "api_error", "Motor apagado (engine_down)");
        let _ = req.respond(json_response_for_origin(code, text, origin.as_deref()));
        return;
    }
    let model_for_log = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| served_model_id(&st, cfg));
    let t0 = Instant::now();
    match ureq::post(&format!("http://127.0.0.1:{}/v1/messages", st.port))
        .set("Content-Type", "application/json")
        // El motor exige `--api-key` (D-45): sin la cabecera responde 401.
        .set(
            "Authorization",
            &crate::auth::bearer(crate::auth::gateway_key()),
        )
        .send_bytes(&body_bytes)
    {
        Ok(resp) => {
            let status = resp.status();
            let ct = resp
                .header("content-type")
                .unwrap_or("application/json")
                .to_string();
            if status == 200 && ct.contains("text/event-stream") {
                // Streaming directo: cada chunk al cliente en cuanto llega.
                let reader = NativeUsageTee::new(
                    resp.into_reader(),
                    "messages".to_string(),
                    model_for_log,
                    t0,
                );
                let sse_ct =
                    Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
                let mut hdrs = vec![sse_ct];
                if let Some(o) = cors_origin_header(origin.as_deref()) {
                    hdrs.push(o);
                }
                hdrs.extend(cors_fixed_headers());
                let r = Response::new(StatusCode(200), hdrs, reader, None, None);
                let _ = req.respond(r);
            } else {
                let mut raw = String::new();
                let _ = resp.into_reader().read_to_string(&mut raw);
                let (p, c) = extract_native_usage(&raw);
                crate::usage::log_usage(
                    "messages",
                    &model_for_log,
                    p,
                    c,
                    t0.elapsed().as_millis() as u64,
                    false,
                );
                let body_out = if raw.is_empty() {
                    serde_json::json!({ "error": format!("el motor respondió {}", status) })
                        .to_string()
                } else {
                    raw
                };
                let _ = req.respond(json_response_for_origin(
                    status,
                    body_out,
                    origin.as_deref(),
                ));
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            // El motor ya habla Anthropic: el error viene con forma, tal cual.
            let mut body = String::new();
            let _ = resp.into_reader().read_to_string(&mut body);
            let body_out = if body.is_empty() {
                serde_json::json!({ "error": format!("el motor respondió {}", code) }).to_string()
            } else {
                body
            };
            let _ = req.respond(json_response_for_origin(code, body_out, origin.as_deref()));
        }
        Err(_) => {
            let _ = req.respond(json_response_for_origin(
                502,
                r#"{"error":"Error al contactar motor"}"#.into(),
                origin.as_deref(),
            ));
        }
    }
}

/// `POST /v1/responses` (Responses/Codex): reenvío directo al motor nativo
/// (auditoría: la traducción dejaba `output` vacío cuando el modelo pensaba).
fn handle_responses(
    mut req: tiny_http::Request,
    mgr: &Arc<ProcessManager>,
    cfg: &Arc<ConfigStore>,
    origin: Option<String>,
) {
    // P1 producción: cap 25 MB anti-OOM (413 si excede).
    let body_bytes = match read_capped_bytes(&mut req.as_reader(), MAX_V1_BODY) {
        Ok(b) => b,
        Err((code, msg)) => {
            let _ = req.respond(json_response_for_origin(
                code,
                serde_json::json!({ "error": msg }).to_string(),
                origin.as_deref(),
            ));
            return;
        }
    };
    mgr.touch_activity();

    let st = mgr.get_status();
    // Fase A6: ver chat (el 8080 placeholder no es motor).
    if !crate::agents::engine_reachable(&st) {
        let _ = req.respond(json_response_for_origin(
            502,
            r#"{"error":"engine_down"}"#.into(),
            origin.as_deref(),
        ));
        return;
    }
    let model_for_log = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| served_model_id(&st, cfg));
    let t0 = Instant::now();
    match ureq::post(&format!("http://127.0.0.1:{}/v1/responses", st.port))
        .set("Content-Type", "application/json")
        // El motor exige `--api-key` (D-45): sin la cabecera responde 401.
        .set(
            "Authorization",
            &crate::auth::bearer(crate::auth::gateway_key()),
        )
        .send_bytes(&body_bytes)
    {
        Ok(resp) => {
            let status = resp.status();
            let ct = resp
                .header("content-type")
                .unwrap_or("application/json")
                .to_string();
            if status == 200 && ct.contains("text/event-stream") {
                // Streaming directo: cada chunk al cliente en cuanto llega.
                let reader = NativeUsageTee::new(
                    resp.into_reader(),
                    "responses".to_string(),
                    model_for_log,
                    t0,
                );
                let sse_ct =
                    Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
                let mut hdrs = vec![sse_ct];
                if let Some(o) = cors_origin_header(origin.as_deref()) {
                    hdrs.push(o);
                }
                hdrs.extend(cors_fixed_headers());
                let r = Response::new(StatusCode(200), hdrs, reader, None, None);
                let _ = req.respond(r);
            } else {
                let mut raw = String::new();
                let _ = resp.into_reader().read_to_string(&mut raw);
                let (p, c) = extract_native_usage(&raw);
                crate::usage::log_usage(
                    "responses",
                    &model_for_log,
                    p,
                    c,
                    t0.elapsed().as_millis() as u64,
                    false,
                );
                let body_out = if raw.is_empty() {
                    serde_json::json!({ "error": format!("el motor respondió {}", status) })
                        .to_string()
                } else {
                    raw
                };
                let _ = req.respond(json_response_for_origin(
                    status,
                    body_out,
                    origin.as_deref(),
                ));
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            // El motor ya habla Responses: el error viene con forma, tal cual.
            let mut body = String::new();
            let _ = resp.into_reader().read_to_string(&mut body);
            let body_out = if body.is_empty() {
                serde_json::json!({ "error": format!("el motor respondió {}", code) }).to_string()
            } else {
                body
            };
            let _ = req.respond(json_response_for_origin(code, body_out, origin.as_deref()));
        }
        Err(_) => {
            let _ = req.respond(json_response_for_origin(
                502,
                r#"{"error":"Error al contactar motor"}"#.into(),
                origin.as_deref(),
            ));
        }
    }
}
fn sse_headers() -> Vec<Header> {
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
    let cc = Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).unwrap();
    let cn = Header::from_bytes(&b"Connection"[..], &b"keep-alive"[..]).unwrap();
    let mut hs = vec![ct, cc, cn];
    hs.extend(cors_fixed_headers());
    hs
}

/// Lectores de proxy incremental (LM-PXY-1): envuelven el reader del motor y
/// ceden cada chunk al cliente en cuanto llega, sin acumular el body entero.
/// Al llegar a EOF registran la línea de uso desde una cola acotada (64 KiB).
mod proxy {
    use super::*;

    /// Tee pasante para `POST /v1/chat/completions` en streaming: reenvía los
    /// bytes tal cual y guarda solo la cola para el usage final.
    pub struct TeeLogReader<R: Read + Send> {
        inner: R,
        buf: [u8; 8192],
        out: Vec<u8>,
        tail: Vec<u8>,
        done: bool,
        logged: bool,
        endpoint: String,
        model: String,
        t0: Instant,
    }
    impl<R: Read + Send> TeeLogReader<R> {
        pub fn new(inner: R, endpoint: String, model: String, t0: Instant) -> Self {
            Self {
                inner,
                buf: [0u8; 8192],
                out: Vec::new(),
                tail: Vec::new(),
                done: false,
                logged: false,
                endpoint,
                model,
                t0,
            }
        }

        fn log_once(&mut self) {
            if self.logged {
                return;
            }
            self.logged = true;
            let text = String::from_utf8_lossy(&self.tail).to_string();
            let (p, c) = crate::usage::extract_usage_from_sse(&text);
            crate::usage::log_usage(
                &self.endpoint,
                &self.model,
                p,
                c,
                self.t0.elapsed().as_millis() as u64,
                true,
            );
        }
    }

    impl<R: Read + Send> Read for TeeLogReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                if !self.out.is_empty() {
                    let n = self.out.len().min(buf.len());
                    buf[..n].copy_from_slice(&self.out[..n]);
                    self.out.drain(..n);
                    return Ok(n);
                }
                if self.done {
                    self.log_once();
                    return Ok(0);
                }
                match self.inner.read(&mut self.buf) {
                    Ok(0) => {
                        self.done = true;
                    }
                    Ok(n) => {
                        crate::usage::push_tail(&mut self.tail, &self.buf[..n]);
                        self.out.extend_from_slice(&self.buf[..n]);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Read;
        use std::sync::{Arc, Mutex};

        /// Upstream que cede la respuesta en 3 trozos y anota cuándo se le
        /// pide cada uno (para probar que el cliente recibe el trozo 1 antes
        /// de que se pida el trozo 2: incrementalidad real, sin timing).
        struct ChunkedUpstream {
            chunks: Vec<Vec<u8>>,
            next: usize,
            log: Arc<Mutex<Vec<usize>>>,
        }

        impl Read for ChunkedUpstream {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.next >= self.chunks.len() {
                    return Ok(0);
                }
                let i = self.next;
                self.log.lock().map(|mut g| g.push(100 + i)).unwrap_or(());
                let n = self.chunks[i].len().min(buf.len());
                buf[..n].copy_from_slice(&self.chunks[i][..n]);
                if n == self.chunks[i].len() {
                    self.next += 1;
                } else {
                    let rest = self.chunks[i][n..].to_vec();
                    self.chunks[i] = rest;
                }
                Ok(n)
            }
        }

        #[test]
        fn tee_incremental_y_cola_acotada() {
            // Higiene (auditoría): el tee registra en `usage.jsonl`; fijar un
            // scratch para no contaminar el registro real del dueño.
            let scratch =
                std::env::temp_dir().join(format!("lm-test-usage-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&scratch);
            unsafe { std::env::set_var("LOCALMIND_USAGE_PATH", scratch.join("usage.jsonl")) };
            let c1 = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hola\"}}]}\n\n".to_vec();
            let c2 = b"data: {\"choices\":[{\"delta\":{\"content\":\" mundo\"}}]}\n\n".to_vec();
            let c3 = b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n".to_vec();
            let log = Arc::new(Mutex::new(Vec::new()));
            let upstream = ChunkedUpstream {
                chunks: vec![c1.clone(), c2.clone(), c3.clone()],
                next: 0,
                log: Arc::clone(&log),
            };
            let mut tee = TeeLogReader::new(
                upstream,
                "test.tee".to_string(),
                "test-model".to_string(),
                Instant::now(),
            );
            // Leer solo el primer trozo: el cliente lo tiene sin esperar al resto.
            let mut first = vec![0u8; c1.len()];
            let mut got = 0;
            while got < c1.len() {
                let n = tee.read(&mut first[got..]).unwrap();
                assert!(n > 0, "el tee debe ceder el trozo 1 de inmediato");
                got += n;
            }
            assert_eq!(&first, &c1);
            // Y el upstream solo fue pedido hasta el trozo 1 (índice 0).
            let seen: Vec<usize> = log.lock().map(|g| g.clone()).unwrap_or_default();
            assert!(
                seen.iter().all(|&x| x <= 100),
                "trozo 2 pedido antes de entregar el 1: {:?}",
                seen
            );
            // Drenar el resto: tail acotada y usage visible al final.
            let mut rest = Vec::new();
            let mut buf = [0u8; 512];
            loop {
                let n = tee.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                rest.extend_from_slice(&buf[..n]);
            }
            assert_eq!(
                &rest,
                &(c2.into_iter()
                    .chain(c3.clone().into_iter())
                    .collect::<Vec<u8>>())
            );
            assert!(tee.tail.len() <= crate::usage::TAIL_MAX_BYTES);
            let text = String::from_utf8_lossy(&tee.tail).to_string();
            assert_eq!(
                crate::usage::extract_usage_from_sse(&text),
                (Some(5), Some(7))
            );
            // Limpieza: quitar el override y el scratch (no filtrar al resto).
            unsafe { std::env::remove_var("LOCALMIND_USAGE_PATH") };
        }
        #[test]
        fn favicon_faltante_es_404() {
            // Decisión del handler vía load_asset: nombre inexistente → None → 404.
            let dir = std::path::PathBuf::from("dir-que-no-existe-12345");
            assert!(super::super::load_asset(&dir, "localmind.ico").is_none());
            assert!(super::super::load_asset(&dir, "localmind.png").is_none());
            // Y un archivo real sí carga (el propio server.rs como testigo).
            let here = std::path::PathBuf::from("src");
            assert!(super::super::load_asset(&here, "server.rs").is_some());
        }
        #[test]
        fn iconos_omni_publicos_con_tipo_correcto() {
            // Fase C: las cuatro rutas son publicas y cada una mapea a su
            // fichero y MIME (con y sin query); cualquier otra cosa no es icono.
            assert!(super::super::is_public_path("GET", "/omni.ico"));
            assert!(super::super::is_public_path("GET", "/omni.png"));
            assert!(super::super::is_public_path("GET", "/localmind.ico"));
            assert!(super::super::is_public_path("GET", "/omni.ico?x=1"));
            assert!(!super::super::is_public_path("POST", "/omni.ico"));
            assert_eq!(
                super::super::icon_asset_for("/omni.ico"),
                Some(("omni.ico", b"image/x-icon".as_slice()))
            );
            assert_eq!(
                super::super::icon_asset_for("/omni.png"),
                Some(("omni.png", b"image/png".as_slice()))
            );
            assert!(super::super::icon_asset_for("/otro.png").is_none());
        }
        #[test]
        fn puerta_auth_protegidas_401() {
            // Puerta D-7 (`puerta_auth_exige_clave` + `is_authorized`): las
            // rutas públicas y el login no exigen clave; todo lo demás sí —
            // incluido `GET /api/version` (decisión documentada en el
            // docstring de `handle_request`: antes era pública de facto).
            // Sin la puerta, una ruta protegida sin credencial llegaba al
            // handler y respondía 200: este test falla en ese código.
            use tiny_http::Header;
            let exige = super::super::puerta_auth_exige_clave;
            // Públicas: no exigen.
            assert!(!exige("GET", "/"));
            assert!(!exige("GET", "/index.html"));
            assert!(!exige("GET", "/omni.ico"));
            assert!(!exige("OPTIONS", "/api/status"));
            // Login: no exige (es quien ENTREGA la credencial).
            assert!(!exige("POST", "/api/unlock"));
            // Protegidas: exigen (api, versiones, v1, 404s no públicas).
            assert!(exige("GET", "/api/status"));
            assert!(exige("GET", "/api/version"));
            assert!(exige("GET", "/api/models"));
            assert!(exige("GET", "/v1/models"));
            assert!(exige("POST", "/api/start"));
            assert!(exige("GET", "/definitely-not-a-route"));
            // Y sin credencial `is_authorized` es falso → el handler
            // respondería 401 (`unauthorized_json_for_origin`).
            let clave = "clave-de-prueba-puerta-d7";
            assert!(!crate::auth::is_authorized(&[], clave));
            let mala = [Header::from_bytes(b"Authorization", b"Bearer otra").unwrap()];
            assert!(!crate::auth::is_authorized(&mala, clave));
            // Con credencial válida sí pasa: Bearer, x-api-key y cookie.
            let bearer =
                [
                    Header::from_bytes(b"Authorization", format!("Bearer {}", clave).as_bytes())
                        .unwrap(),
                ];
            assert!(crate::auth::is_authorized(&bearer, clave));
            let api_key = [Header::from_bytes(b"x-api-key", clave.as_bytes()).unwrap()];
            assert!(crate::auth::is_authorized(&api_key, clave));
            let cookie =
                [Header::from_bytes(b"Cookie", format!("lm_key={}", clave).as_bytes()).unwrap()];
            assert!(crate::auth::is_authorized(&cookie, clave));
            // El 401 lleva el cuerpo pactado.
            let resp = super::super::unauthorized_json_for_origin(None);
            assert_eq!(resp.status_code().0, 401);
        }
        #[test]
        fn native_usage_lee_ambos_dialectos_y_toma_el_ultimo() {
            // Anthropic no-streaming y message_start/message_delta.
            let j = r#"{"id":"m","usage":{"input_tokens":13,"output_tokens":30}}"#;
            assert_eq!(super::super::extract_native_usage(j), (Some(13), Some(30)));
            let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":14,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\ndata: [DONE]\n";
            assert_eq!(
                super::super::extract_native_usage(sse),
                (Some(14), Some(20))
            );
            // Responses con usage anidado en response.*.
            let r = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":55,\"output_tokens\":30,\"total_tokens\":85}}}\n";
            assert_eq!(super::super::extract_native_usage(r), (Some(55), Some(30)));
            // Basura y vacíos no inventan tokens.
            assert_eq!(
                super::super::extract_native_usage("no-json\n[DONE]\n"),
                (None, None)
            );
            assert_eq!(super::super::extract_native_usage(""), (None, None));
        }
        #[test]
        fn native_errores_con_forma_de_dialecto() {
            let (c, t) =
                super::super::anthropic_error(502, "api_error", "Motor apagado (engine_down)");
            assert_eq!(c, 502);
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            assert_eq!(v["error"]["type"], serde_json::json!("api_error"));
            let (c, t) = super::super::responses_error(502, "remoto no alcanzado");
            assert_eq!(c, 502);
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            assert_eq!(v["error"]["type"], serde_json::json!("api_error"));
        }
        #[test]
        fn prepare_chat_payload_normaliza_modelo_stream_y_effort() {
            // Único parseo del proxy chat: modelo reescrito, stream_options
            // inyectado solo si falta, effort mapeado o eliminado.
            let prep = |body: &[u8], served: &str| super::super::prepare_chat_payload(body, served);
            // Stream sin options -> se inyecta.
            let (out, model, stream) = prep(br#"{"model":"m","stream":true,"messages":[]}"#, "S");
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(
                v["stream_options"],
                serde_json::json!({"include_usage": true})
            );
            assert_eq!((model, stream), ("m".to_string(), true));
            // Stream con options objeto -> se respeta.
            let (out, _, _) = prep(br#"{"model":"m","stream":true,"stream_options":{}}"#, "S");
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert!(v["stream_options"].is_object());
            // Sin stream -> sin options.
            let (out, _, _) = prep(br#"{"model":"m"}"#, "S");
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert!(v.get("stream_options").is_none());
            // Prompt grande con todo: rewrite + usage + effort juntos.
            let big = format!(
                r#"{{"model":"viejo","stream":true,"reasoning_effort":"HIGH","messages":[{{"role":"user","content":"{}"}}]}}"#,
                "x".repeat(200_000)
            );
            let (out, model, stream) = prep(big.as_bytes(), "SERVIDO");
            assert_eq!((model.as_str(), stream), ("viejo", true));
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(v["model"], serde_json::json!("SERVIDO"));
            assert_eq!(v["reasoning_effort"], serde_json::json!("xhigh"));
            assert_eq!(
                v["stream_options"],
                serde_json::json!({"include_usage": true})
            );
            // No-objeto y no-JSON pasan intactos.
            assert_eq!(prep(b"[1,2]", "S").0, b"[1,2]".to_vec());
            assert_eq!(prep(b"no-json", "S").0, b"no-json".to_vec());
            assert_eq!(prep(b"", "S").0, b"".to_vec());
        }
        #[test]
        fn reasoning_effort_max_a_xhigh_y_desconocido_se_elimina() {
            // Misma normalización que aplica el proxy `/v1/chat/completions`
            // (`prepare_chat_payload`): `/v1/messages` y `/v1/responses` van
            // nativos al motor sin tocar.
            let get = |body: &str| {
                let (out, _, _) = super::super::prepare_chat_payload(body.as_bytes(), "m");
                let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
                v.get("reasoning_effort").cloned()
            };
            // `max` → `xhigh` (era el 500 de la plantilla Qwen).
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"max"}"#),
                Some(serde_json::json!("xhigh"))
            );
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"MAX"}"#),
                Some(serde_json::json!("xhigh"))
            );
            // `off`/`none` desactivan el thinking (soportado por el servidor).
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"off"}"#),
                Some(serde_json::json!("none"))
            );
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"none"}"#),
                Some(serde_json::json!("none"))
            );
            // Comportamiento previo intacto.
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"minimal"}"#),
                Some(serde_json::json!("low"))
            );
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"high"}"#),
                Some(serde_json::json!("xhigh"))
            );
            // Válidos intactos (normalizados a minúsculas).
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"low"}"#),
                Some(serde_json::json!("low"))
            );
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"medium"}"#),
                Some(serde_json::json!("medium"))
            );
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"xhigh"}"#),
                Some(serde_json::json!("xhigh"))
            );
            assert_eq!(
                get(r#"{"model":"m","reasoning_effort":"Medium"}"#),
                Some(serde_json::json!("medium"))
            );
            // Desconocidos y no-strings: campo eliminado, el motor usa su
            // default (el caso `reasoning_effort:null` de pi `--thinking max`).
            let dropped = |body: &str| {
                let (out, _, _) = super::super::prepare_chat_payload(body.as_bytes(), "m");
                let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
                assert!(
                    v.get("reasoning_effort").is_none(),
                    "debió eliminarse: {:?}",
                    String::from_utf8_lossy(&out)
                );
            };
            dropped(r#"{"model":"m","reasoning_effort":"ultra"}"#);
            dropped(r#"{"model":"m","reasoning_effort":""}"#);
            dropped(r#"{"model":"m","reasoning_effort":42}"#);
            dropped(r#"{"model":"m","reasoning_effort":null}"#);
            // Sin campo → `medium` (default Qwen alineado), resto intacto.
            let (out, _, _) =
                super::super::prepare_chat_payload(br#"{"model":"m","temperature":0.7}"#, "m");
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(v["reasoning_effort"], serde_json::json!("medium"));
            assert_eq!(v["temperature"], serde_json::json!(0.7));
            // No-JSON pasa intacto.
            assert_eq!(
                super::super::prepare_chat_payload(b"no-json", "m").0,
                b"no-json".to_vec()
            );
        }

        #[test]
        fn qwen_sampler_se_rellena_solo_en_claves_ausentes() {
            // Sampler oficial Qwen3.8 según modo; lo explícito siempre gana.
            let get = |body: &[u8]| {
                let (out, _, _) = super::super::prepare_chat_payload(body, "m");
                serde_json::from_slice::<serde_json::Value>(&out).unwrap()
            };
            // Thinking (`medium` inyectado): temp 1.0 / top_k 20 / top_p 0.95 /
            // min_p 0.0 / presence 0.0.
            let v = get(br#"{"model":"m","messages":[]}"#);
            assert_eq!(v["reasoning_effort"], serde_json::json!("medium"));
            assert_eq!(v["temperature"], serde_json::json!(1.0));
            assert_eq!(v["top_k"], serde_json::json!(20));
            assert_eq!(v["top_p"], serde_json::json!(0.95));
            assert_eq!(v["min_p"], serde_json::json!(0.0));
            assert_eq!(v["presence_penalty"], serde_json::json!(0.0));
            // Explícito intacto: la puerta de aceptación (temp 0) y los benches
            // no se tocan.
            let v = get(br#"{"model":"m","temperature":0,"top_k":40,"messages":[]}"#);
            assert_eq!(v["temperature"], serde_json::json!(0));
            assert_eq!(v["top_k"], serde_json::json!(40));
            assert_eq!(v["top_p"], serde_json::json!(0.95));
            // Sin thinking (`none`): sampler instruct 0.7 / 20 / 0.80 / 0.0 / 1.5.
            let v = get(br#"{"model":"m","reasoning_effort":"off","messages":[]}"#);
            assert_eq!(v["reasoning_effort"], serde_json::json!("none"));
            assert_eq!(v["temperature"], serde_json::json!(0.7));
            assert_eq!(v["top_p"], serde_json::json!(0.80));
            assert_eq!(v["presence_penalty"], serde_json::json!(1.5));
            // `enable_thinking:false` explícito también rige instruct.
            let v = get(
                br#"{"model":"m","chat_template_kwargs":{"enable_thinking":false},"messages":[]}"#,
            );
            assert_eq!(v["temperature"], serde_json::json!(0.7));
            assert_eq!(v["presence_penalty"], serde_json::json!(1.5));
        }

        #[test]
        fn sampler_por_familia_qwen_llama_mistral_generico() {
            use super::super::{model_family, sampler_defaults, ModelFamily};
            assert_eq!(model_family("Qwen3.8-27B-IQ4_XS_4BPW"), ModelFamily::Qwen);
            assert_eq!(
                model_family("Ternary-Bonsai-2-27B-PTQ1_0"),
                ModelFamily::Qwen
            );
            assert_eq!(model_family("Llama-3.1-8B-Q4_K_M"), ModelFamily::Llama);
            assert_eq!(model_family("Mistral-7B-v0.3-Q4_K_M"), ModelFamily::Mistral);
            assert_eq!(model_family("Mixtral-8x7B-Q4"), ModelFamily::Mistral);
            assert_eq!(model_family("Phi-4-14B-Q4"), ModelFamily::Generic);
            assert_eq!(model_family(""), ModelFamily::Generic);
            // Qwen/Generic = card medida; Llama/Mistral = sus guías.
            let get = |fam, th: bool| {
                let m: std::collections::HashMap<&str, serde_json::Value> =
                    sampler_defaults(fam, th).into_iter().collect();
                (
                    m["temperature"].clone(),
                    m["top_k"].clone(),
                    m["top_p"].clone(),
                )
            };
            assert_eq!(
                get(ModelFamily::Qwen, true),
                (
                    serde_json::json!(1.0),
                    serde_json::json!(20),
                    serde_json::json!(0.95)
                )
            );
            assert_eq!(
                get(ModelFamily::Llama, false),
                (
                    serde_json::json!(0.6),
                    serde_json::json!(40),
                    serde_json::json!(0.9)
                )
            );
            assert_eq!(
                get(ModelFamily::Mistral, false),
                (
                    serde_json::json!(0.7),
                    serde_json::json!(40),
                    serde_json::json!(1.0)
                )
            );
            // Payload con servido Llama: el sampler es Llama (0.6), no Qwen.
            let (out, _, _) = super::super::prepare_chat_payload(
                br#"{"model":"m","messages":[]}"#,
                "Llama-3.1-8B-Q4_K_M",
            );
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(v["temperature"], serde_json::json!(0.6));
            assert_eq!(v["top_k"], serde_json::json!(40));
        }

        #[test]
        fn effort_mismatch_solo_qwen_razona() {
            use super::super::effort_mismatch;
            // Qwen/Bonsai con effort = sin mismatch (razonan).
            assert!(!effort_mismatch("Qwen3.8-27B", Some("max")));
            assert!(!effort_mismatch(
                "Ternary-Bonsai-2-27B-PTQ1_0",
                Some("medium")
            ));
            // Llama/Mistral/desconocido con effort = mismatch (responden directo).
            assert!(effort_mismatch("Llama-3.1-8B", Some("high")));
            assert!(effort_mismatch("Mistral-7B", Some("medium")));
            assert!(effort_mismatch("Phi-4-14B", Some("low")));
            // off/none/ausente = sin pedido = sin mismatch en nadie.
            assert!(!effort_mismatch("Llama-3.1-8B", Some("off")));
            assert!(!effort_mismatch("Llama-3.1-8B", Some("none")));
            assert!(!effort_mismatch("Llama-3.1-8B", None));
            assert!(!effort_mismatch("Qwen3.8-27B", None));
        }
        #[test]
        fn config_runtime_fingerprint_cubre_flags_sin_exponer_sus_valores() {
            let dir = std::env::temp_dir()
                .join(format!("lm-test-engine-fingerprint-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let store = crate::config::ConfigStore::load_from_path(&dir.join("localmind.toml"));
            let base_json = super::super::app_config_json(&store.get());
            let base: serde_json::Value = serde_json::from_str(&base_json).unwrap();
            let base_hash = base["engine"]["runtime_fingerprint_sha256"]
                .as_str()
                .unwrap();
            assert_eq!(base_hash.len(), 64);
            assert_eq!(base["engine"]["extra_flags_count"], serde_json::json!(0));

            let mut changed = store.get();
            changed
                .engine
                .extra_flags
                .push("--private-test-value".to_string());
            let changed_json = super::super::app_config_json(&changed);
            let changed_value: serde_json::Value = serde_json::from_str(&changed_json).unwrap();
            let changed_hash = changed_value["engine"]["runtime_fingerprint_sha256"]
                .as_str()
                .unwrap();
            assert_ne!(base_hash, changed_hash);
            assert_eq!(
                changed_value["engine"]["extra_flags_count"],
                serde_json::json!(1)
            );
            assert!(!changed_json.contains("--private-test-value"));

            let mut changed_profile = store.get();
            changed_profile.profiles[0].context += 1024;
            changed_profile.profiles[0]
                .extra_flags
                .push("--profile-private-value".to_string());
            let profile_json = super::super::app_config_json(&changed_profile);
            let profile_value: serde_json::Value = serde_json::from_str(&profile_json).unwrap();
            assert_ne!(
                profile_value["engine"]["runtime_fingerprint_sha256"],
                base["engine"]["runtime_fingerprint_sha256"]
            );
            assert!(!profile_json.contains("--profile-private-value"));

            let mut changed_argv = store.get();
            changed_argv.engine.metrics = !changed_argv.engine.metrics;
            changed_argv.mmproj.auto = !changed_argv.mmproj.auto;
            let argv_json = super::super::app_config_json(&changed_argv);
            let argv_value: serde_json::Value = serde_json::from_str(&argv_json).unwrap();
            assert_ne!(
                argv_value["engine"]["runtime_fingerprint_sha256"],
                base["engine"]["runtime_fingerprint_sha256"]
            );

            let mut reordered_profiles = store.get();
            reordered_profiles.profiles.reverse();
            let reordered_json = super::super::app_config_json(&reordered_profiles);
            let reordered_value: serde_json::Value = serde_json::from_str(&reordered_json).unwrap();
            assert_eq!(
                reordered_value["engine"]["runtime_fingerprint_sha256"],
                base["engine"]["runtime_fingerprint_sha256"]
            );
        }

        #[test]
        fn config_speculation_enabled_persiste_y_subclave_desconocida_se_rechaza() {
            // `POST /api/config {"engine":{"speculation":{"enabled":false}}}`:
            // misma lógica de validación del handler, probada sin socket:
            // persiste `false` y expone `speculation_enabled` en el GET.
            let dir = std::env::temp_dir().join(format!("lm-test-speccfg-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("localmind.toml");
            let store = crate::config::ConfigStore::load_from_path(&path);
            // Estado inicial: `speculation` ausente => GET expone `false`.
            let v0: serde_json::Value =
                serde_json::from_str(&super::super::app_config_json(&store.get())).unwrap();
            assert_eq!(
                v0["engine"]["speculation_enabled"],
                serde_json::json!(false)
            );
            // Aplicar `{"enabled":false}` con la regla real del gateway
            // (`config::apply_config_patch`, Fase A4): solo se acepta la
            // sub-clave `enabled` y debe ser booleano.
            let apply = |store: &crate::config::ConfigStore, body: &str| -> Result<bool, String> {
                let val: serde_json::Value =
                    serde_json::from_str(body).map_err(|e| e.to_string())?;
                let next = crate::config::apply_config_patch(&store.get(), &val)?;
                let en = next.engine.speculation.as_ref().is_some_and(|s| s.enabled);
                store.update(|c| *c = next);
                store.save()?;
                Ok(en)
            };
            assert!(!apply(&store, r#"{"engine":{"speculation":{"enabled":false}}}"#).unwrap());
            assert_eq!(
                store.get().engine.speculation.as_ref().map(|s| s.enabled),
                Some(false)
            );
            let v1: serde_json::Value =
                serde_json::from_str(&super::super::app_config_json(&store.get())).unwrap();
            assert_eq!(
                v1["engine"]["speculation_enabled"],
                serde_json::json!(false)
            );
            // Recarga desde disco: persiste.
            let store2 = crate::config::ConfigStore::load_from_path(&path);
            assert_eq!(
                store2.get().engine.speculation.as_ref().map(|s| s.enabled),
                Some(false)
            );
            // Sub-clave desconocida => 400 con el mensaje del handler.
            let err = apply(
                &store,
                r#"{"engine":{"speculation":{"enabled":true,"n":8}}}"#,
            )
            .unwrap_err();
            assert!(
                err.contains("engine.speculation.n") && err.contains("solo se acepta enabled"),
                "{}",
                err
            );
            // `enabled` no booleano => 400.
            let err2 = apply(&store, r#"{"engine":{"speculation":{"enabled":"si"}}}"#).unwrap_err();
            assert!(err2.contains("engine.speculation.enabled"), "{}", err2);
            let _ = std::fs::remove_dir_all(&dir);
        }

        // ---- Fase B3b: target remoto y errores con forma de dialecto ----

        fn cfg_con_remoto(url: &str) -> crate::config::AppConfig {
            let mut c = crate::config::AppConfig::default();
            c.remote.url = url.to_string();
            c.remote.key = "k".to_string();
            c
        }

        #[test]
        fn remoto_target_normaliza_v1_y_rechaza_malo() {
            // Con y sin `/v1` → misma raíz; vacía o mala → None.
            assert_eq!(
                super::super::remote_target(&cfg_con_remoto("http://192.168.1.10:17860/v1")),
                Some(("http://192.168.1.10:17860".to_string(), "k".to_string()))
            );
            assert_eq!(
                super::super::remote_target(&cfg_con_remoto("http://192.168.1.10:17860/")),
                Some(("http://192.168.1.10:17860".to_string(), "k".to_string()))
            );
            assert!(super::super::remote_target(&cfg_con_remoto("")).is_none());
            assert!(super::super::remote_target(&cfg_con_remoto("http://8.8.8.8/")).is_none());
        }

        #[test]
        fn remoto_error_transporte_con_forma_de_dialecto() {
            // Chat/models: OpenAI plano; messages: Anthropic; responses: Responses.
            let (c, t) = super::super::remote_transport_error("/v1/chat/completions");
            assert_eq!(c, 502);
            assert!(t.contains("Error al contactar remoto"), "{}", t);
            let (c, t) = super::super::remote_transport_error("/v1/models");
            assert_eq!(c, 502);
            assert!(t.contains("remoto"), "{}", t);
            let (c, t) = super::super::remote_transport_error("/v1/messages");
            assert_eq!(c, 502);
            assert!(t.contains("remote_unreachable"), "{}", t);
            let (c, t) = super::super::remote_transport_error("/v1/responses");
            assert_eq!(c, 502);
            assert!(t.contains("remoto"), "{}", t);
        }

        // ---- LM-MOD-3: el id anunciado sigue al motor, D-2 intacto ----

        fn store_de_prueba(dir: &std::path::Path) -> crate::config::ConfigStore {
            let _ = std::fs::create_dir_all(dir);
            crate::config::ConfigStore::load_from_path(&dir.join("localmind.toml"))
        }

        /// El id que anuncia `/v1/models` y al que el proxy reescribe ES el
        /// modelo realmente servido, y cambia con él.
        ///
        /// Falla sobre el código original solo en la parte de "cambia con él":
        /// allí el literal `qwen3.8-27b` convivía con el filename, así que la
        /// lista podía anunciar los dos. Aquí se exige una sola verdad.
        #[test]
        fn v1_models_anuncia_el_servido_y_no_un_nombre_fijo() {
            let dir = std::env::temp_dir().join(format!("lm-test-v1m-{}", std::process::id()));
            let cfg = store_de_prueba(&dir);
            let st = |model: &str, port: u16| ServerStatus {
                status: "running".to_string(),
                is_healthy: true,
                model: model.to_string(),
                port,
                ..Default::default()
            };

            let bonsai: serde_json::Value = serde_json::from_str(&models_list_json(
                &st("Ternary-Bonsai-2-27B-PTQ1_0.gguf", 8080),
                &cfg,
            ))
            .unwrap();
            let ids: Vec<String> = bonsai["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(
                ids[0], "Ternary-Bonsai-2-27B-PTQ1_0",
                "el id servido no encabeza la lista: {:?}",
                ids
            );
            // El alias de compat sigue publicado (agentes en la calle lo usan).
            assert!(
                ids.iter().any(|i| i == "localmind"),
                "se perdió el alias localmind: {:?}",
                ids
            );

            // Con Qwen cargado, la lista cambia: no es un literal con aliases.
            let qwen: serde_json::Value = serde_json::from_str(&models_list_json(
                &st("Qwen3.8-27B-IQ4_XS_4BPW.gguf", 8080),
                &cfg,
            ))
            .unwrap();
            let qwen_ids: Vec<String> = qwen["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(qwen_ids[0], "Qwen3.8-27B-IQ4_XS_4BPW", "{:?}", qwen_ids);
            assert_ne!(ids[0], qwen_ids[0]);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// D-2 intacto: un agente configurado con `localmind`, y otro con el
        /// nombre del modelo ANTERIOR, siguen llegando al motor con el id
        /// servido. Esto es la red de seguridad que hace seguro cambiar la
        /// etiqueta: se cambia lo que se ANUNCIA, no lo que se ACEPTA.
        #[test]
        fn un_alias_o_un_modelo_viejo_siempre_llega_al_servido() {
            let dir = std::env::temp_dir().join(format!("lm-test-rw-{}", std::process::id()));
            let cfg = store_de_prueba(&dir);
            let st = ServerStatus {
                status: "running".to_string(),
                is_healthy: true,
                model: "Ternary-Bonsai-2-27B-PTQ1_0.gguf".to_string(),
                port: 8080,
                ..Default::default()
            };
            let served = served_model_id(&st, &cfg);
            assert_eq!(served, "Ternary-Bonsai-2-27B-PTQ1_0");

            let get = |b: &[u8]| {
                let (out, _, _) = super::super::prepare_chat_payload(b, &served);
                serde_json::from_slice::<serde_json::Value>(&out).unwrap()
            };
            // Alias de compat.
            let v = get(br#"{"model":"localmind","messages":[]}"#);
            assert_eq!(v["model"], serde_json::json!("Ternary-Bonsai-2-27B-PTQ1_0"));
            // Con prefijo de provider (como lo escribe el lanzador).
            let v = get(br#"{"model":"localmind/localmind","messages":[]}"#);
            assert_eq!(v["model"], serde_json::json!("Ternary-Bonsai-2-27B-PTQ1_0"));
            // El modelo ANTERIOR: el caso de un config escrito antes del cambio.
            let v = get(br#"{"model":"qwen3.8-27b","messages":[]}"#);
            assert_eq!(v["model"], serde_json::json!("Ternary-Bonsai-2-27B-PTQ1_0"));
            // Un nombre inventado tampoco se rechaza (nunca se rechaza).
            let v = get(br#"{"model":"modelo-que-no-existe","messages":[]}"#);
            assert_eq!(v["model"], serde_json::json!("Ternary-Bonsai-2-27B-PTQ1_0"));
            // Lo explícito no se toca (temp 0 intacta); el proxy solo rellena
            // el sampler Qwen en claves ausentes + `reasoning_effort:medium`.
            let v = get(br#"{"model":"localmind","temperature":0,"stream":true}"#);
            assert_eq!(v["temperature"], serde_json::json!(0));
            assert_eq!(v["stream"], serde_json::json!(true));
            // Un body sin `model` NO inventa el campo `model`, pero sí recibe
            // los defaults Qwen (medium + sampler thinking) como cualquier otro.
            let (out, _, _) = super::super::prepare_chat_payload(br#"{"messages":[]}"#, &served);
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert!(v.get("model").is_none());
            assert_eq!(v["reasoning_effort"], serde_json::json!("medium"));
            assert_eq!(v["temperature"], serde_json::json!(1.0));
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Motor apagado: `/v1/models` SIGUE contestando (compat) y usa el
        /// alias estable, no el nombre del modelo de la sesión anterior.
        /// La etiqueta honesta de "no hay modelo" la pone la UI y el 409 del
        /// lanzador; aquí la red de seguridad nunca se queda sin responder.
        #[test]
        fn motor_apagado_responde_con_el_alias_y_no_con_el_viejo() {
            let dir = std::env::temp_dir().join(format!("lm-test-off-{}", std::process::id()));
            let cfg = store_de_prueba(&dir);
            // `ServerStatus` conserva el `model` de la sesión anterior al parar.
            let parado = ServerStatus {
                status: "stopped".to_string(),
                port: 0,
                ..Default::default()
            };
            let cuerpo = models_list_json(&parado, &cfg);
            let v: serde_json::Value = serde_json::from_str(&cuerpo).unwrap();
            let ids: Vec<String> = v["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect();
            assert!(
                !ids.is_empty(),
                "/v1/models dejó de contestar con el motor apagado"
            );
            assert!(
                ids.iter().any(|i| i == "localmind"),
                "sin alias de compat: {:?}",
                ids
            );
            // El lanzador, en cambio, NO escribe nada: eso ya lo hace el 409.
            assert!(!crate::agents::engine_live(&parado));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn header_parse_de_literal_no_pania() {
            // Los `Header::from_bytes(...).unwrap()` que quedan en el archivo
            // parsean LITERALES de byte: no pueden fallar, y por eso el `unwrap`
            // es aceptable ahí. Este test fija ese supuesto para cada literal
            // realmente usado: si una edición futura vuelve dinámico uno de
            // ellos, esto falla y obliga a la forma `Option` de
            // `cors_origin_header` / `session_cookie_header`.
            let literales: &[(&[u8], &[u8])] = &[
                (b"Content-Type", b"application/json"),
                (b"Content-Type", b"text/html; charset=utf-8"),
                (b"Content-Type", b"text/plain; charset=utf-8"),
                (b"Content-Type", b"text/event-stream"),
                (b"Content-Type", b"image/x-icon"),
                (b"Content-Type", b"image/png"),
                (
                    b"Content-Disposition",
                    b"attachment; filename=\"localmind_profiles.json\"",
                ),
                (b"Cache-Control", b"no-cache"),
                (b"Connection", b"keep-alive"),
                (
                    b"Access-Control-Allow-Methods",
                    b"GET, POST, OPTIONS, PUT, DELETE",
                ),
                (
                    b"Access-Control-Allow-Headers",
                    b"Content-Type, Authorization, x-api-key",
                ),
            ];
            for &(name, value) in literales {
                assert!(
                    Header::from_bytes(name, value).is_ok(),
                    "literal no parseable: {}: {}",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(value)
                );
            }
            // Y el valor DINÁMICO de la cookie ya no entra en panic: una clave
            // manipulada da `None` y el llamante responde 500.
            // Y el valor DINÁMICO de la cookie ya no entra en panic: `Option`
            // en vez de `unwrap`, así que una clave manipulada da `None` y el
            // llamante responde 500. Comprobado empíricamente: el parser de
            // `http` rechaza un byte NO-ASCII en el valor (un `gateway.key`
            // alterado o guardado en otra codificación), no un byte de control.
            assert!(super::super::session_cookie_header(&"a".repeat(64)).is_some());
            assert!(super::super::session_cookie_header("clave\u{80}").is_none());
        }

        #[test]
        fn api_start_rechaza_cuerpo_malformado() {
            // Un tipo equivocado NO puede degradarse a un arranque con
            // defaults (que devolvía 200 y encendía el motor).
            let err = super::super::parse_start_body(r#"{"context":"abc"}"#).unwrap_err();
            assert!(err.starts_with("JSON inválido"), "{}", err);
            // Tampoco un cuerpo que no es un objeto.
            let err2 = super::super::parse_start_body("[1,2,3]").unwrap_err();
            assert!(err2.starts_with("JSON inválido"), "{}", err2);
            // Cuerpo VACÍO: sigue siendo default (POST sin cuerpo, no rompe).
            let d = super::super::parse_start_body("").unwrap();
            assert!(d.model.is_none() && d.context.is_none() && d.priority.is_none());
            let ws = super::super::parse_start_body("   ").unwrap();
            assert!(ws.profile.is_none());
            // Bien formado: pasa intacto.
            let ok =
                super::super::parse_start_body(r#"{"model":"a.gguf","context":32768}"#).unwrap();
            assert_eq!(ok.model.as_deref(), Some("a.gguf"));
            assert_eq!(ok.context, Some(32768));
        }

        #[test]
        fn profiles_import_rechaza_flags_invalidas() {
            // El import no puede saltarse los validadores de /profiles/save.
            let perfil = |id: &str, ctx: u64, flags: &str| {
                format!(
                    r#"[{{"id":"{}","name":"N","description":"","context":{},"cache_ram":0,"extra_flags":{}}}]"#,
                    id, ctx, flags
                )
            };
            // Flag con inyección de shell (mismo charset que `profile_flag_ok`).
            let err = super::super::parse_profiles_import(&perfil(
                "velocidad",
                32768,
                r#"["-flag & calc.exe"]"#,
            ))
            .unwrap_err();
            assert!(err.contains("extra_flags"), "{}", err);
            // Contexto fuera de rango / no múltiplo de 1024.
            let err2 =
                super::super::parse_profiles_import(&perfil("velocidad", 1023, "[]")).unwrap_err();
            assert!(err2.contains("context"), "{}", err2);
            // Id inválido.
            let err3 = super::super::parse_profiles_import(&perfil("Perfil Mal", 32768, "[]"))
                .unwrap_err();
            assert!(err3.contains("id"), "{}", err3);
            // Lista vacía y JSON roto.
            assert!(super::super::parse_profiles_import("[]")
                .unwrap_err()
                .contains("vacía"));
            assert!(super::super::parse_profiles_import("{")
                .unwrap_err()
                .starts_with("JSON inválido"));
            // Perfil válido: pasa intacto.
            let ok =
                super::super::parse_profiles_import(&perfil("libros", 131072, r#"["--no-mmap"]"#))
                    .unwrap();
            assert_eq!(ok.len(), 1);
            assert_eq!(ok[0].context, 131072);
        }

        #[test]
        fn limit_body_acota_y_informa_bytes() {
            // Helper puro: exacto al límite pasa, 1 byte más falla con mensaje útil.
            assert!(super::super::limit_body(b"12345", 5).is_ok());
            assert!(super::super::limit_body(b"", 5).is_ok());
            let err = super::super::limit_body(b"123456", 5).unwrap_err();
            assert!(err.contains("cuerpo demasiado grande"), "{}", err);
            assert!(err.contains("6 bytes"), "{}", err);
            assert!(err.contains("5 bytes"), "{}", err);

            // Reader acotado: detecta exceso leyendo como máximo max + 1 bytes.
            let data = vec![b'a'; 100];
            let mut cur = std::io::Cursor::new(data);
            let ok = super::super::read_bounded_body(&mut cur, 100).unwrap();
            assert_eq!(ok.len(), 100);

            let data_overflow = vec![b'b'; 101];
            let mut cur_over = std::io::Cursor::new(data_overflow);
            let err_over = super::super::read_bounded_body(&mut cur_over, 100).unwrap_err();
            assert_eq!(err_over.0, 413);
            assert!(err_over.1.contains("demasiado grande"));

            // Cap /v1/* (P1 producción): bytes crudos hasta MAX_V1_BODY.
            let big = vec![b'x'; super::super::MAX_V1_BODY + 1];
            let mut cur_big = std::io::Cursor::new(big);
            let err_big = super::super::read_capped_bytes(&mut cur_big, super::super::MAX_V1_BODY)
                .unwrap_err();
            assert_eq!(err_big.0, 413);
        }

        /// Allowlist `POST /api/open_data_dir` (P1-7 producción): solo
        /// `appdata`/`temp` resuelven; `..`, rutas absolutas y resto → `None`.
        #[test]
        fn data_dir_allowlist_cerrada() {
            assert!(super::super::data_dir_for("appdata").is_some());
            assert!(super::super::data_dir_for("temp").is_some());
            assert!(super::super::data_dir_for("..").is_none());
            assert!(super::super::data_dir_for("C:/Windows").is_none());
            assert!(super::super::data_dir_for("").is_none());
            assert!(super::super::data_dir_for("models").is_none());
        }

        #[test]
        fn import_model_body_parse_y_overwrite_optin() {
            // Parsea path obligatorio y overwrite por defecto false.
            let p1 = super::super::parse_import_model_body(r#"{"path":"C:/m/a.gguf"}"#).unwrap();
            assert_eq!(p1.path, "C:/m/a.gguf");

            // overwrite true explícito.
            let p2 =
                super::super::parse_import_model_body(r#"{"path":"C:/m/a.gguf","overwrite":true}"#)
                    .unwrap();
            assert!(p2.overwrite);

            // Errores de validación: falta path o vacío.
            assert!(super::super::parse_import_model_body(r#"{"overwrite":true}"#).is_err());
            assert!(super::super::parse_import_model_body(r#"{"path":"   "}"#).is_err());
            assert!(super::super::parse_import_model_body(r#"[]"#).is_err());
        }

        #[test]
        fn profiles_import_rechaza_flags_de_escritura_peligrosas() {
            let perfil = |flags: &str| {
                format!(
                    r#"[{{"id":"test","name":"T","description":"","context":32768,"cache_ram":0,"extra_flags":{}}}]"#,
                    flags
                )
            };
            // --log-file y --out-file deben ser rechazadas al importar/persistir.
            let err1 = super::super::parse_profiles_import(&perfil(r#"["--log-file=pwn.log"]"#))
                .unwrap_err();
            assert!(err1.contains("seguridad de escritura"), "{}", err1);

            let err2 = super::super::parse_profiles_import(&perfil(r#"["--out-file", "evil"]"#))
                .unwrap_err();
            assert!(err2.contains("seguridad de escritura"), "{}", err2);
        }
    }
    // ---- Fase UX-Guest: target remoto y resolver sin red ----

    #[cfg(test)]
    fn cfg_guest(url: &str) -> crate::config::AppConfig {
        let mut c = crate::config::AppConfig::default();
        c.client.enabled = true;
        c.remote.url = url.to_string();
        c.remote.key = "KR".to_string();
        c
    }

    #[test]
    fn remoto_status_da_modelo_y_contexto_con_fallbacks() {
        // Status real del Oráculo: stem + contexto.
        let (m, c) = crate::server::remote_status_target(
            r#"{"status":"running","model":"Qwen3.8-27B-IQ4_XS_4BPW.gguf","context":131072}"#,
        );
        assert_eq!((m.as_str(), c), ("Qwen3.8-27B-IQ4_XS_4BPW", 131072));
        // Sin modelo → alias; sin contexto o cero → default honesto.
        let (m, c) =
            crate::server::remote_status_target(r#"{"status":"running","model":"","context":0}"#);
        assert_eq!((m.as_str(), c), ("localmind", 32768));
        let (m, c) = crate::server::remote_status_target("no-json");
        assert_eq!((m.as_str(), c), ("localmind", 32768));
    }

    #[test]
    fn resolver_guest_sin_remoto_y_oraculo_apagado_dan_409() {
        // Guest sin remoto: 409 sin tocar la red (el probe ni arranca).
        let st = crate::process::ServerStatus {
            status: "stopped".to_string(),
            port: 8080,
            ..Default::default()
        };
        let err =
            crate::server::resolve_launch_target(&cfg_guest(""), &st, "K", 17860).unwrap_err();
        assert!(err.contains("remoto_no_configurado"), "{}", err);
        // Oráculo apagado: mensaje histórico intacto, sin red.
        let cfg = crate::config::AppConfig::default();
        let err = crate::server::resolve_launch_target(&cfg, &st, "K", 17860).unwrap_err();
        assert!(err.contains("motor_apagado"), "{}", err);
    }
}
// (módulo `crate::sse`; el `mod sse` interno se extrajo a `sse.rs`).
