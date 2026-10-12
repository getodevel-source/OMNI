#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agents;
mod auth;
mod config;
mod engine_gate;
mod filelog;
mod launcher;
mod meta;
mod models;
mod notify;
mod process;
mod server;
mod sse;
mod tray;
mod update;
mod usage;

use config::ConfigStore;
use process::ProcessManager;
use server::HttpServer;
use std::env;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Icon, WindowBuilder};
use wry::WebViewBuilder;

fn get_base_dir() -> PathBuf {
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            // In “installed” layout: exe junto a models/ y ui.html.
            if parent.join("models").is_dir() {
                return parent.to_path_buf();
            }
        }
    }
    // Desarrollo / server-only: cwd debe contener models/ (raíz del proyecto).
    if let Ok(cwd) = env::current_dir() {
        if cwd.join("models").is_dir() {
            return cwd;
        }
    }
    env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_default()
}

/// Directorio de INSTALACIÓN para el swap del auto-update (P0 2026-10-09):
/// siempre el directorio del ejecutable en ejecución, jamás el `base_dir` de
/// datos (que en desarrollo apunta al repo con `models/` y el swap movería el
/// árbol equivocado: la app quedaba pidiendo reinicio para siempre).
/// Puro en decisión: `check_app_dir` (update.rs) lo valida al usar.
fn app_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_default()
}

/// Iconos de título y taskbar: `omni.ico` junto al exe → `base_dir` (datos) →
/// embebido en el binario. Tao usa ICON_SMALL para el título e ICON_BIG para
/// la barra de tareas; se preparan a 32 y 256 px para evitar el reescalado GDI.
fn load_window_icons(base_dir: &Path) -> Option<(Icon, Icon)> {
    for dir in [app_dir(), base_dir.to_path_buf()] {
        for name in ["omni.ico", "localmind.ico"] {
            if let Ok(bytes) = std::fs::read(dir.join(name)) {
                if let Some(icons) = icons_from_bytes(&bytes) {
                    return Some(icons);
                }
            }
        }
    }
    icons_from_bytes(include_bytes!("../assets/omni.ico"))
}

fn icons_from_bytes(bytes: &[u8]) -> Option<(Icon, Icon)> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    let taskbar_icon = Icon::from_rgba(img.as_raw().clone(), width, height).ok()?;
    let small = image::imageops::resize(&img, 32, 32, image::imageops::FilterType::Lanczos3);
    let window_icon = Icon::from_rgba(small.into_raw(), 32, 32).ok()?;
    Some((window_icon, taskbar_icon))
}

/// Formatear la línea que el hook de pánico escribe en el log (D-46).
/// Pura: el hook en sí (que captura `PanicHookInfo`, no `Send + Sync`) queda
/// fuera, así que esto se puede testear sin matar el proceso de test.
///
/// `panic = "abort"` mata el proceso entero sin desenrollar. El stderr del
/// MOTOR sí queda persistido: `attach_reader_threads` (`process.rs`) escribe
/// stdout y stderr del hijo en `logs/localmind.log` por
/// `filelog::write_log_line` desde 2840237. Lo que NO tiene esa ruta es un
/// pánico del PROPIO LocalMind: el archivo se escribe solo por llamadas
/// explícitas (nada lo hereda por herencia), la app es GUI sin consola
/// (`windows_subsystem = "windows"`), y `abort()` no desenrolla ni deja que un
/// `Drop` vacíe nada. El mensaje del pánico no llegaría, literalmente, a
/// ningún lado: sin esta línea, un pánico de LocalMind no deja rastro. Es una
/// breadcrumb, NO un crash dump: dice qué y dónde, no el estado de memoria.
fn panic_log_line(secs_epoch: u64, payload: &str, file: &str, line: u32) -> String {
    format!(
        "[{}] [LocalMind] PANIC: {} ({}:{})",
        secs_epoch, payload, file, line
    )
}

/// Instalar el hook de pánico (D-46). Envuelve el hook previo en vez de
/// reemplazarlo, así el backtrace por defecto de Rust se sigue viendo.
///
/// ¿Corre con `panic = "abort"`? Sí, verificado empíricamente: una sonda
/// release con `panic = "abort", lto, strip, opt-level=3` escribió su
/// marcador desde el hook y luego murió con 0xC0000409. El hook se invoca
/// antes del `abort()`.
///
/// Lo que NO cubre: `abort()` explícito, SIGSEGV/stack overflow y un panic
/// dentro de un `Drop` durante el desenrollado quedan fuera. Sigue siendo una
/// breadcrumb, no un volcado.
fn install_panic_hook(log_file: PathBuf) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<payload no downcast>".to_string());
        let (file, line) = info
            .location()
            .map(|l| (l.file().to_string(), l.line()))
            .unwrap_or_else(|| ("<sin ubicacion>".to_string(), 0));
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Primero el archivo: es la evidencia que sobrevive al proceso. Si el
        // archivo no se puede escribir, el hook previo (stderr) sigue después.
        crate::filelog::write_log_line(&log_file, &panic_log_line(secs, &payload, &file, line));
        prev(info);
    }));
}

