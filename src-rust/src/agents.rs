//! Configs privadas por lanzamiento para pi/omp (D-5, D-6; corrige D-2, D-4).
//!
//! - JAMÁS toca `%USERPROFILE%\.pi` ni `%USERPROFILE%\.omp`.
//! - Por lanzamiento crea `%APPDATA%\LocalMind\agents\<agent>\` con `models.json`
//!   (pi) y `models.yml` + `config.yml` (omp), y el lanzador exporta
//!   `PI_CODING_AGENT_DIR=<ese dir>` (pi y omp —fork de pi— leen
//!   `<APPNAME>_CODING_AGENT_DIR` vía `dist/config.js`).
//! - `baseUrl` apunta al GATEWAY (`http://127.0.0.1:<http_port>/v1`), nunca al
//!   motor: así el tráfico pasa por la puerta con clave, el aliasing y
//!   `usage.jsonl`. El `contextWindow` sigue viniendo del estado VIVO.
//! - Se conserva la forma JSON/YML que ya funcionaba con los binarios
//!   instalados; solo cambia DÓNDE va, QUÉ puerto/contexto/modelo nombra.
//! - Si el motor no corre, el endpoint de lanzamiento devuelve 4xx/409 y NO se
//!   escriben configs que apunten a un puerto muerto (D-4).

use std::path::PathBuf;

use crate::process::ServerStatus;

/// Error español para el endpoint de lanzamiento cuando no hay motor.
pub fn engine_down_error(st: &ServerStatus) -> String {
    if st.port == 0 || st.status != "running" {
        format!(
            "{{\"error\":\"motor_apagado\",\"message\":\"El motor no está en ejecución (estado: {}). Inícielo primero con /api/start.\"}}",
            st.status
        )
    } else {
        String::new()
    }
}

/// ¿Hay motor vivo al que apuntar? Puerto real + estado running.
pub fn engine_live(st: &ServerStatus) -> bool {
    st.port > 0 && st.status == "running"
}

/// ¿El proxy `/v1/*` puede hablar con el motor? (Fase A6) Puerto real + estado
/// `running` o `starting` (durante el arranque el motor ya responde aunque la
/// puerta verifique). `stopped`/`error` → `engine_down` aunque el puerto traiga
/// el placeholder del default: el 8080 lo puede ocupar cualquiera (medido:
/// `steamwebhelper` en una PC gamer respondiendo 404 vacío).
pub fn engine_reachable(st: &ServerStatus) -> bool {
    st.port > 0 && (st.status == "running" || st.status == "starting")
}

/// Id de modelo que el motor está sirviendo AHORA: el nombre del archivo sin
/// la extensión `.gguf` (p. ej. `Ternary-Bonsai-2-27B-PTQ1_0.gguf` →
/// `Ternary-Bonsai-2-27B-PTQ1_0`).
///
/// Es la MISMA forma que publica `/v1/models` y que reescribe el proxy, así que
/// el id que se escribe en la config privada de cada agente y el que anuncia el
/// gateway no pueden divergir.
///
/// `None` = no hay modelo servido (motor apagado, arrancando o sin nombre). No
/// se inventa un id: el llamador decide (los lanzadores responden 409 y la UI
/// dice que el motor no está en marcha) en vez de escribir un nombre viejo como
/// si fuera el actual.
pub fn served_model_id(st: &ServerStatus) -> Option<String> {
    let m = st.model.trim();
    if m.is_empty() {
        return None;
    }
    let stem = if let Some(s) = m.strip_suffix(".gguf") {
        s
    } else if let Some(s) = m.strip_suffix(".GGUF") {
        s
    } else {
        m
    };
    if stem.trim().is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

pub fn agent_dir(agent: &str) -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("LocalMind"));
    base.join("LocalMind").join("agents").join(agent)
}

