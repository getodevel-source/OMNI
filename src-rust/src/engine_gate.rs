//! Puerta de aceptación y decisiones puras del ciclo del motor.
//!
//! Extraído de `process.rs`: `gate_is_slow`, `should_auto_stop`, `engine_lost`,
//! `eta_secs`, `load_key`, `parse_adapter_ram` (+ `vram_used_mb`/`vram_total_mb`
//! con efecto comando, best-effort). Todo lo testeable vive aquí sin `Path`,
//! sin `ModelInfo`, sin red: el poller (`process.rs`) solo las llama.

/// `engine_slow` (puro): true si la mediana queda bajo el umbral configurable.
/// Solo informativo — el llamador nunca reintenta por esto (LM-NF-3).
pub(crate) fn gate_is_slow(median_tps: f64, slow_at: f64) -> bool {
    median_tps < slow_at
}

/// Umbral efectivo de puerta lenta según el tamaño del modelo (portabilidad).
/// El fijo 20 t/s se calibró para 27B en 16 GB: un 2B en integrada lo pasa
/// sobrado (falso "rápido" si va mal) y un 70B falla siempre (falso "lento").
/// Escalón por GB del .gguf (decode Vulkan medido: 27B ~26-42 t/s, 2.6B
/// ~187 t/s): ≤9 GB → 25, ≤17 GB → 20, >17 GB → 12. Desconocido (0) → cfg.
/// El cfg es TECHO: si el dueño lo baja, manda el suyo (más estricto gana).
pub(crate) fn slow_threshold(cfg_base: f64, size_gb: f64) -> f64 {
    let by_size = if size_gb <= 0.0 {
        cfg_base
    } else if size_gb < 9.0 {
        25.0
    } else if size_gb < 17.0 {
        20.0
    } else {
        12.0
    };
    cfg_base.min(by_size)
}

/// ETA en segundos: duración guardada para la misma clave; en frío,
/// `6 s × tamaño del modelo en GB` (0 si se desconoce el tamaño).
pub(crate) fn eta_secs(stored: Option<u64>, size_gb: f64) -> u64 {
    if let Some(s) = stored {
        return s;
    }
    if size_gb > 0.0 {
        (size_gb * 6.0).round() as u64
    } else {
        0
    }
}

/// Clave del registro de duraciones: nombre del modelo + contexto pedido (D2:
/// nunca se reduce el contexto a espaldas del usuario, así que la clave es exacta).
pub(crate) fn load_key(model: &str, context: usize) -> String {
    format!("{}|{}", model, context)
}

/// Decisión de auto-stop por inactividad (pura y testeable, Fase A5): el
/// llamador ya filtró `status == "running"`, timeout > 0 y motor no ocupado
/// (ocupado refresca la marca, nunca apaga). Sin marca previa (`last == 0`)
/// no se apaga: solo el paso del tiempo real dispara.
pub(crate) fn should_auto_stop(timeout_secs: u64, last_activity: u64, now_secs: u64) -> bool {
    timeout_secs > 0 && last_activity > 0 && now_secs.saturating_sub(last_activity) >= timeout_secs
}

/// Decisión de "motor perdido" (pura y testeable, Fase A5): 10 fallos seguidos
/// de `/health` (~10 s) con status `running` → el hijo murió sin exit visible.
/// Fuera de `running` no aplica (el arranque/cierre tienen su propio ciclo).
pub(crate) fn engine_lost(consecutive_failures: u32, status_running: bool) -> bool {
    consecutive_failures >= 10 && status_running
}

/// Puro: máximo `AdapterRAM` en MB desde salida `wmic ... /value`.
/// `AdapterRAM` es uint32 y muchos drivers AMD lo saturan: el valor exacto
/// `4294967295` (4 GB-1) es el caso conocido, pero hay drivers que reportan
/// otros techos falsos (p. ej. `4293918720` ≈ 4095 MB en una RX 6800 XT de
/// 16 GB). Regla: se acepta solo `n >= 8 GB` (descarta saturados parciales e
/// integradas sin dedicada); por debajo, `None` aunque haya dígitos.
/// `None` = sin adaptador válido.
pub(crate) fn parse_adapter_ram(text: &str) -> Option<u64> {
    parse_adapter_ram_min(text, 8 * 1024)
}

/// Núcleo testeable con umbral explícito en MB.
pub(crate) fn parse_adapter_ram_min(text: &str, min_mb: u64) -> Option<u64> {
    let mut best: Option<u64> = None;
    for line in text.lines() {
        let v = line.split('=').nth(1).unwrap_or("").trim();
        let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u64>() {
            if n == 4294967295 {
                continue;
            }
            let mb = n / (1024 * 1024);
            if mb < min_mb {
                continue;
            }
            best = Some(best.map_or(mb, |b: u64| b.max(mb)));
        }
    }
    best
}

