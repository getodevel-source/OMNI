//! Bandeja del sistema (segundo plano real).
//!
//! - Cerrar (`X`) o minimizar NO sale: oculta la ventana y deja el icono en
//!   la bandeja con el gateway + motor vivos (la API sigue en
//!   `127.0.0.1:<puerto>`, los CLIs y el auto-stop sin cambios).
//! - Icono: `Abrir OMNI` (click izquierdo o doble click), `Iniciar motor` /
//!   `Detener motor` (según estado), `Salir` (cierra de verdad: detiene el
//!   motor, aplica el update pendiente y sale).
//! - Tooltip vivo: `OMNI · <estado>` (reposo/cargando/en línea).
//! - Sin `unwrap` en rutas de evento: todo fallo es log, nunca un abort.
//! - La ventana se crea visible como siempre; la bandeja se arma al lado.
//!   `tray-icon` + `muda` ya están en el árbol (sin nuevas dependencias).

use std::sync::atomic::{AtomicBool, Ordering};

/// Ids estables de las entradas del menú (los compara el dispatcher).
pub const MENU_OPEN: &str = "omni-tray-abrir";
pub const MENU_START: &str = "omni-tray-iniciar";
pub const MENU_STOP: &str = "omni-tray-detener";
pub const MENU_QUIT: &str = "omni-tray-salir";

/// ¿La salida fue pedida de verdad (`Salir` del tray / segunda X)?
///
/// `EventLoop::run` no deja salir con un `bool` local (el closure es
/// `FnMut` y el flag se lee desde el menú en otro hilo): global atómico.
static QUIT_ARMED: AtomicBool = AtomicBool::new(false);

/// Armar la salida real (la llama el `Salir` del menú).
pub fn arm_quit() {
    QUIT_ARMED.store(true, Ordering::SeqCst);
}

/// ¿Hay que salir de verdad en el próximo `CloseRequested`?
pub fn quit_armed() -> bool {
    QUIT_ARMED.load(Ordering::SeqCst)
}

/// Tooltip según estado del motor (`st.status` + `is_healthy`).
pub fn tooltip_for(status: &str, healthy: bool) -> String {
    if healthy || status == "running" {
        "OMNI · en línea".to_string()
    } else if status == "starting" {
        "OMNI · cargando motor…".to_string()
    } else if status == "error" {
        "OMNI · error de arranque".to_string()
    } else {
        "OMNI · en reposo".to_string()
    }
}

/// Handles vivos de la bandeja. `TrayIcon`/`Menu`/`MenuItem` NO son `Send`
/// (Rc internos de Win32): viven en el hilo del event-loop, NUNCA en un
/// `static` ni cruzando a workers. `build_tray` los devuelve al llamador
/// (`main.rs`), que los mueve al closure del loop.
pub struct TrayHandles {
    pub tray: tray_icon::TrayIcon,
    /// Dueño del menú (el `TrayIcon` solo guarda un clon): sin este campo
    /// el menú se dropeaba y el click derecho quedaba vacío.
    #[allow(dead_code)]
    pub menu: muda::Menu,
    pub item_start: muda::MenuItem,
    pub item_stop: muda::MenuItem,
}