/// Single-instance: intento crear un lock file exclusivo en %TEMP%; si ya existe,
/// enfocar la ventana existente (trae al frente vía PowerShell) y salir.
///
/// Sin carrera D-14: primero se intenta abrir con `create_new` SIN borrar. Solo
/// si el archivo ya existe se sondea si el dueño sigue vivo (abrir en lectura:
/// con `share_mode(0)` del dueño, falla si vive y pasa si es huérfano de un
/// crash). Solo el huérfano confirmado se borra antes de reintentar.
fn acquire_single_instance() -> Option<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    let lock_path = env::temp_dir().join("localmind.lock");
    // Intento 1: crear sin tocar nada (caso feliz, sin carrera).
    if let Ok(f) = OpenOptions::new()
        .create_new(true)
        .write(true)
        .share_mode(0)
        .open(&lock_path)
    {
        return Some(f);
    }
    // El archivo existe: ¿hay dueño vivo? Abrir en lectura: con el `share_mode(0)`
    // del dueño, falla si vive → enfocar+salir; pasa si es huérfano de un crash
    // → borrar y crear de nuevo.
    let live = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&lock_path)
        .is_err();
    if live {
        focus_existing_instance();
        return None;
    }
    let _ = std::fs::remove_file(&lock_path); // huérfano confirmado de un crash previo
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .share_mode(0)
        .open(&lock_path)
    {
        Ok(f) => Some(f),
        Err(_) => {
            // Otro proceso ganó la carrera entre la sonda y el create: enfocar y salir.
            focus_existing_instance();
            None
        }
    }
}

/// Traer al frente la ventana de la instancia viva (best-effort).
fn focus_existing_instance() {
    // Fase C (OMNI): el exe instalado se llama OMNI.exe; se acepta el
    // nombre histórico para no romper el foco durante la transición.
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "$h = Get-Process OMNI,LocalMind -ErrorAction SilentlyContinue | Select-Object -First 1; \
             Add-Type -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool SetForegroundWindow(IntPtr h);' -Name U32 -Namespace W; \
             [W.U32]::SetForegroundWindow($h.MainWindowHandle)",
        ])
        .spawn();
}