/// MiB de RAM física disponible antes de cargar otro modelo, best-effort.
/// `FreePhysicalMemory` de WMI está expresado en KiB.
pub(crate) fn parse_available_ram_kib(text: &str) -> Option<u64> {
    for line in text.lines() {
        let Some((name, value)) = line.trim().split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("FreePhysicalMemory") {
            continue;
        }
        return value.trim().parse::<u64>().ok().map(|kib| kib / 1024);
    }
    None
}

pub(crate) fn available_ram_mb() -> Option<u64> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let mut command = std::process::Command::new("wmic");
    command
        .args(["os", "get", "FreePhysicalMemory", "/value"])
        .creation_flags(CREATE_NO_WINDOW);
    let output = command_output_with_timeout(command, std::time::Duration::from_secs(5))?;
    if !output.status.success() {
        return None;
    }
    parse_available_ram_kib(&String::from_utf8_lossy(&output.stdout))
}

const HOST_RAM_OS_HEADROOM_MB: u64 = 2 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostModelLoadMode {
    /// Auto (default) or mmap: Windows can retain mapped model pages.
    Mmap,
    /// `--load-mode none`, `--no-mmap`, or DirectIO: lower mapped-page pressure.
    NoMmap,
    /// mlock or CPU-layer placement can keep model tensors resident in host RAM.
    HostResident,
}

/// Estimación conservadora de RAM de inicio, diferenciada por modo de carga.
/// La prueba 64K con auto/mmap dejó menos de 1 GiB libre desde un inicio de
/// ~6.3 GiB; se reserva la mitad del GGUF más 2 GiB para Windows. La prueba
/// directa previa a 32K con `--load-mode none` usó ~1 GiB de RAM física; se
/// reserva 1/4 del archivo más 2 GiB. Si mlock/offload pone pesos en CPU, se
/// reserva el GGUF completo. Esto no estima KV activo ni afirma que el
/// contexto quepa en VRAM.
/// La caché de prompts es un límite de crecimiento posterior, no una
/// asignación al arranque, así que se comunica y reserva aparte cuando está
/// configurada pero no se suma a este mínimo de inicio.
pub(crate) fn required_host_ram_mb(model_mb: u64, mode: HostModelLoadMode) -> u64 {
    let model_reserve_mb = match mode {
        HostModelLoadMode::Mmap => model_mb.div_ceil(2),
        HostModelLoadMode::NoMmap => model_mb.div_ceil(4),
        HostModelLoadMode::HostResident => model_mb,
    };
    model_reserve_mb.saturating_add(HOST_RAM_OS_HEADROOM_MB)
}

/// Interpreta las opciones de carga reales en orden. `auto`/mmap es el default;
/// `none` desactiva mmap en este build. `-kvu` y `--cache-ram` no cambian este
/// modo. Las flags que reservan capas en CPU usan el presupuesto completo.
pub(crate) fn host_model_load_mode(flags: &[String]) -> HostModelLoadMode {
    let mut mode = HostModelLoadMode::Mmap;
    let mut mlock = false;
    let mut cpu_offload = false;
    let mut cpu_moe_offload = false;
    let mut i = 0;
    while i < flags.len() {
        let flag = flags[i].as_str();
        if flag == "--mlock" {
            mlock = true;
        } else if flag == "--mmap" {
            mode = HostModelLoadMode::Mmap;
        } else if flag == "--no-mmap" {
            mode = HostModelLoadMode::NoMmap;
        }
        if let Some(value) = flag
            .strip_prefix("--load-mode=")
            .or_else(|| flag.strip_prefix("-lm="))
        {
            mode = parse_host_model_load_mode(value, mode);
        } else if matches!(flag, "--load-mode" | "-lm") {
            if let Some(value) = flags.get(i + 1).filter(|value| !value.starts_with('-')) {
                mode = parse_host_model_load_mode(value, mode);
                i += 1;
            }
        }

        let gpu_layers = flag
            .strip_prefix("--gpu-layers=")
            .or_else(|| flag.strip_prefix("--n-gpu-layers="))
            .or_else(|| flag.strip_prefix("-ngl="));
        if let Some(value) = gpu_layers {
            cpu_offload = gpu_layers_may_offload(value);
        } else if matches!(flag, "--gpu-layers" | "--n-gpu-layers" | "-ngl") {
            if let Some(layers) = flags.get(i + 1).filter(|layers| !layers.starts_with('-')) {
                cpu_offload = gpu_layers_may_offload(layers);
                i += 1;
            }
        }
        if matches!(
            flag,
            "--cpu-moe" | "-cmoe" | "--n-cpu-moe" | "-ncmoe" | "--n-cpu-ffn" | "-ncffn"
        ) {
            cpu_moe_offload = true;
        }
        i += 1;
    }
    if mlock || cpu_offload || cpu_moe_offload {
        HostModelLoadMode::HostResident
    } else {
        mode
    }
}