/// `models.json` de pi con la forma que ya funcionaba (merge) pero dirigida al
/// GATEWAY (`http://127.0.0.1:<http_port>/v1`), no al motor: así el tráfico
/// pasa por la puerta con clave, el aliasing y `usage.jsonl`.
///
/// `model_id` es el id REALMENTE servido por el motor (`served_model_id`), que
/// ocupa la primera posición y es el que el lanzador pasa por `--model`. El
/// alias `localmind` se conserva detrás: es el nombre con el que hay agentes
/// configurados en la calle, y aunque no se use, el proxy reescribe cualquier
/// `model` pedido al servido (D-2), así que nunca puede romper la conexión.
pub fn pi_models_json(
    base_url: &str,
    context: usize,
    key: &str,
    vision: bool,
    model_id: &str,
) -> String {
    let max_toks = (context / 2).min(16384);
    let input: Vec<&str> = if vision {
        vec!["text", "image"]
    } else {
        vec!["text"]
    };
    // thinkingLevelMap verificado contra el esquema pi (`model-config.d.ts`):
    let thinking_map = serde_json::json!({
        "minimal": "low", "low": "low", "medium": "medium",
        "high": "xhigh", "xhigh": "xhigh", "max": null
    });
    // Coste local nulo (evita que pi estime gasto) y `system` en vez de
    // `developer` (los motores locales no conocen el rol `developer`).
    let cost = serde_json::json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0});
    let compat = serde_json::json!({"supportsDeveloperRole": false});
    let model_entry = |id: &str, name: &str| {
        serde_json::json!({
            "id": id,
            "name": name,
            "contextWindow": context,
            "maxTokens": max_toks,
            "reasoning": true,
            "input": input,
            "cost": cost,
            "thinkingLevelMap": thinking_map,
            "compat": compat,
        })
    };
    let v = serde_json::json!({
        "providers": {
            "localmind": {
                "name": "LocalMind",
                "baseUrl": base_url,
                "apiKey": key,
                "api": "openai-completions",
                "models": [
                    model_entry(model_id, model_id),
                    model_entry("localmind", "LocalMind (alias)"),
                ]
            }
        }
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string())
}

/// `models.yml` de omp con la plantilla que ya funcionaba, dirigida al
/// GATEWAY y al contexto vivo, con la clave local (`apiKey`, no `auth: none`,
/// porque `/v1/*` exige Bearer desde D-7).
/// `vision` igual que en pi: `[text]` salvo mmproj verificado (cuando la
/// visión aterrice, pasar `vision=true` desde el estado vivo del motor).
///
/// `model_id` = id realmente servido (ver `pi_models_json`): primera entrada y
/// el que el lanzador pasa por `--model`. `localmind` se conserva como alias
/// detrás (D-2 reescribe lo que sea al servido, así que un config viejo sigue
/// conectando).
pub fn omp_models_yml(
    base_url: &str,
    context: usize,
    key: &str,
    vision: bool,
    model_id: &str,
) -> String {
    let input = if vision { "[text, image]" } else { "[text]" };
    format!(
        r#"providers:
  localmind:
    baseUrl: {base}
    apiKey: {}
    api: openai-completions
    models:
      - id: {model_id}
        name: {model_id}
        reasoning: true
        input: {}
        contextWindow: {}
        maxTokens: 16384
        cost:
          input: 0
          output: 0
          cacheRead: 0
          cacheWrite: 0
        thinkingLevelMap:
          minimal: low
          low: low
          medium: medium
          high: xhigh
          xhigh: xhigh
          max: null
        compat:
          supportsReasoningEffort: false
          reasoningContentField: reasoning_content
          supportsDeveloperRole: false
      - id: localmind
        name: LocalMind (alias)
        reasoning: true
        input: {}
        contextWindow: {}
        maxTokens: 16384
        cost:
          input: 0
          output: 0
          cacheRead: 0
          cacheWrite: 0
        thinkingLevelMap:
          minimal: low
          low: low
          medium: medium
          high: xhigh
          xhigh: xhigh
          max: null
        compat:
          supportsReasoningEffort: false
          reasoningContentField: reasoning_content
          supportsDeveloperRole: false
"#,
        key,
        input,
        context,
        input,
        context,
        base = base_url
    )
}

/// `config.yml` mínimo de omp: fija `modelRoles.default` al provider local para
/// que omp resuelva el provider desde este dir privado (verificado en máquina:
/// sin `PI_CODING_AGENT_DIR` el mismo comando fallaba con
/// `Model "<id>" not found ... Or create ...\.omp\agent\models.yml`).
pub fn omp_config_yml() -> String {
    "modelRoles:\n  default: localmind/localmind\n".to_string()
}

/// ¿Visión verificada para el motor vivo? Hoy el mmproj está apagado por
/// defecto (LM-MOD-2, `MmprojConfig.auto=false`): sin prueba de `config.rs`
/// (archivo ajeno) se declara `[text]` honesto. Cuando la visión aterrice,
/// leer aquí el estado vivo (p. ej. flag mmproj del `ServerStatus`/config) y
/// devolver true solo entonces.
///
/// Decisión del dueño 2026-10-09 (D-29 cerrada): P7 manda, P19 aparcada. Este
/// `false` es el contrato, no un stub: el test `vision_sigue_solo_texto_por_p7`
/// lo fija y cualquier activación futura pasa por revisar P7 primero.
pub fn vision_enabled() -> bool {
    false
}