fn main() {
    // Single instance: si ya corre otra instancia, enfoca y sale.
    if acquire_single_instance().is_none() {
        return;
    }

    let base_dir = get_base_dir();
    // D-46: ANTES de cualquier otra cosa, para que un pánico temprano (config,
    // servidor, WebView) también deje rastro. Usa el MISMO archivo que
    // `ProcessManager::log`: no hay un canal nuevo ni un archivo nuevo.
    install_panic_hook(crate::filelog::log_file(&base_dir));
    let config = Arc::new(ConfigStore::load(&base_dir));
    let cfg_now = config.get();

    let process_mgr = Arc::new(ProcessManager::new(base_dir.clone(), Arc::clone(&config)));
    // Migración tuning: una línea de log con los perfiles refrescados (P30).
    if let Some(note) = config.take_migration_note() {
        process_mgr.log(&note);
    }
    // Update interrumpido (apagón entre descarga y swap): si quedó un staging
    // válido en %TEMP%, registrarlo como pendiente para instalar al salir.
    for ver in crate::update::recover_stale_stagings() {
        process_mgr.log(&format!(
            "[LocalMind] Actualización {} recuperada de un staging previo: se instalará al reiniciar la app.",
            ver
        ));
    }
    // Healing de modelos: si el swap dejó un dir sin models/ (canal viejo),
    // reponerla desde el respaldo `.prev-<ver>` más próximo.
    match crate::update::heal_models(&base_dir) {
        Ok(Some(prev)) => process_mgr.log(&format!(
            "[LocalMind] models/ repuesta desde el respaldo {} (el update no la traía).",
            prev
        )),
        Ok(None) => {}
        Err(e) => process_mgr.log(&format!("[LocalMind] [WARN] {}", e)),
    }
    // Start embedded HTTP server
    let server = match HttpServer::start(
        Arc::clone(&process_mgr),
        Arc::clone(&config),
        base_dir.clone(),
    ) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("Error al iniciar servidor HTTP: {}", e);
            crate::filelog::write_log_line(&crate::filelog::log_file(&base_dir), &msg);
            // Sin consola (GUI): el fallo debe verse. `rfd` ya es dependencia.
            let _ = rfd::MessageDialog::new()
                .set_title("OMNI no pudo arrancar")
                .set_description(&msg)
                .show();
            return;
        }
    };

    let port = server.port();
    let server_url = format!("http://127.0.0.1:{}", port);

    // Actualización automática (Fase Prod): chequeo silencioso al arrancar
    // (30 s de gracia para no competir con el motor) + re-chequeo cada 6 h
    // si `check_on_startup`. Con `auto_download`, una novedad se descarga
    // sola (la instalación siempre espera al reinicio y la pide el dueño).
    {
        let cfg_bg = Arc::clone(&config);
        let mgr_bg = Arc::clone(&process_mgr);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let log: std::sync::Arc<dyn Fn(String) + Send + Sync> =
                std::sync::Arc::new(move |line: String| mgr_bg.log(&line));
            loop {
                let c = cfg_bg.get();
                if c.update.check_on_startup {
                    crate::update::spawn_check(c.update.feed.clone(), true, Arc::clone(&log));
                }
                std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
            }
        });
    }

    // Native Window Mode
    let event_loop = EventLoop::new();

    let mut window_builder = WindowBuilder::new()
        .with_title("OMNI")
        .with_inner_size(LogicalSize::new(1180.0, 820.0))
        .with_min_inner_size(LogicalSize::new(960.0, 680.0));

    if let Some((window_icon, taskbar_icon)) = load_window_icons(&base_dir) {
        window_builder = window_builder
            .with_window_icon(Some(window_icon))
            .with_taskbar_icon(Some(taskbar_icon));
    }

    let window = match window_builder.build(&event_loop) {
        Ok(w) => w,
        Err(e) => {
            let msg = format!("Error al crear ventana: {}", e);
            crate::filelog::write_log_line(&crate::filelog::log_file(&base_dir), &msg);
            let _ = rfd::MessageDialog::new()
                .set_title("OMNI no pudo arrancar")
                .set_description(&msg)
                .show();
            return;
        }
    };

    let webview = match WebViewBuilder::new(&window).with_url(&server_url).build() {
        Ok(wv) => wv,
        Err(e) => {
            let msg = format!("Error al crear webview (falta WebView2): {}", e);
            crate::filelog::write_log_line(&crate::filelog::log_file(&base_dir), &msg);
            let _ = rfd::MessageDialog::new()
                .set_title("OMNI necesita WebView2")
                .set_description(format!(
                    "{msg}\n\nInstala Microsoft Edge WebView2 y reintenta."
                ))
                .show();
            return;
        }
    };
    // Keep webview alive during event loop
    let _wv = webview;
    let _ = cfg_now; // (leído para materializar el TOML default en el primer arranque)

    window.set_focus();

    // Bandeja (segundo plano real): X/minimizar ocultan, no salen. Si la
    // bandeja falla (entorno sin tray), la app sigue con cierre clásico.
    // Los handles NO son `Send`: viven en este hilo (event-loop), nunca en
    // workers. El refresco va por `WaitUntil(5 s)` en el propio loop.
    let tray_handles: Option<crate::tray::TrayHandles> = match crate::tray::build_tray(&base_dir) {
        Ok(h) => {
            process_mgr.log("[LocalMind] OMNI en segundo plano: cerrar la ventana no detiene el motor (Salir desde la bandeja).");
            Some(h)
        }
        Err(e) => {
            crate::filelog::write_log_line(
                &crate::filelog::log_file(&base_dir),
                &format!("[LocalMind] [WARN] Sin bandeja (cierre clásico): {}", e),
            );
            None
        }
    };
    let tray_ok = tray_handles.is_some();

    let mgr_cleanup = Arc::clone(&process_mgr);

    // Salida real compartida (X con `arm_quit`, `Salir` del tray, Destroyed):
    // detiene el motor + aplica el update pendiente + sale. El swap opera
    // sobre `app_dir()` (dir del exe), NO sobre `base_dir` (datos): en una
    // instalación ambos pueden diferir y mover el equivocado deja la app
    // pidiendo reinicio para siempre (P0 2026-10-09).
    // Idempotente (P0 2026-10-10, re-auditoría): `CloseRequested→Exit` suele
    // venir seguido de `Destroyed`, y restart armado + cierre manual también
    // se combinan. Sin guard, cada ruta lanzaba su propio swap concurrente
    // (doble move + doble relanzamiento). Un solo disparo por proceso.
    static QUIT_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let do_quit = move |mgr: &Arc<ProcessManager>| {
        if QUIT_DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // P0 producción: parar el motor ANTES del swap/salida. Sin esto el
        // `llama-server` quedaba huérfano reteniendo VRAM y puerto, y el
        // relanzamiento post-update chocaba con la instancia vieja.
        mgr.stop();
        let app = app_dir();
        if let Some(pending) = crate::update::pending_update() {
            match crate::update::prepare_install_on_exit(&app) {
                Ok(script) => {
                    // `CREATE_NO_WINDOW` + stdio a null: CERO consola, CERO
                    // terminal colgada. El `.ps1` hace el swap, relanza con
                    // `Start-Process` desacoplado y se autoborra (ver
                    // `swap_script`). Si el spawn falla, queda en el log.
                    const CREATE_NO_WINDOW: u32 = 0x08000000;
                    match std::process::Command::new("powershell.exe")
                        .args([
                            "-NoProfile",
                            "-NonInteractive",
                            "-WindowStyle",
                            "Hidden",
                            "-ExecutionPolicy",
                            "Bypass",
                            "-File",
                            script.to_string_lossy().as_ref(),
                        ])
                        .creation_flags(CREATE_NO_WINDOW)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        Ok(_) => {}
                        Err(e) => {
                            crate::filelog::write_log_line(
                                &crate::filelog::log_file(&base_dir),
                                &format!(
                                    "[LocalMind] No se pudo lanzar la instalación {}: {}",
                                    pending.version, e
                                ),
                            );
                        }
                    }
                }
                Err(e) => {
                    crate::filelog::write_log_line(
                        &crate::filelog::log_file(&base_dir),
                        &format!(
                            "[LocalMind] No se pudo instalar la actualización {}: {}",
                            pending.version, e
                        ),
                    );
                }
            }
        }
    };

    event_loop.run(move |event, _, control_flow| {
        // Tick cada 5 s para refrescar el tray (tooltip + Iniciar/Detener)
        // aunque no haya eventos de ventana/ratón/teclado.
        *control_flow =
            ControlFlow::WaitUntil(std::time::Instant::now() + std::time::Duration::from_secs(5));
        // Reinicio para instalar pedido por `POST /api/update/restart` (P0):
        // mismo `do_quit` que Salir del tray (stop + swap + Exit). El script
        // desacoplado relanza la app ya actualizada.
        if crate::update::take_restart_armed() {
            do_quit(&mgr_cleanup);
            *control_flow = ControlFlow::Exit;
            return;
        }
        if let Some(h) = tray_handles.as_ref() {
            let st = mgr_cleanup.get_status();
            crate::tray::refresh_tray(h, &st.status, st.is_healthy, false);
        }

        // `try_recv` en bucle: pueden acumularse varios eventos por tick.
        while let Ok(ev) = tray_icon::TrayIconEvent::receiver().try_recv() {
            use tray_icon::TrayIconEvent;
            let open = match ev {
                TrayIconEvent::Click {
                    button,
                    button_state,
                    ..
                } => {
                    use tray_icon::{MouseButton, MouseButtonState};
                    button == MouseButton::Left && button_state == MouseButtonState::Up
                }
                TrayIconEvent::DoubleClick { .. } => true,
                _ => false,
            };
            if open {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus();
            }
        }
        // Menú del tray: Abrir / Iniciar / Detener / Salir.
        while let Ok(mev) = muda::MenuEvent::receiver().try_recv() {
            match crate::tray::action_for_menu_id(mev.id.0.as_str()) {
                Some(crate::tray::TrayAction::Open) => {
                    window.set_visible(true);
                    window.set_minimized(false);
                    window.set_focus();
                }
                Some(crate::tray::TrayAction::Start) => {
                    // Arranque con la última config (igual que el botón de la
                    // UI sin cuerpo): el `start()` resuelve modelo/perfil.
                    let mgr = Arc::clone(&mgr_cleanup);
                    std::thread::spawn(move || {
                        let _ = mgr.start(crate::process::StartRequest {
                            model: None,
                            profile: None,
                            context: None,
                            threads: None,
                            priority: None,
                        });
                    });
                }
                Some(crate::tray::TrayAction::Stop) => {
                    mgr_cleanup.stop();
                }
                Some(crate::tray::TrayAction::Quit) => {
                    crate::tray::arm_quit();
                    do_quit(&mgr_cleanup);
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                None => {}
            }
        }

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if crate::tray::quit_armed() || !tray_ok {
                    // Salida real: motor parado + update pendiente aplicado.
                    do_quit(&mgr_cleanup);
                    *control_flow = ControlFlow::Exit;
                } else {
                    // Segundo plano: ocultar, NO parar nada. El lock
                    // single-instance sigue tomado (el proceso vive).
                    window.set_visible(false);
                    // Aviso único: la X no cierra (gateway+motor vivos, VRAM
                    // retenida). Salir de verdad = `Salir` en la bandeja.
                    static TRAY_HINT_ONCE: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !TRAY_HINT_ONCE.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        let ncfg = mgr_cleanup.config_snapshot().notifications.clone();
                        if crate::notify::should_notify(ncfg.enabled, true) {
                            let logf = mgr_cleanup.log_file_path();
                            std::thread::spawn(move || {
                                crate::notify::notify(
                                    "OMNI sigue en segundo plano",
                                    "La ventana se ocultó; el motor sigue en marcha. Salir de verdad: botón derecho en la bandeja → Salir.",
                                    "tray-background",
                                    |err| crate::filelog::write_log_line(&logf, err),
                                );
                            });
                        }
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Destroyed,
                ..
            } => {
                do_quit(&mgr_cleanup);
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-46: la línea de la breadcrumb de pánico. Un test del formateo puro:
    /// no aborta el proceso de test (que sería imposible de asertar).
    #[test]
    fn panic_line_dice_que_where_y_cuando() {
        let l = panic_log_line(1700000000, "index out of bounds", "src\\process.rs", 412);
        assert!(l.starts_with("[1700000000] "), "{:?}", l);
        assert!(l.contains("PANIC"), "{:?}", l);
        assert!(l.contains("index out of bounds"), "{:?}", l);
        assert!(l.contains("src\\process.rs:412"), "{:?}", l);
    }

    /// El payload de un pánico no siempre es un `&str` ni un `String`: hay
    /// tipos propios. La línea debe salir igual, no romperse.
    #[test]
    fn panic_line_tolera_payload_no_texto() {
        let l = panic_log_line(1, "<payload no downcast>", "<sin ubicacion>", 0);
        assert!(l.contains("<payload no downcast>"), "{:?}", l);
        assert!(l.contains("<sin ubicacion>:0"), "{:?}", l);
    }

    /// La breadcrumb va al archivo que rota `filelog`, no a uno nuevo: la línea
    /// tiene que pasar por el mismo formato que el resto del log.
    #[test]
    fn panic_line_pasa_por_el_formato_de_filelog() {
        let dir = std::env::temp_dir().join(format!("lm-panic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = dir.join("localmind.log");
        // Un payload con salto de línea: el formateo de `filelog` lo sanea, así
        // que un panic con mensaje multilínea no rompe el archivo de log.
        crate::filelog::write_log_line(&base, &panic_log_line(7, "a\nb\0c", "f.rs", 9));
        let text = std::fs::read_to_string(&base).unwrap();
        // `write_log_line` antepone SU propio epoch UTC (no el de la línea) y
        // `format_line` sanea \n, \r y \0 a espacio: el archivo tiene UNA línea
        // lógica, que es lo que permite seguir un pánico al leer el log.
        assert!(
            text.contains("[LocalMind] PANIC: a b c (f.rs:9)"),
            "{:?}",
            text
        );
        assert_eq!(text.matches('\n').count(), 1, "una sola linea: {:?}", text);
        assert!(!text.contains('\0'), "{:?}", text);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// D-14: el lock NO se borra a ciegas. Con dueño vivo (handle abierto con
    /// `share_mode(0)`), la sonda de lectura falla → hay instancia viva.
    /// Sin dueño (huérfano de crash), la sonda abre bien → se puede reclamar.
    #[test]
    fn lock_con_dueno_vivo_rechaza_y_huerfano_pasa() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("lm-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("localmind.lock");
        let probe_live = || {
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&p)
                .is_err()
        };
        // Huérfano (archivo cerrado): la sonda pasa → reclamable.
        std::fs::write(&p, b"x").unwrap();
        assert!(!probe_live(), "huérfano debe sondar libre");
        // Dueño vivo: la sonda falla → hay que enfocar+salir, no borrar.
        let owner = std::fs::OpenOptions::new()
            .create_new(false)
            .write(true)
            .share_mode(0)
            .open(&p)
            .unwrap();
        assert!(probe_live(), "con dueño vivo la sonda debe fallar");
        drop(owner);
        assert!(!probe_live(), "tras soltar, vuelve a sondar libre");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