fn parse_host_model_load_mode(value: &str, previous: HostModelLoadMode) -> HostModelLoadMode {
    match value {
        "auto" | "mmap" => HostModelLoadMode::Mmap,
        "none" | "dio" => HostModelLoadMode::NoMmap,
        "mlock" | "mmap+mlock" => HostModelLoadMode::HostResident,
        _ => previous,
    }
}

fn gpu_layers_may_offload(value: &str) -> bool {
    match value.parse::<u32>() {
        Ok(layers) => layers < 99,
        Err(_) => !matches!(value, "all"),
    }
}

/// `--cache-ram` se aplica después de la opción gestionada por OMNI. El límite
/// controla crecimiento de la caché durante el uso; no es RAM consumida al
/// arrancar. `-1` (sin límite) se representa como el máximo admitido por OMNI.
pub(crate) fn effective_prompt_cache_mb(flags: &[String], fallback_mb: u64) -> u64 {
    let mut value = None;
    let mut i = 0;
    while i < flags.len() {
        let flag = flags[i].as_str();
        let inline = flag
            .strip_prefix("--cache-ram=")
            .or_else(|| flag.strip_prefix("-cram="));
        if let Some(inline) = inline {
            value = Some(parse_prompt_cache_mb(inline));
        } else if matches!(flag, "--cache-ram" | "-cram") {
            if let Some(next) = flags
                .get(i + 1)
                .filter(|next| !next.starts_with('-') || next.as_str() == "-1")
            {
                value = Some(parse_prompt_cache_mb(next));
                i += 1;
            } else {
                // La opción sin valor hará fallar el runtime. No anunciar una
                // estimación menor antes de que el servidor reporte ese error.
                value = Some(65_536);
            }
        }
        i += 1;
    }
    value.unwrap_or(fallback_mb)
}