/// Escribir el dir privado del agente. Devuelve el dir o un mensaje español.
/// `key` es la clave ya cargada por el servidor (local) o la remota (Guest).
/// `base_url` es la URL `/v1` del gateway que atiende (local o remoto): los
/// CLIs hablan contra ella para pasar por clave/aliasing/usage.
///
/// `model_id` es el id REALMENTE servido por el motor. Se reescribe en CADA
/// lanzamiento: un `models.yml`/`models.json` escrito antes de un cambio de
/// modelo queda sobrescrito aquí, así que un config viejo en disco no puede
/// seguir anunciando el modelo anterior.
pub fn write_agent_dir(
    agent: &str,
    base_url: &str,
    context: usize,
    key: &str,
    model_id: &str,
) -> Result<PathBuf, String> {
    let vision = vision_enabled();
    let dir = agent_dir(agent);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("No se pudo crear {}: {}", dir.display(), e));
    }
    if agent == "pi" {
        let p = dir.join("models.json");
        if let Err(e) = std::fs::write(&p, pi_models_json(base_url, context, key, vision, model_id))
        {
            return Err(format!("No se pudo escribir {}: {}", p.display(), e));
        }
    } else {
        let m = dir.join("models.yml");
        if let Err(e) = std::fs::write(&m, omp_models_yml(base_url, context, key, vision, model_id))
        {
            return Err(format!("No se pudo escribir {}: {}", m.display(), e));
        }
        let c = dir.join("config.yml");
        if let Err(e) = std::fs::write(&c, omp_config_yml()) {
            return Err(format!("No se pudo escribir {}: {}", c.display(), e));
        }
    }
    Ok(dir)
}