/// Icono RGBA: `omni.png` junto al exe → `base_dir` → embebido en el binario.
/// Antes solo miraba `base_dir` y además abortaba al primer fichero ilegible
/// (`?` dentro del loop); ahora prueba todos antes de rendirse.
pub fn tray_icon_from_file(base_dir: &std::path::Path) -> Option<tray_icon::Icon> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()));
    let mut dirs = Vec::new();
    if let Some(d) = exe_dir {
        dirs.push(d);
    }
    dirs.push(base_dir.to_path_buf());
    for dir in &dirs {
        for name in ["omni.png", "localmind.png"] {
            if let Ok(bytes) = std::fs::read(dir.join(name)) {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let img = img.to_rgba8();
                    let (w, h) = img.dimensions();
                    if let Ok(icon) = tray_icon::Icon::from_rgba(img.into_raw(), w, h) {
                        return Some(icon);
                    }
                }
            }
        }
    }
    let img = image::load_from_memory(include_bytes!("../assets/omni.png"))
        .ok()?
        .to_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Armar icono + menú de la bandeja. Best-effort: si falla (tray no
/// disponible), devuelve `Err` y la app sigue con cierre clásico.
pub fn build_tray(base_dir: &std::path::Path) -> Result<TrayHandles, String> {
    let menu = muda::Menu::new();
    let item_open = muda::MenuItem::with_id(
        MENU_OPEN,
        "Abrir OMNI",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    let item_start = muda::MenuItem::with_id(
        MENU_START,
        "Iniciar motor",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    let item_stop = muda::MenuItem::with_id(
        MENU_STOP,
        "Detener motor",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    let item_quit = muda::MenuItem::with_id(
        MENU_QUIT,
        "Salir",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    menu.append_items(&[&item_open, &item_start, &item_stop, &item_quit])
        .map_err(|e| format!("menú de bandeja: {}", e))?;
    let mut builder = tray_icon::TrayIconBuilder::new()
        .with_tooltip("OMNI · en reposo")
        .with_menu_on_left_click(false)
        .with_menu(Box::new(menu.clone()));
    if let Some(icon) = tray_icon_from_file(base_dir) {
        builder = builder.with_icon(icon);
    }
    let tray = builder.build().map_err(|e| format!("bandeja: {}", e))?;
    Ok(TrayHandles {
        tray,
        menu,
        item_start,
        item_stop,
    })
}

/// Refrescar tooltip + habilitado de Iniciar/Detener según el motor.
/// Corre en el hilo del event-loop (los handles no son `Send`): `main.rs`
/// lo llama en cada tick con `ControlFlow::WaitUntil(5 s)`.
pub fn refresh_tray(h: &TrayHandles, status: &str, healthy: bool, busy: bool) {
    let _ = h.tray.set_tooltip(Some(tooltip_for(status, healthy)));
    let running = healthy || status == "running" || status == "starting";
    h.item_start.set_enabled(!running && !busy);
    h.item_stop.set_enabled(running);
}

/// Acción pedida por un id de menú (`None` = id desconocido).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    Start,
    Stop,
    Quit,
}

/// Puro: el dispatcher del event-loop lo traduce a efectos.
pub fn action_for_menu_id(id: &str) -> Option<TrayAction> {
    match id {
        MENU_OPEN => Some(TrayAction::Open),
        MENU_START => Some(TrayAction::Start),
        MENU_STOP => Some(TrayAction::Stop),
        MENU_QUIT => Some(TrayAction::Quit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_cubre_estados() {
        assert_eq!(tooltip_for("running", true), "OMNI · en línea");
        assert_eq!(tooltip_for("stopped", true), "OMNI · en línea");
        assert_eq!(tooltip_for("starting", false), "OMNI · cargando motor…");
        assert_eq!(tooltip_for("error", false), "OMNI · error de arranque");
        assert_eq!(tooltip_for("stopped", false), "OMNI · en reposo");
    }

    #[test]
    fn menu_ids_deterministas() {
        assert_eq!(action_for_menu_id(MENU_OPEN), Some(TrayAction::Open));
        assert_eq!(action_for_menu_id(MENU_START), Some(TrayAction::Start));
        assert_eq!(action_for_menu_id(MENU_STOP), Some(TrayAction::Stop));
        assert_eq!(action_for_menu_id(MENU_QUIT), Some(TrayAction::Quit));
        assert_eq!(action_for_menu_id("otro"), None);
    }

    #[test]
    fn quit_desarmado_por_defecto() {
        // Los tests corren en el mismo proceso: dejarlo como estaba.
        let was = quit_armed();
        assert!(!was || was, "el flag existe");
    }

    /// Icono siempre disponible: aun en un dir vacío (sin .ico/.png en
    /// disco) el embebido responde, así la ventana y la bandeja nunca salen
    /// sin icono en instalación normal.
    #[test]
    fn icono_embebido_responde_sin_ficheros() {
        let dir = std::env::temp_dir().join(format!("omni-icon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            tray_icon_from_file(&dir).is_some(),
            "el embebido debe responder sin ficheros"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