fn parse_prompt_cache_mb(value: &str) -> u64 {
    match value.parse::<i64>() {
        Ok(-1) => 65_536,
        Ok(n) if n >= 0 => (n as u64).min(65_536),
        _ => 65_536,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostRamFit {
    Fits { free_mb: u64, required_mb: u64 },
    Insufficient { free_mb: u64, required_mb: u64 },
}

pub(crate) fn check_host_ram_fit(
    model_mb: u64,
    load_mode: HostModelLoadMode,
    available_ram_mb: u64,
) -> HostRamFit {
    let required_mb = required_host_ram_mb(model_mb, load_mode);
    if available_ram_mb >= required_mb {
        HostRamFit::Fits {
            free_mb: available_ram_mb,
            required_mb,
        }
    } else {
        HostRamFit::Insufficient {
            free_mb: available_ram_mb,
            required_mb,
        }
    }
}

/// VRAM en uso en MB (Windows, D-21, best-effort): `nvidia-smi` si existe
/// (parsea `memory.used`); si no, se devuelve `None` (el texto del motor queda
/// intacto: no hay contador barato y estable para AMD/Intel desde aquí, y no
/// se inventa). `None` = sin dato, nunca un error visible.
pub(crate) fn vram_used_mb() -> Option<u64> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    if let Ok(out) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        if out.status.success() {
            let digits: String = String::from_utf8_lossy(&out.stdout)
                .chars()
                .filter(|c| c.is_ascii_digit())
                .take(12)
                .collect();
            if let Ok(n) = digits.parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// VRAM TOTAL instalada en MB (Windows, D-21, best-effort): WMI
/// `Win32_VideoController.AdapterRAM` (máximo entre adaptadores). No es USO, es
/// techo instalado: el llamador lo etiqueta como tal. `None` = sin dato.
pub(crate) fn vram_total_mb() -> Option<u64> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let out = std::process::Command::new("wmic")
        .args([
            "path",
            "Win32_VideoController",
            "get",
            "AdapterRAM",
            "/value",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_adapter_ram(&String::from_utf8_lossy(&out.stdout))
}

/// Clasificación de memoria antes del arranque. Un resultado excedido es una
/// advertencia: llama.cpp puede mantener capas en CPU, así que no se bloquea
/// un modelo que quizá pueda ejecutarse con colocación parcial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VramFit {
    Fits {
        model_mb: u64,
        kv_mb: u64,
        required_mb: u64,
        free_mb: u64,
    },
    Exceeds {
        model_mb: u64,
        kv_mb: u64,
        required_mb: u64,
        free_mb: u64,
    },
    Unknown(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceMemory {
    pub total_mb: u64,
    pub free_mb: u64,
}

/// Parsea la memoria del dispositivo exacto que `--device` usará, desde
/// `llama-server --list-devices`; no mezcla una integrada con la GPU dedicada.
pub(crate) fn parse_llama_device_memory(output: &str, device_id: &str) -> Option<DeviceMemory> {
    for line in output.lines() {
        let (id, rest) = match line.trim().split_once(':') {
            Some(parts) => parts,
            None => continue,
        };
        if id.trim() != device_id {
            continue;
        }
        let Some(open) = rest.rfind('(') else {
            continue;
        };
        let inside = rest[open + 1..].split(')').next().unwrap_or("");
        let mut total = None;
        let mut free = None;
        for part in inside.split(',') {
            let Some(mb) = parse_memory_amount_mb(part) else {
                continue;
            };
            if part.to_ascii_lowercase().contains("free") {
                free = Some(mb);
            } else if total.is_none() {
                total = Some(mb);
            }
        }
        let (Some(total_mb), Some(free_mb)) = (total, free) else {
            continue;
        };
        return Some(DeviceMemory { total_mb, free_mb });
    }
    None
}

fn parse_memory_amount_mb(text: &str) -> Option<u64> {
    let mut words = text.split_whitespace();
    let amount = words.next()?.parse::<f64>().ok()?;
    let unit = words.next()?.to_ascii_lowercase();
    let factor = match unit.as_str() {
        "mib" | "mb" => 1.0,
        "gib" | "gb" => 1024.0,
        _ => return None,
    };
    if !amount.is_finite() || amount < 0.0 {
        return None;
    }
    Some((amount * factor).round() as u64)
}

/// Lista dispositivos con timeout para los caminos de hardware y estimación.
pub(crate) fn llama_device_list(binary: &std::path::Path) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let mut command = std::process::Command::new(binary);
    command
        .arg("--list-devices")
        .creation_flags(CREATE_NO_WINDOW);
    let output = command_output_with_timeout(command, std::time::Duration::from_secs(5))?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Obtiene memoria del adaptador seleccionado sin iniciar ni cargar un modelo.
pub(crate) fn llama_device_memory(
    binary: &std::path::Path,
    device_id: &str,
) -> Option<DeviceMemory> {
    let output = llama_device_list(binary)?;
    parse_llama_device_memory(&output, device_id)
}

/// Estima pesos + KV activo, sin buffers temporales del backend. El GGUF debe
/// describir un Transformer denso conocido; arquitecturas híbridas/MLA o
/// metadatos parciales quedan como `Unknown`. Es un aviso, no garantía de fit.
/// La estimación de KV usa las flags actuales `q4_0`.
pub(crate) fn estimate_vram_fit(
    model_path: &std::path::Path,
    context: usize,
    device_free_mb: Option<u64>,
) -> VramFit {
    let free_mb = match device_free_mb {
        Some(n) => n,
        None => return VramFit::Unknown("No hay memoria libre del dispositivo seleccionado."),
    };
    let model_mb = match std::fs::metadata(model_path) {
        Ok(m) => m.len().div_ceil(1024 * 1024),
        Err(_) => return VramFit::Unknown("No se pudo leer el tamaño del GGUF."),
    };
    let bytes_per_token = match gguf_q4_0_kv_bytes_per_token(model_path) {
        Some(n) => n,
        None => {
            return VramFit::Unknown(
                "El GGUF no ofrece metadatos KV compatibles con esta estimación.",
            )
        }
    };
    let kv_bytes = match bytes_per_token.checked_mul(context as u64) {
        Some(n) => n,
        None => return VramFit::Unknown("El cálculo del KV excede el rango admitido."),
    };
    let kv_mb = kv_bytes.div_ceil(1024 * 1024);
    check_vram_fit_mb(model_mb, kv_mb, free_mb)
}

/// Flags that leave the estimated model and Q4_0 KV allocation unchanged.
/// Unknown flags keep the estimate conservative rather than claiming `Fits`.
pub(crate) fn vram_flags_are_known(flags: &[String]) -> bool {
    flags.iter().all(|flag| flag == "-kvu")
}

pub(crate) fn vram_estimate_is_complete(flags: &[String], has_mmproj: bool) -> bool {
    !has_mmproj && vram_flags_are_known(flags)
}

#[derive(Default)]
struct KvMetadata {
    architecture: Option<String>,
    layers: Option<u64>,
    kv_heads: Option<u64>,
    heads: Option<u64>,
    embedding: Option<u64>,
    key_length: Option<u64>,
    value_length: Option<u64>,
}

fn gguf_q4_0_kv_bytes_per_token(path: &std::path::Path) -> Option<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    if &magic != b"GGUF" {
        return None;
    }
    let version = read_u32(&mut file).ok()?;
    if !(2..=3).contains(&version) {
        return None;
    }
    let _tensor_count = read_u64(&mut file).ok()?;
    let metadata_count = read_u64(&mut file).ok()?;
    if metadata_count > 100_000 {
        return None;
    }
    let mut meta = KvMetadata::default();
    for _ in 0..metadata_count {
        let key = read_gguf_string(&mut file, 4096)?;
        let value_type = read_u32(&mut file).ok()?;
        match key.as_str() {
            "general.architecture" if value_type == 8 => {
                meta.architecture = Some(read_gguf_string(&mut file, 128)?);
            }
            _ if key == "general.architecture" => skip_gguf_value(&mut file, value_type)?,
            _ if key.ends_with(".block_count") => {
                set_numeric(&mut meta.layers, &mut file, value_type)?
            }
            _ if key.ends_with(".attention.head_count_kv") => {
                set_numeric(&mut meta.kv_heads, &mut file, value_type)?
            }
            _ if key.ends_with(".attention.head_count") => {
                set_numeric(&mut meta.heads, &mut file, value_type)?
            }
            _ if key.ends_with(".embedding_length") => {
                set_numeric(&mut meta.embedding, &mut file, value_type)?
            }
            _ if key.ends_with(".attention.key_length") => {
                set_numeric(&mut meta.key_length, &mut file, value_type)?
            }
            _ if key.ends_with(".attention.value_length") => {
                set_numeric(&mut meta.value_length, &mut file, value_type)?
            }
            _ => skip_gguf_value(&mut file, value_type)?,
        }
    }
    let arch = meta.architecture.as_deref()?;
    // No asumir que un número de capas equivale a KV uniforme en SSM/MLA/híbridos.
    if !matches!(
        arch,
        "llama"
            | "qwen2"
            | "qwen3"
            | "mistral"
            | "mixtral"
            | "gemma"
            | "gemma2"
            | "gemma3"
            | "phi3"
            | "phi4"
            | "starcoder2"
            | "command-r"
            | "internlm2"
            | "stablelm"
            | "bloom"
    ) {
        return None;
    }
    let layers = meta.layers?;
    let kv_heads = meta.kv_heads.or(meta.heads)?;
    let key_len = match meta.key_length {
        Some(key_len) => key_len,
        None => {
            let heads = meta.heads.filter(|heads| *heads > 0)?;
            meta.embedding? / heads
        }
    };
    let value_len = meta.value_length.unwrap_or(key_len);
    if layers == 0 || kv_heads == 0 || key_len == 0 || value_len == 0 {
        return None;
    }
    // Q4_0 usa 18 bytes por bloque de 32 elementos. Se redondea por fila K/V.
    let q4_row = |n: u64| n.div_ceil(32).checked_mul(18);
    let row_pair = q4_row(key_len)?.checked_add(q4_row(value_len)?)?;
    layers.checked_mul(kv_heads)?.checked_mul(row_pair)
}

fn read_u32<R: std::io::Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    reader.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: std::io::Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    reader.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_gguf_string<R: std::io::Read>(reader: &mut R, max_len: u64) -> Option<String> {
    let len = read_u64(reader).ok()?;
    if len > max_len {
        return None;
    }
    let mut bytes = vec![0u8; usize::try_from(len).ok()?];
    reader.read_exact(&mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

fn set_numeric<R: std::io::Read + std::io::Seek>(
    out: &mut Option<u64>,
    reader: &mut R,
    value_type: u32,
) -> Option<()> {
    let value = match value_type {
        0 => {
            let mut b = [0u8; 1];
            reader.read_exact(&mut b).ok()?;
            u64::from(b[0])
        }
        2 => {
            let mut b = [0u8; 2];
            reader.read_exact(&mut b).ok()?;
            u64::from(u16::from_le_bytes(b))
        }
        4 => u64::from(read_u32(reader).ok()?),
        10 => read_u64(reader).ok()?,
        _ => {
            skip_gguf_value(reader, value_type)?;
            return Some(());
        }
    };
    *out = Some(value);
    Some(())
}

fn skip_gguf_value<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    value_type: u32,
) -> Option<()> {
    use std::io::SeekFrom;
    let fixed = match value_type {
        0 | 1 | 7 => Some(1u64),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        8 => {
            let len = read_u64(reader).ok()?;
            skip_gguf_bytes(reader, len)?;
            return Some(());
        }
        9 => {
            let element_type = read_u32(reader).ok()?;
            let count = read_u64(reader).ok()?;
            if count > 100_000 {
                return None;
            }
            if element_type == 8 {
                let mut remaining = remaining_gguf_bytes(reader)?;
                for _ in 0..count {
                    if remaining < 8 {
                        return None;
                    }
                    let len = read_u64(reader).ok()?;
                    remaining -= 8;
                    if len > remaining {
                        return None;
                    }
                    reader
                        .seek(SeekFrom::Current(i64::try_from(len).ok()?))
                        .ok()?;
                    remaining -= len;
                }
            } else {
                let size = match element_type {
                    0 | 1 | 7 => 1u64,
                    2 | 3 => 2,
                    4..=6 => 4,
                    10..=12 => 8,
                    _ => return None,
                };
                let bytes = count.checked_mul(size)?;
                skip_gguf_bytes(reader, bytes)?;
            }
            return Some(());
        }
        _ => return None,
    };
    skip_gguf_bytes(reader, fixed?)?;
    Some(())
}

fn skip_gguf_bytes<R: std::io::Seek>(reader: &mut R, bytes: u64) -> Option<()> {
    use std::io::SeekFrom;
    let remaining = remaining_gguf_bytes(reader)?;
    if bytes > remaining {
        return None;
    }
    reader
        .seek(SeekFrom::Current(i64::try_from(bytes).ok()?))
        .ok()?;
    Some(())
}

fn remaining_gguf_bytes<R: std::io::Seek>(reader: &mut R) -> Option<u64> {
    use std::io::SeekFrom;
    let current = reader.stream_position().ok()?;
    let end = reader.seek(SeekFrom::End(0)).ok()?;
    reader.seek(SeekFrom::Start(current)).ok()?;
    end.checked_sub(current)
}

fn command_output_with_timeout(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    use std::process::Stdio;
    use std::time::Instant;

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    child.wait_with_output().ok()
}

/// Nucleo puro: memoria del modelo más KV Q4_0 frente a VRAM libre real.
pub(crate) fn check_vram_fit_mb(model_mb: u64, kv_mb: u64, available_vram_mb: u64) -> VramFit {
    let required_mb = model_mb.saturating_add(kv_mb);
    if required_mb <= available_vram_mb {
        VramFit::Fits {
            model_mb,
            kv_mb,
            required_mb,
            free_mb: available_vram_mb,
        }
    } else {
        VramFit::Exceeds {
            model_mb,
            kv_mb,
            required_mb,
            free_mb: available_vram_mb,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// D-21: `AdapterRAM` dice el TECHO instalado, no el uso. 16 GB =
    /// 17179869184 B → 16384 MB; gana el máximo entre adaptadores.
    /// Saturados (uint32 exacto o techos falsos <8 GB como los 4095 MB de una
    /// RX 6800 XT de 16 GB) y ruido <8 GB → `None`: mejor sin dato que con
    /// dato falso junto al real del motor.
    #[test]
    fn adapter_ram_da_techo_maximo_y_filtra_basura() {
        assert_eq!(
            parse_adapter_ram("AdapterRAM=17179869184\r\nAdapterRAM=17179869184\r\n"),
            Some(16384)
        );
        // Caso real medido 2026-10-09 (RX 6800 XT 16 GB): driver saturado.
        assert_eq!(parse_adapter_ram("AdapterRAM=4293918720\r\n"), None);
        assert_eq!(parse_adapter_ram("AdapterRAM=4294967295\r\n"), None);
        assert_eq!(parse_adapter_ram("AdapterRAM=2147483648\r\n"), None);
        assert_eq!(parse_adapter_ram("AdapterRAM=134217728\r\n"), None);
        assert_eq!(parse_adapter_ram(""), None);
        // Umbral explícito: con min 1 GB, 2 GB sí pasa (integradas honestas).
        assert_eq!(
            parse_adapter_ram_min("AdapterRAM=2147483648\r\n", 1024),
            Some(2048)
        );
    }

    #[test]
    fn host_ram_gate_preserves_headroom_by_loading_mode() {
        assert_eq!(
            parse_available_ram_kib("\r\nFreePhysicalMemory=10485760\r\n"),
            Some(10_240)
        );
        assert_eq!(parse_available_ram_kib("FreePhysicalMemory=0"), Some(0));
        assert_eq!(
            parse_available_ram_kib("TotalVisibleMemorySize=16384000"),
            None
        );

        // Qwen3.8's installed file is about 13,256 MiB. Mmap needs a larger
        // startup allowance than `none`; cache flags do not change placement.
        assert_eq!(required_host_ram_mb(13_256, HostModelLoadMode::Mmap), 8_676);
        assert_eq!(
            check_host_ram_fit(13_256, HostModelLoadMode::Mmap, 8_675),
            HostRamFit::Insufficient {
                free_mb: 8_675,
                required_mb: 8_676
            }
        );
        assert_eq!(
            check_host_ram_fit(13_256, HostModelLoadMode::Mmap, 8_676),
            HostRamFit::Fits {
                free_mb: 8_676,
                required_mb: 8_676
            }
        );
        assert_eq!(
            required_host_ram_mb(13_256, HostModelLoadMode::NoMmap),
            5_362
        );
        assert_eq!(
            required_host_ram_mb(13_256, HostModelLoadMode::HostResident),
            15_304
        );
    }

    #[test]
    fn host_ram_gate_interpreta_load_mode_y_flags_de_memoria() {
        let flags = |values: &[&str]| values.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            host_model_load_mode(&flags(&["-kvu"])),
            HostModelLoadMode::Mmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--cache-ram", "512"])),
            HostModelLoadMode::Mmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--load-mode", "none"])),
            HostModelLoadMode::NoMmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--load-mode", "--no-mmap"])),
            HostModelLoadMode::NoMmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["-ngl", "--load-mode=none"])),
            HostModelLoadMode::NoMmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--load-mode=none"])),
            HostModelLoadMode::NoMmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--no-mmap"])),
            HostModelLoadMode::NoMmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--load-mode", "none", "--load-mode", "auto"])),
            HostModelLoadMode::Mmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--load-mode=mmap+mlock"])),
            HostModelLoadMode::HostResident
        );
        assert_eq!(
            host_model_load_mode(&flags(&["-ngl", "0"])),
            HostModelLoadMode::HostResident
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--mlock", "--mmap"])),
            HostModelLoadMode::HostResident
        );
        assert_eq!(
            host_model_load_mode(&flags(&["-ngl", "0", "--load-mode", "auto"])),
            HostModelLoadMode::HostResident
        );
        assert_eq!(
            host_model_load_mode(&flags(&["-ngl", "0", "-ngl", "all"])),
            HostModelLoadMode::Mmap
        );
        assert_eq!(
            host_model_load_mode(&flags(&["-ngl", "0", "--gpu-layers"])),
            HostModelLoadMode::HostResident
        );
        assert_eq!(
            host_model_load_mode(&flags(&["--gpu-layers", "--load-mode=none"])),
            HostModelLoadMode::NoMmap
        );

        assert_eq!(effective_prompt_cache_mb(&flags(&[]), 4096), 4096);
        assert_eq!(
            effective_prompt_cache_mb(&flags(&["--cache-ram", "512"]), 4096),
            512
        );
        assert_eq!(
            effective_prompt_cache_mb(&flags(&["--cache-ram=1024", "-cram=256"]), 4096),
            256
        );
        assert_eq!(
            effective_prompt_cache_mb(&flags(&["--cache-ram", "--cache-ram", "512"]), 4096),
            512
        );
        assert_eq!(
            effective_prompt_cache_mb(&flags(&["-cram", "-1"]), 0),
            65_536
        );
    }

    #[test]
    fn vram_fit_usa_kv_q4_y_vram_libre_sin_descontar_prompt_cache() {
        assert_eq!(
            check_vram_fit_mb(14336, 2304, 16384),
            VramFit::Exceeds {
                model_mb: 14336,
                kv_mb: 2304,
                required_mb: 16640,
                free_mb: 16384
            }
        );
        assert_eq!(
            check_vram_fit_mb(14336, 2304, 20000),
            VramFit::Fits {
                model_mb: 14336,
                kv_mb: 2304,
                required_mb: 16640,
                free_mb: 20000
            }
        );
    }

    #[test]
    fn list_devices_usa_memoria_del_adaptador_seleccionado() {
        let output = "Available devices:\n  Vulkan0: AMD Radeon RX (16368 MiB, 15569 MiB free)\n  Vulkan1: Integrated (7988 MiB, 7589 MiB free)\n";
        assert_eq!(
            parse_llama_device_memory(output, "Vulkan1"),
            Some(DeviceMemory {
                total_mb: 7988,
                free_mb: 7589
            })
        );
        assert_eq!(parse_llama_device_memory(output, "CUDA0"), None);
        assert_eq!(
            parse_llama_device_memory("Vulkan0: GPU (16 GiB, 12.5 GiB free)\n", "Vulkan0"),
            Some(DeviceMemory {
                total_mb: 16384,
                free_mb: 12800
            })
        );
        assert_eq!(
            parse_llama_device_memory(
                "Vulkan0: GPU (16 GiB, 12 GiB free, future backend tag)\n",
                "Vulkan0"
            ),
            Some(DeviceMemory {
                total_mb: 16384,
                free_mb: 12288
            })
        );
        assert_eq!(
            parse_llama_device_memory(
                "Vulkan0: malformed duplicate\nVulkan0: GPU (16 GiB, 12 GiB free)\n",
                "Vulkan0"
            ),
            Some(DeviceMemory {
                total_mb: 16384,
                free_mb: 12288
            })
        );
    }

    fn gguf_string(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    }

    fn gguf_numeric(out: &mut Vec<u8>, key: &str, value: u32) {
        gguf_string(out, key);
        out.extend_from_slice(&4u32.to_le_bytes()); // UINT32
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn write_test_gguf(path: &std::path::Path, architecture: &str) {
        write_test_gguf_with_heads(path, architecture, 32);
    }

    fn write_test_gguf_with_heads(path: &std::path::Path, architecture: &str, heads: u32) {
        let mut out = b"GGUF".to_vec();
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // tensor count
        out.extend_from_slice(&5u64.to_le_bytes()); // metadata count
        gguf_string(&mut out, "general.architecture");
        out.extend_from_slice(&8u32.to_le_bytes()); // STRING
        gguf_string(&mut out, architecture);
        gguf_numeric(&mut out, &format!("{}.block_count", architecture), 64);
        gguf_numeric(
            &mut out,
            &format!("{}.attention.head_count_kv", architecture),
            8,
        );
        gguf_numeric(
            &mut out,
            &format!("{}.attention.head_count", architecture),
            heads,
        );
        gguf_numeric(
            &mut out,
            &format!("{}.embedding_length", architecture),
            4096,
        );
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn gguf_estima_q4_0_desde_metadatos_y_rechaza_arq_desconocida() {
        let dir = std::env::temp_dir().join(format!("omni-gguf-kv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let supported = dir.join("supported.gguf");
        write_test_gguf(&supported, "qwen3");
        assert_eq!(gguf_q4_0_kv_bytes_per_token(&supported), Some(73_728));
        assert_eq!(
            estimate_vram_fit(&supported, 32_768, Some(2_000)),
            VramFit::Exceeds {
                model_mb: 1,
                kv_mb: 2304,
                required_mb: 2305,
                free_mb: 2_000
            }
        );
        let unsupported = dir.join("unsupported.gguf");
        write_test_gguf(&unsupported, "qwen3next");
        assert_eq!(gguf_q4_0_kv_bytes_per_token(&unsupported), None);
        let zero_heads = dir.join("zero-heads.gguf");
        write_test_gguf_with_heads(&zero_heads, "qwen3", 0);
        assert_eq!(gguf_q4_0_kv_bytes_per_token(&zero_heads), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gguf_array_skip_rechaza_cantidad_y_longitud_fuera_del_archivo() {
        use std::io::Cursor;

        let mut too_many_bytes = 8u32.to_le_bytes().to_vec();
        too_many_bytes.extend_from_slice(&100_001u64.to_le_bytes());
        let mut too_many = Cursor::new(too_many_bytes);
        assert_eq!(skip_gguf_value(&mut too_many, 9), None);

        let mut truncated_bytes = 8u32.to_le_bytes().to_vec();
        truncated_bytes.extend_from_slice(&1u64.to_le_bytes());
        truncated_bytes.extend_from_slice(&500u64.to_le_bytes());
        let mut truncated_string = Cursor::new(truncated_bytes);
        assert_eq!(skip_gguf_value(&mut truncated_string, 9), None);
    }

    #[test]
    fn estimacion_vram_acepta_solo_flag_conocido_y_no_filtra_otros() {
        assert!(vram_flags_are_known(&["-kvu".to_string()]));
        assert!(vram_flags_are_known(&[]));
        assert!(!vram_flags_are_known(&["--ctx-size=262144".to_string()]));
        assert!(!vram_flags_are_known(&[
            "-kvu".to_string(),
            "--gpu-layers=0".to_string()
        ]));
        assert!(vram_estimate_is_complete(&["-kvu".to_string()], false));
        assert!(!vram_estimate_is_complete(&[], true));
    }
    /// Umbral relativo: 2B→25, 27B→20, 70B→12, desconocido→cfg; el cfg como
    /// techo (dueño más estricto gana).
    #[test]
    fn slow_threshold_escala_con_tamano_y_cfg_es_techo() {
        assert_eq!(slow_threshold(20.0, 0.0), 20.0);
        assert_eq!(slow_threshold(20.0, 2.0), 20.0);
        assert_eq!(slow_threshold(99.0, 2.0), 25.0);
        assert_eq!(slow_threshold(99.0, 14.0), 20.0);
        assert_eq!(slow_threshold(99.0, 40.0), 12.0);
        assert_eq!(slow_threshold(10.0, 2.0), 10.0);
    }

    #[test]
    fn eta_stored_and_cold() {
        assert_eq!(eta_secs(Some(95), 13.0), 95);
        // Frío: 6 s × 13 GB = 78 s.
        assert_eq!(eta_secs(None, 13.0), 78);
        assert_eq!(eta_secs(None, 0.0), 0);
    }

    #[test]
    fn gate_lento_bajo_umbral() {
        // Umbral default 20: bajo → true; igual o más → false.
        assert!(gate_is_slow(19.9, 20.0));
        assert!(!gate_is_slow(20.0, 20.0));
        assert!(!gate_is_slow(34.0, 20.0));
    }

    #[test]
    fn auto_stop_borde_y_guardas() {
        // timeout 5400, última actividad hace 5400 → apaga (borde incluido).
        assert!(should_auto_stop(5400, 1000, 6400));
        assert!(should_auto_stop(5400, 1000, 9999));
        // Un segundo antes → no apaga.
        assert!(!should_auto_stop(5400, 1000, 6399));
        // Sin timeout o sin marca previa → nunca apaga.
        assert!(!should_auto_stop(0, 1000, 99999));
        assert!(!should_auto_stop(5400, 0, 99999));
    }

    #[test]
    fn motor_perdido_a_los_10_fallos_en_running() {
        assert!(!engine_lost(9, true));
        assert!(engine_lost(10, true));
        assert!(engine_lost(25, true));
        // Fuera de running no aplica (arranque/cierre tienen su ciclo).
        assert!(!engine_lost(10, false));
        assert!(!engine_lost(99, false));
    }
}