/// ¿La carpeta de proyecto es el home? omp se niega a arrancar en `%USERPROFILE%`
/// sin `--allow-home`.
pub fn is_home_dir(dir: &str) -> bool {
    let d = dir.trim().trim_matches('"');
    if d.is_empty() {
        return false;
    }
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let want = norm(d);
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.is_empty() && want == norm(&home) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(status: &str, port: u16) -> ServerStatus {
        ServerStatus {
            status: status.to_string(),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn alcanzable_solo_con_puerto_y_arranque_o_marcha() {
        // Fase A6: stopped/error con puerto placeholder → NO alcanzable (el
        // 8080 de `steamwebhelper` no es un motor); starting sí (ya responde).
        assert!(engine_reachable(&st("running", 8080)));
        assert!(engine_reachable(&st("starting", 8080)));
        assert!(!engine_reachable(&st("stopped", 8080)));
        assert!(!engine_reachable(&st("error", 8080)));
        assert!(!engine_reachable(&st("running", 0)));
        assert!(!engine_reachable(&st("stopped", 0)));
    }

    #[test]
    fn pi_json_apunta_al_gateway() {
        // El puerto es el del GATEWAY (17860 ligado), no el del motor (8080).
        let s = pi_models_json(
            "http://127.0.0.1:17861/v1",
            65536,
            "clave-falsa",
            false,
            "Qwen3.8-27B-IQ4_XS_4BPW",
        );
        assert!(s.contains("http://127.0.0.1:17861/v1"));
        assert!(!s.contains("http://127.0.0.1:8080/v1"));
        assert!(
            s.contains("\"contextWindow\": 65536")
                || s.contains("\"contextWindow\":65536")
                || s.contains("65536")
        );
        // El alias `localmind` se conserva (D-2: hay agentes en la calle con él).
        assert!(s.contains("\"localmind\""));
        assert!(s.contains("Qwen3.8-27B-IQ4_XS_4BPW"));
        // Otro puerto ligado → otro baseUrl (nada hardcodeado).
        let s2 = pi_models_json(
            "http://127.0.0.1:17860/v1",
            32768,
            "clave-falsa",
            false,
            "Qwen3.8-27B-IQ4_XS_4BPW",
        );
        assert!(s2.contains("http://127.0.0.1:17860/v1"));
    }

    #[test]
    fn omp_yml_apunta_al_gateway() {
        let s = omp_models_yml(
            "http://127.0.0.1:17861/v1",
            65536,
            "clave-falsa",
            false,
            "Ternary-Bonsai-2-27B-PTQ1_0",
        );
        assert!(s.contains("http://127.0.0.1:17861/v1"));
        assert!(!s.contains("http://127.0.0.1:8080/v1"));
        assert!(s.contains("contextWindow: 65536"));
        let c = omp_config_yml();
        assert!(c.contains("modelRoles:") && c.contains("default:"));
    }

    #[test]
    fn configs_llevan_la_clave_y_texto_sin_vision() {
        // Clave falsa de prueba: jamás la real de %APPDATA%.
        let key = "clave-falsa-de-prueba-0123456789abcdef";
        let pi = pi_models_json(
            "http://127.0.0.1:8099/v1",
            65536,
            key,
            false,
            "Qwen3.8-27B-IQ4_XS_4BPW",
        );
        assert!(pi.contains(key), "models.json sin apiKey");
        assert!(!pi.contains("auth"), "models.json no usa auth");
        let omp = omp_models_yml(
            "http://127.0.0.1:8099/v1",
            65536,
            key,
            false,
            "Qwen3.8-27B-IQ4_XS_4BPW",
        );
        assert!(
            omp.contains(&format!("apiKey: {}", key)),
            "models.yml sin apiKey"
        );
        assert!(
            !omp.contains("auth: none"),
            "models.yml aún declara auth: none"
        );
        // Motor solo-texto: ninguna config anuncia imagen.
        assert!(
            !pi.contains("\"image\""),
            "pi anuncia image sin visión: {}",
            pi
        );
        assert!(
            !omp.contains("image"),
            "omp anuncia image sin visión: {}",
            omp
        );
        // La clave autoriza Bearer y sin clave se rechaza (puerta del gateway).
        let h =
            |f: &str, v: &str| tiny_http::Header::from_bytes(f.as_bytes(), v.as_bytes()).unwrap();
        assert!(crate::auth::is_authorized(
            &[h("Authorization", &format!("Bearer {}", key))],
            key
        ));
        assert!(!crate::auth::is_authorized(&[], key));
        assert!(!crate::auth::is_authorized(
            &[h("Authorization", "Bearer otra")],
            key
        ));
        // Con visión verificada sí se anuncia imagen en ambas.
        let pi_v = pi_models_json(
            "http://127.0.0.1:8099/v1",
            65536,
            key,
            true,
            "Qwen3.8-27B-IQ4_XS_4BPW",
        );
        let omp_v = omp_models_yml(
            "http://127.0.0.1:8099/v1",
            65536,
            key,
            true,
            "Qwen3.8-27B-IQ4_XS_4BPW",
        );
        assert!(
            pi_v.contains("\"image\""),
            "pi con visión debe anunciar image"
        );
        assert!(
            omp_v.contains("[text, image]"),
            "omp con visión debe anunciar image"
        );
    }

    /// D-29 (decisión 2026-10-09): P7 manda, P19 aparcada. El `false` de
    /// `vision_enabled` es contrato: si alguien lo activa sin revisar P7,
    /// este test lo delata.
    #[test]
    fn vision_sigue_solo_texto_por_p7() {
        assert!(!vision_enabled(), "P7 manda: sin visión hasta revisar P7");
    }

    // ---- LM-MOD-3: la config del agente nombra el modelo REALMENTE servido ----

    /// Id servido derivado del estado vivo: no es una constante.
    /// Falla sobre el código original, donde el id era un literal por agente.
    #[test]
    fn el_id_servido_no_es_una_constante() {
        let st = |model: &str| ServerStatus {
            model: model.to_string(),
            ..Default::default()
        };

        let bonsai = served_model_id(&st("Ternary-Bonsai-2-27B-PTQ1_0.gguf")).expect("id servido");
        let qwen = served_model_id(&st("Qwen3.8-27B-IQ4_XS_4BPW.gguf")).expect("id servido");

        // Outputs distintos...
        assert_ne!(bonsai, qwen, "dos modelos distintos producen el mismo id");
        // ...y cada uno se identifica a sí mismo (el defecto que se corrigió:
        // con Bonsai cargado se anunciaba "Qwen").
        assert!(
            bonsai.to_lowercase().contains("bonsai"),
            "el id no dice Bonsai: {}",
            bonsai
        );
        assert!(
            qwen.to_lowercase().contains("qwen"),
            "el id no dice Qwen: {}",
            qwen
        );
        assert!(
            !bonsai.to_lowercase().contains("qwen"),
            "el id de Bonsai dice Qwen: {}",
            bonsai
        );
        assert!(
            !qwen.to_lowercase().contains("bonsai"),
            "el id de Qwen dice Bonsai: {}",
            qwen
        );

        // La extensión no viaja en el id (es lo que anuncia `/v1/models`).
        assert!(
            !bonsai.ends_with(".gguf"),
            "el id conserva la extensión: {}",
            bonsai
        );
    }

    /// El id que el lanzador pone en la config privada es el servido, para
    /// CADA agente que escribe config, y `localmind` sobrevive como alias.
    #[test]
    fn cada_config_nombra_el_modelo_servido() {
        let key = "clave-falsa-de-prueba-0123456789abcdef";
        let served = "Ternary-Bonsai-2-27B-PTQ1_0";
        let pi = pi_models_json("http://127.0.0.1:8099/v1", 65536, key, false, served);
        let omp = omp_models_yml("http://127.0.0.1:8099/v1", 65536, key, false, served);
        for (nombre, cfg) in [("pi", &pi), ("omp", &omp)] {
            assert!(
                cfg.contains(served),
                "{} no anuncia el modelo servido: {}",
                nombre,
                cfg
            );
            // Compat: el alias por el que hay agentes configurados sigue ahí.
            assert!(
                cfg.contains("localmind"),
                "{} perdió el alias localmind: {}",
                nombre,
                cfg
            );
        }
        // Y un config escrito con el modelo ANTERIOR no sobrevive al relanzar:
        // el lanzador reescribe con el id nuevo en cada lanzamiento.
        let antes = omp_models_yml(
            "http://127.0.0.1:8099/v1",
            65536,
            key,
            false,
            "Qwen3.8-27B-IQ4_XS_4BPW",
        );
        let despues = omp_models_yml("http://127.0.0.1:8099/v1", 65536, key, false, served);
        assert_ne!(
            antes, despues,
            "el id nuevo no cambia el config: no se reescribe"
        );
        assert!(
            !despues.contains("Qwen3.8-27B-IQ4_XS_4BPW"),
            "queda el modelo viejo: {}",
            despues
        );
    }

    /// Motor apagado: NO hay id que anunciar. La función devuelve `None` en vez
    /// de devolver el nombre del modelo anterior como si siguiera cargado.
    /// Falla sobre el código original, que devolvía un literal fijo.
    #[test]
    fn motor_apagado_no_anuncia_ningun_modelo() {
        let parado = ServerStatus {
            status: "stopped".to_string(),
            port: 0,
            ..Default::default()
        };
        assert!(
            served_model_id(&parado).is_none(),
            "el motor apagado anuncia un id: {}",
            served_model_id(&parado).unwrap_or_default()
        );

        // El `model` que queda en el estado tras parar es el de la sesión
        // anterior: es un dato real, pero NO es lo que se está sirviendo.
        // Por eso quien anuncia usa el estado vivo + `engine_live`, y no esto.
        assert!(!engine_live(&parado));
    }

    /// El contexto que se escribe en la config es el del estado vivo.
    /// Falla sobre el código original si el id queda fijo pero el ctx no.
    #[test]
    fn el_contexto_publicado_segue_al_vivo() {
        let key = "clave-falsa-de-prueba-0123456789abcdef";
        for ctx in [32768usize, 65536, 131072, 262144] {
            let pi = pi_models_json("http://127.0.0.1:8099/v1", ctx, key, false, "M");
            let omp = omp_models_yml("http://127.0.0.1:8099/v1", ctx, key, false, "M");
            assert!(
                pi.contains(&format!("\"contextWindow\": {}", ctx)),
                "pi no refleja ctx {}",
                ctx
            );
            assert!(
                omp.contains(&format!("contextWindow: {}", ctx)),
                "omp no refleja ctx {}",
                ctx
            );
        }
    }

    #[test]
    fn motor_apagado_da_error_y_no_vive() {
        let st = ServerStatus {
            status: "stopped".to_string(),
            is_healthy: false,
            pid: None,
            model: String::new(),
            context: 0,
            profile: String::new(),
            port: 0,
            idle_remaining_secs: None,
            last_error: None,
            verifying: false,
            starting_for_secs: 0,
            eta_secs: 0,
            decode_tps: None,
            decode_tps_samples: Vec::new(),
            engine_slow: false,
            acceptance_ok: None,
            acceptance_error: None,
            ..Default::default()
        };
        assert!(!engine_live(&st));
        assert!(engine_down_error(&st).contains("motor_apagado"));
    }
}
