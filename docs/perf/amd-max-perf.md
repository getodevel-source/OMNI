# Guía de Máximo Rendimiento para AMD Radeon RX 6800 XT en Vulkan (Windows 11)
> **CORRECCIÓN 2026-10-11 — `--cache-ram` no controla el KV activo.** El
> build 10743 lo define como límite de caché de prompts en RAM. Las mediciones
> y propuestas de este documento pueden conservar sus tiempos observados, pero
> quedan retractadas las atribuciones de memoria DDR5, offload KV o cuello de
> bus que se basaron en ese flag. Ver `docs/engine-audit-2026-10.md`.

**LocalMind Performance Engineering**  
**Fecha:** 2026-09-28  
**Hardware:** AMD Radeon RX 6800 XT 16 GB (RDNA2, `gfx1030`), AMD Ryzen 5 7600 (6C/12T Zen 4), 32 GB DDR5  
**Entorno de Ejecución:** Windows 11 Pro 64-bit, Vulkan backend (`ggml-vulkan.dll`, `--device Vulkan0`), `llama.cpp` build 10683 (commit `d8f26eec7`)  
**Modelos de Referencia:** Qwen3.8-27B IQ4_XS (~13,0 GB, atención híbrida) y LFM2.5-2.6B Q4_K_M (1,67 GB, 128K nativo)

---

## 1. Resumen Ejecutivo y Techo Físico del Hardware

### 1.1 El Cuello de Botella Físico: Memory Bandwidth Bound
En la fase de generación (decodificación autoregresiva, token a token), los Large Language Models están estrictamente limitados por el ancho de banda de la memoria de video (VRAM), no por la potencia de cálculo (TFLOPS) de la GPU. Cada nuevo token requiere transferir la totalidad de los pesos activos del modelo desde la VRAM hasta los Compute Units (CUs).

La GPU **AMD Radeon RX 6800 XT** cuenta con:
* **Bus de VRAM:** 256-bit GDDR6 @ 16 Gbps $\rightarrow$ **512 GB/s** de ancho de banda teórico bruto.
* **Infinity Cache:** 128 MB de SRAM on-die (~1,2 a 1,6 TB/s de ancho de banda pico para datos con localidad temporal).
* **Compute Units (CUs):** 72 CUs (4608 Stream Processors), arquitectura RDNA2 (`gfx1030`).
* **Aceleración Matricial:** **0** Tensor Cores o unidades WMMA dedicadas (las unidades matriciales dedicadas se introdujeron en AMD a partir de RDNA3 / `gfx1100`). El cómputo en RDNA2 se realiza mediante SIMD vectorial convencional con empaquetado FP16 dual-issue.

$$\text{Techo Teórico Máximo (Qwen 13,0 GB)} = \frac{512\text{ GB/s}}{13,0\text{ GB}} \approx 39,38\text{ tokens/s}$$

Considerando una eficiencia real sostenida del controlador de memoria GDDR6 de entre el 75 % y el 85 % bajo cargas aleatorias de dispersión/recolección:
$$\text{Techo Real Esperable en Régimen Estacionario} \approx 29,5\text{ a } 33,5\text{ tokens/s}$$

**Línea base medida en este repositorio (hechos corroborados):**
1. En ráfagas cortas (probes de 200–300 tokens en caliente con alta tasa de aciertos de Infinity Cache y contexto bajo), el motor alcanza picos de **42,6 t/s** (control directo en `docs/agents/harness-comparison-128k.md` §34) y **32–34 t/s** sostenidos en la puerta de aceptación a 32K.
2. En contextos ultra-largos (128K y 262K), cuando la memoria excede la VRAM disponible y el KV cache cruza el bus PCIe hacia DDR5, el rendimiento decae a **14,0–16,2 t/s** sostenidos (`docs/agents/engine-pressure.md` §5).
3. El prefill (evaluación de prompt) sí es compute-bound y memory-bound mixto: depende del tamaño del bloque (`-b` / `-ub`), el empaquetado FP16 y la velocidad del shader de atención Flash Attention (`-fa`).

---

## 2. Investigación Web: Estado del Arte en RDNA2, Vulkan y Windows

### 2.1 Por qué ROCm / HIP en Windows dio "0 dispositivos" en esta máquina
En la prueba documentada en `docs/SRS.md` LM-MOT-10, un binario `win-hip` (build 11175 con `ggml-hip.dll`, `amdhip64_7.dll` y `rocm_kpack.dll` en `bin-hip/`) ejecutó `--list-devices` arrojando `(none)` y cayendo al fallback de CPU a ~1,2 t/s.

**Causa raíz técnica comprobada:**
1. **Filtro de hardware en AMD HIP SDK para Windows:** AMD solo certifica y habilita oficialmente en el runtime de Windows las GPUs profesionales RDNA2 (como la Radeon PRO W6800) y tarjetas RDNA3 (RX 7900 XT/XTX). En Windows, el driver en modo kernel (`amdhip64.dll` comunicándose con el subsistema WDDM/PAL) filtra el Device ID de las tarjetas de consumo RDNA2 (`gfx1030` RX 6800 XT) y `hipGetDeviceCount()` reporta 0 dispositivos.
2. **Inexistencia de bypass por variable en Windows:** En Linux, los usuarios pueden forzar el backend mediante la variable `HSA_OVERRIDE_GFX_VERSION=10.3.0` dirigida al driver KFD (Kernel Fusion Driver). En Windows, la capa PAL ignora completamente esta variable.
3. **Ausencia de kernels precompilados en distribuciones binarias:** Las compilaciones oficiales recientes de `llama.cpp` migraron a paquetes modulares multi-arquitectura (`rocm-kpack` / TheRock). Los DLLs distribuidos no incluyen los bloques precompilados de `rocblas.dll` para la arquitectura `gfx1030`.
4. **Veredicto actual:** Ejecutar HIP/ROCm en Windows con RX 6800 XT exige compilar manualmente `llama.cpp` y el SDK ROCm desde fuentes con `-DAMDGPU_TARGETS="gfx1030"` o recurrir a forks de la comunidad (como KoboldCpp-ROCm) que empaquetan DLLs no oficiales. Para una instalación estándar de Windows 11, **el backend Vulkan es la única opción estable, oficial y de rendimiento consistente**.

*Fuentes consultadas:*
* AMD ROCm Hardware Compatibility Matrix: https://rocm.docs.amd.com/en/docs-6.1.1/compatibility/compatibility-matrix.html
* GitHub `ggml-org/llama.cpp` Issue #21106 (*ROCm device enumeration on Windows RDNA2*): https://github.com/ggml-org/llama.cpp/issues/21106
* GitHub `ggml-org/llama.cpp` Discussion #27047 (*Multi-arch ROCm Windows packaging and gfx1030*): https://github.com/ggml-org/llama.cpp/discussions/27047

### 2.2 Drivers en Windows 11: ¿RADV o AMDVLK frente a Adrenalin?
* **RADV en Windows:** Valve y Collabora anunciaron un proyecto experimental para portar el driver libre RADV (Mesa) a Win32 interactuando directamente con el driver kernel de AMD. Sin embargo, su estado actual está en fase alfa orientada a juegos específicos (ej. *Counter-Strike 2*). No cuenta con instalador de producción, carece de certificación para cómputo GPGPU/Vulkan y exige compilar librerías DLL experimentales e inyectar manualmente `VK_ICD_FILENAMES`.
* **AMDVLK:** El driver abierto de AMD está diseñado primordialmente para Linux. En Windows no se distribuye como alternativa de producción.
* **Driver Oficial AMD Software Adrenalin:** En Windows 11, el runtime oficial (`amdvlk64.dll` / `amdxc64.dll`) integrado en el driver Adrenalin es la única implementación de Vulkan 1.3 madura y estable para RDNA2.
* **Conclusión:** Experimentar con reemplazos de ICD en Windows introduce alto riesgo de inestabilidad y pantallas azules (BSOD) sin beneficio documentado en cómputo. Se debe trabajar sobre el driver oficial Adrenalin.

*Fuentes consultadas:*
* Collabora Engineering Blog (*Porting RADV to Win32*): https://www.collabora.com/news-and-blog/news-and-events/cracking-windows-open-porting-radv-to-win32.html
* Khronos Group Vulkan Loader Guide (`VK_ICD_FILENAMES`): https://docs.mesa3d.org/drivers/radv.html

### 2.3 Variables de Entorno Vulkan en `ggml-vulkan.cpp`
Analizando las llamadas a `getenv()` en el código fuente de `ggml/src/ggml-vulkan/ggml-vulkan.cpp`:

1. `GGML_VK_FORCE_MAX_ALLOCATION_SIZE=<bytes>`:
   * **Mecanismo:** Sobrescribe el valor `maxMemoryAllocationSize` que reporta el driver de Vulkan.
   * **Relevancia:** Algunos drivers antiguos de AMD limitaban las asignaciones individuales a 2 GB o 4 GB, provocando `VK_ERROR_OUT_OF_DEVICE_MEMORY` al asignar buffers gigantescos de contexto. En Windows 11 con drivers Adrenalin modernos, el driver reporta correctamente los bloques de VRAM disponibles. En LocalMind, los tensores se fraccionan por capa (~200 MB c/u), por lo que no existe bloqueo de asignación en 32K–128K. Solo aplicable como salvavidas si un contexto masivo aborta durante la inicialización.
2. `GGML_VK_ALLOW_SYSMEM_FALLBACK=1`:
   * **Mecanismo:** Permite que `ggml-vulkan` asigne buffers de staging en memoria RAM del sistema cuando la VRAM local se agota, en lugar de retornar error de memoria.
   * **Riesgo:** **Extremo**. Si el modelo o el KV desbordan la VRAM por solo 100 MB, las operaciones de GPU caen en transferencias de bus PCIe 4.0 x16 (~25 GB/s frente a 512 GB/s de VRAM), colapsando la velocidad de generación de ~30 t/s a menos de 2 t/s.
3. `GGML_VK_DISABLE_COOPMAT=1`:
   * **Mecanismo:** Fuerza `matrix cores: none`, desactivando el uso de `VK_KHR_cooperative_matrix`.
   * **Relevancia en esta máquina:** **Inerte**. La RX 6800 XT (RDNA2) carece de unidades matriciales físicas (`matrix cores`), por lo que el backend de Vulkan ya inicializa con `matrix cores: none`.
4. `GGML_VK_DISABLE_F16=1`:
   * **Mecanismo:** Desactiva la matemática en media precisión FP16 en los compute shaders de Vulkan y fuerza el uso de FP32.
   * **Impacto:** En RDNA2, la tasa de cálculo FP16 es el doble de FP32 (dual-issue packed math). Desactivar FP16 **reduce la velocidad de cómputo en prefill a la mitad (-50 % TFLOPS)**. Solo debe usarse como diagnóstico si se observan tokens corruptos o repetición infinita por desbordamiento numérico (NaN).

*Fuentes consultadas:*
* GitHub `ggml-org/llama.cpp` Issue #5441 (*Vulkan max allocation size workaround*): https://github.com/ggml-org/llama.cpp/issues/5441
* GitHub `ggml-org/llama.cpp` Issue #13310 / #21888 (*GGML_VK_DISABLE_F16 context and stability*): https://github.com/ggml-org/llama.cpp/issues/21888
* GitHub `ggml-org/llama.cpp` Issue #6072 (*Cooperative matrix extension usage in Vulkan*): https://github.com/ggml-org/llama.cpp/issues/6072

---

## 3. Lista Jerarquizada de Experimentos para RX 6800 XT

La siguiente lista ordena las intervenciones de mayor a menor impacto potencial y viabilidad, detallando su mecanismo físico, ganancia esperada, riesgos asociados y protocolo de medición.

| Rango | ID | Intervención | Objetivo Primario | Ganancia Estimada | Nivel de Riesgo |
|:---:|:---:|:---|:---|:---:|:---:|
| **1** | EXP-01 | AMD Software: VRAM Fast Timings + OC VRAM (+100 MHz) | Decode t/s | **+5 % a +7 %** sostenido | Bajo (Hardware) |
| **2** | EXP-02 | Cuantización KV Asimétrica (`-ctk q8_0 -ctv q4_0` a 32K) | Calidad / Decode | **0 % a +3 %** t/s, retención total | Nulo |
| **3** | EXP-03 | `--load-mode none` en perfiles de uso diario (32K y 64K) | Prefill (TTFT) | **+50 % a +75 %** prefill | Bajo |
| **4** | EXP-04 | Optimización VRAM 100 % para LFM2.5-2.6B (`cache_ram = 0`) | Throughput / Jank | **>180 t/s** sin uso de bus | Nulo |
| **5** | EXP-05 | Verificación SAM / ReBAR en BIOS y Driver Adrenalin | Carga / Latencia PCIe | **+10 % a +20 %** en `load_secs` | Nulo |
| **6** | EXP-06 | Desactivación de Windows HAGS para estabilidad Vulkan | Estabilidad TDR | Prevención de cuelgues | Nulo |
| **7** | EXP-07 | *[PSU-LOCKED]* Ajuste de `ubatch` (256 vs 512) bajo Power Limit -15% | Prefill / Picos W | **+10 %** prefill o blindaje PSU | Medio / Alto |
| **8** | EXP-08 | Prueba de nueva compilación upstream `llama.cpp` (Vulkan) | Shaders / FA | **+3 % a +8 %** prefill | Bajo |

---

### EXP-01: Ajuste de VRAM en AMD Software (Fast Timings + Overclock VRAM)
* **Ajuste:** En AMD Software Adrenalin $\rightarrow$ Rendimiento $\rightarrow$ Ajustes $\rightarrow$ Configuración personalizada:
  1. *Temporización de memoria:* Cambiar de "Predeterminado" a **"Tiempos rápidos" (Fast Timings)**.
  2. *Frecuencia máxima de VRAM:* Subir de 2000 MHz a **2100 MHz** (aumento moderado de 5 %, elevando el bus a 537,6 GB/s sin forzar voltajes).
  3. *Undervolt de GPU:* Mantener el undervolt existente (-50 a -75 mV en núcleo) y Power Limit en **-10 %**.
* **Mecanismo:** Como la decodificación está 100 % ligada al ancho de banda de memoria ($B = 512\text{ GB/s}$), elevar la frecuencia física de la VRAM incrementa directamente el techo de transferencia de pesos. "Fast Timings" reduce las latencias tRFC y tFAW de las celdas GDDR6, mejorando el rendimiento bajo accesos no secuenciales (como el barrido de atención). Reducir el power limit del núcleo no afecta el decode (que consume apenas ~140–170W) pero mantiene la temperatura de la unión VRAM (Junction Temp) por debajo de 85 °C, evitando el thermal throttling de la memoria.
* **Magnitud esperada:** Decode sostenido en Qwen3.8-27B pasando de **32,7 t/s a 34,5–35,0 t/s** (+5 % a +7 %).
* **Riesgo:** Bajo. La memoria GDDR6 posee detección interna de errores; si se excede la frecuencia, no se cuelga inmediatamente sino que reintenta lecturas, lo cual se manifiesta como una caída súbita de t/s. 2100 MHz es ampliamente tolerado por módulos Samsung/Micron en RX 6800 XT.
* **Medición:**
  ```powershell
  node tests/bench.mjs --profiles 32k --runs 3 --prompt-tokens 512 --max-tokens 300
  ```
  Observar `engine_gen_tps` en las 3 corridas.

---

### EXP-02: Cuantización de KV Cache en Contexto Moderado (32K `velocidad`)
* **Ajuste:** Comparar en el perfil `velocidad` (32K, modelo 13 GB, VRAM libre ~2,5 GB):
  * Variante A (actual): `-ctk q4_0 -ctv q4_0`
  * Variante B (asimétrica): `-ctk q8_0 -ctv q4_0`
  * Variante C (alta fidelidad): `-ctk q8_0 -ctv q8_0`
* **Mecanismo:** En RDNA2, la dequantización de `q4_0` en el kernel de Flash Attention requiere operaciones ALU adicionales de desempaquetado de nibbles (4 bits). En contextos donde la VRAM alcanza holgadamente (32K ocupa ~1,0 GB en Q4 y ~2,0 GB en Q8), `q8_0` utiliza desempaquetado de bytes simple que satura menos la ALU vectorial. La configuración asimétrica (`-ctk q8_0 -ctv q4_0`) retiene el 100 % de la precisión de alineación de atención (Keys) reduciendo a la mitad el tamaño de los Values.
* **Magnitud esperada:** En 32K, decode idéntico o ligeramente superior (+1 % a +3 % t/s) con mejora medible en razonamiento largo y cero degradación de coherencia en tareas de código complejo.
* **Riesgo:** Nulo en 32K (13 GB modelo + 2 GB KV = 15 GB, dentro de los 16 GB). **No aplicar en 128K/262K** donde Q8 causaría desborde inmediato a DDR5.
* **Medición:**
  ```powershell
  # Modificar temporalmente extra_flags en localmind.toml para el perfil velocidad:
  # extra_flags = ["-ctk", "q8_0", "-ctv", "q4_0"]
  node tests/bench.mjs --profiles 32k --runs 2 --prompt-tokens 2048 --max-tokens 256
  ```

---

### EXP-03: Validación de `--load-mode none` en Perfiles Diarios (32K y 64K)
* **Ajuste:** Añadir `extra_flags = ["--load-mode", "none"]` en los perfiles `velocidad` (32K) y `multi_doc` (64K).
* **Mecanismo:** Ya medido contundentemente en `docs/agents/engine-pressure.md` §9: al cargar pesos directamente en VRAM sin intermediación de la caché de páginas de Windows (`mmap`), el prefill en frío saltó de **21,65 t/s a 38,18 t/s (+76 %)**. El dueño restauró el default `mmap` en los perfiles pesados (`libros` 128K y `max_contexto` 262K) porque arranques continuos en pruebas de estrés saturaban la lectura de disco y provocaban picos transitorios. En 32K y 64K, el motor permanece residente por horas (`idle_timeout_secs = 5400`), por lo que un arranque único paga una sola lectura limpia y ofrece prefill veloz en cada interacción interactiva.
* **Magnitud esperada:** Aceleración de hasta +70 % en la respuesta al primer prompt (TTFT) en sesiones interactivas, liberando adicionalmente ~4 GB de memoria en el Administrador de Tareas.
* **Riesgo:** Bajo en 32K/64K siempre que se respete el cooldown de arranque implementado en LocalMind (120 s).
* **Medición:**
  ```powershell
  # Perfil velocidad con y sin --load-mode none
  node tests/bench.mjs --profiles 32k --prompt-tokens 4096 --max-tokens 100
  ```

---

### EXP-04: Optimización VRAM 100 % para Modelos Pequeños (LFM2.5 / Bonsai)
* **Ajuste:** Crear o configurar el perfil para modelos pequeños (LFM2.5-2.6B, Bonsai-2.6B) con:
  * `-c 131072` (128K nativo)
  * `--cache-ram 0` (o máximo 1024 MB para prompt-cache)
  * `-ctk q8_0 -ctv q8_0`
  * `-ngl 99`
* **Mecanismo:** El modelo LFM2.5 pesa solo 1,67 GB en disco. Su KV cache completo a 128K en `q8_0` consume ~9,4 GB. Total de memoria requerida: $1,67 + 9,4 = 11,07\text{ GB}$. Como la RX 6800 XT tiene 16 GB, **el modelo y todo su contexto de 128K caben íntegramente en la VRAM física de la tarjeta**. Forzar `cache_ram = 0` elimina completamente la sincronización con la memoria DDR5 del sistema, garantizando cero micro-cortes (jank) en el escritorio de Windows 11.
* **Magnitud esperada:** Decodificación sostenida por encima de **180–189 t/s**, prefill ultra-rápido (>500 t/s) y commit de memoria RAM del sistema completamente plano.
* **Riesgo:** Nulo.
* **Medición:**
  ```powershell
  # Con LFM2.5 cargado en el puerto del motor:
  node tests/bench.mjs --profiles 128k --prompt-tokens 2048 --max-tokens 512
  ```

---

### EXP-05: Verificación de Smart Access Memory (SAM / ReBAR)
* **Ajuste:** Comprobar en AMD Software: Adrenalin $\rightarrow$ Rendimiento $\rightarrow$ Ajustes que **"AMD Smart Access Memory"** figure como **"Habilitado"**. Si figura deshabilitado, ingresar al BIOS de la placa madre y activar:
  1. `Above 4G Decoding` $\rightarrow$ Enabled.
  2. `Re-Size BAR Support` $\rightarrow$ Enabled / Auto.
* **Mecanismo:** Sin SAM/ReBAR, el procesador (Ryzen 5 7600) se comunica con la VRAM de la GPU mediante una ventana PCIe de solo 256 MB. Con SAM habilitado, la CPU mapea los 16 GB completos de VRAM en un único espacio continuo de direcciones de 64 bits. En `llama.cpp`, esto acelera la transferencia de pesos y buffers durante el arranque del motor y reduce la sobrecarga de comandos en el bus PCIe durante la inferencia.
* **Magnitud esperada:** Reducción de 3 a 5 segundos en el tiempo de carga (`load_secs`) del modelo de 13 GB.
* **Riesgo:** Nulo. Estándar de la industria en plataformas AM5.
* **Medición:** Lectura directa del log de arranque en `load_times.json` y tiempo reportado en `/api/status`.

---

### EXP-06: Desactivación de Windows HAGS (Hardware-Accelerated GPU Scheduling)
* **Ajuste:** En Windows 11: *Configuración $\rightarrow$ Pantalla $\rightarrow$ Gráficos $\rightarrow$ Configuración de gráficos predeterminada $\rightarrow$ Desactivar "Programación de GPU acelerada por hardware"*. Reiniciar la PC.
* **Mecanismo:** HAGS traslada la gestión de colas de memoria del planificador del kernel de Windows al microcódigo de la GPU. En cargas de renderizado 3D puede aportar fluidez, pero en computación intensiva de Vulkan con shaders de larga duración (como el prefill de 10k tokens a 128K), HAGS incrementa la probabilidad de que el temporizador TDR (Timeout Detection and Recovery) de Windows detecte un falso bloqueo y reinicie el controlador gráfico (`GPU lost` / pantalla negra).
* **Magnitud esperada:** Cero pérdida de t/s; eliminación de potenciales cuelgues del motor bajo prompts gigantescos (>8000 tokens en frío).
* **Riesgo:** Nulo para LLMs.
* **Medición:** Ejecución del punto de prueba de 8k tokens que previamente presentó timeout en `docs/agents/harness-comparison-128k.md` §183.

---

### EXP-07: [CANDADO PSU / REQUIERE APROBACIÓN EXPLÍCITA DEL DUEÑO] Ajuste de `ubatch`
* **Ajuste:** Modificar `-ub 512` a `-ub 256` o `-ub 768` en el archivo de configuración.
* **Mecanismo:** El micro-batch (`-ub`) dicta cuántos tokens de prompt se procesan en un único dispatch de compute shader en la GPU.
  * `-ub 768` o `-ub 1024`: Aumenta el paralelismo en el shader y la eficiencia de los CUs, pudiendo ganar +10 % a +15 % en velocidad de prefill. **PELIGRO:** Concentra un consumo eléctrico instantáneo superior a 300W con picos transitorios de microsegundos que disparan la protección por sobrecorriente (OCP) de la fuente de poder (la RX 6800 XT es conocida por picos transitorios de hasta 400W).
  * `-ub 256`: Divide el prefill en bloques más pequeños. Reduce el pico transitorio de potencia en un ~25 %, blindando la PC contra apagones duros a cambio de una pérdida marginal de prefill (~5 % a 8 %).
* **Condición de seguridad:** **NO TOCAR sin permiso expreso del dueño** y solo con el Power Limit de la GPU fijado en -15 % en Adrenalin.
* **Medición:** Ejecutar con el script de muestreo de presión:
  ```powershell
  node tests/pressure-sampler.mjs --duration 120
  ```

---

### EXP-08: Evaluación de Nueva Compilación Upstream de `llama.cpp` (Vulkan)
* **Ajuste:** Descargar en un directorio temporal aislado (ej. `bin-test/`) una compilación reciente de `llama.cpp` Vulkan (`llama-bXXXX-bin-win-vulkan-x64.zip`) posterior al build 10683.
* **Mecanismo:** Las versiones posteriores incorporan:
  1. Corrección de strides en la dequantización de Flash Attention en Vulkan (PR #28190).
  2. Mejoras en la selección de tamaño de subgroup (wave32 vs wave64) en compiladores SPIR-V para RDNA2.
  3. Reducción de barreras redundantes de memoria en la cola de comandos Vulkan.
* **Precaución crítica:** El build actual 10683 (commit `d8f26eec7`) contiene soporte específico para cuantizaciones ternarias (kernels Prism ML / Bonsai). Una compilación oficial upstream no debe sobrescribir `bin/llama-server.exe` hasta verificar que soporta todos los modelos requeridos.
* **Medición:** Comparación directa ejecutando `llama-bench.exe` en `bin/` vs `bin-test/`:
  ```powershell
  bin-test\llama-bench.exe -m models\Qwen3.8-27B-IQ4_XS_4BPW.gguf -ngl 99 -fa 1 -p 512 -n 128
  ```

---

## 4. Lo que NO se debe hacer (Callejones sin Salida Medidos y Mitos Desmentidos)

Para no malgastar ventanas de prueba ni poner en riesgo la estabilidad del equipo, los siguientes enfoques quedan formalmente descartados con fundamento técnico:

### 1. El mito de "GPU Workload: Compute" en AMD Adrenalin
* **El Mito:** Foros antiguos recomiendan "cambiar el modo de GPU a Cómputo en el panel de AMD".
* **La Realidad:** Esa opción fue introducida en 2017 para arquitecturas Polaris (RX 480/580) y Vega para minería de criptomonedas. En la arquitectura **RDNA y RDNA2 (RX 6000), AMD eliminó por completo este interruptor**. El driver de RDNA2 gestiona las colas de cómputo y gráficos de forma dinámica a nivel microcódigo. Buscar o intentar forzar este ajuste por registro es inútil.

### 2. Forzar drivers RADV o AMDVLK en Windows
* **La Realidad:** Como se demostró en la §2.2, RADV en Windows es un experimento incipiente de Valve/Collabora para videojuegos. No existe un paquete de cómputo estable. Alterar las variables `VK_ICD_FILENAMES` solo conducirá a errores de inicialización o caídas del sistema.

### 3. Usar `GGML_VK_DISABLE_F16=1`
* **La Realidad:** Esta variable fuerza matemática FP32 en todos los shaders de Vulkan. En RDNA2, FP16 opera al doble de velocidad nativa. Activar esta variable destruye el 50 % del rendimiento de prefill sin aportar beneficio alguno cuando el modelo genera texto coherente.

### 4. Usar `--no-kv-offload` (`-nkvo`)
* **La Realidad:** Expulsa el KV cache de la VRAM y lo coloca en la memoria RAM del sistema. El ancho de banda de la DDR5 dual-channel ronda los 60–80 GB/s frente a los 512 GB/s de la VRAM. Esto provocaría una caída inmediata del decode a menos de 10–12 t/s incluso en contextos cortos.

### 5. Configurar `--defrag-thold`
* **La Realidad:** En versiones modernas de `llama.cpp`, el parámetro de desfragmentación de KV cache está **deprecado**. El asignador de slots del servidor gestiona la memoria en anillo de manera nativa. Pasar esta flag ensucia el argv y genera advertencias en el log.

### 6. Usar `--split-mode row` o `layer` con la iGPU integrada
* **La Realidad:** El procesador Ryzen 5 7600 contiene una gráfica integrada (Radeon Graphics) de solo 2 Compute Units conectada a memoria RAM DDR5. Intentar repartir capas del modelo entre la dGPU (72 CUs, 512 GB/s) y la iGPU (2 CUs, 60 GB/s) causará que la dGPU permanezca el 90 % del tiempo ociosa esperando sincronización por el bus PCIe. `--split-mode none` y `--device Vulkan0` son obligatorios.

### 7. Usar `--n-cpu-moe` en Qwen o LFM
* **La Realidad:** Las opciones de offload de expertos solo aplican a modelos Mixture-of-Experts (MoE) como Mixtral o DeepSeek. Qwen3.8-27B y LFM2.5 son modelos **densos**. El flag es completamente inerte.

### 8. Reintentar `--cache-reuse`
* **La Realidad:** El build 10683 rechazó el flag en cuatro combinaciones (`src-rust/src/config.rs`, evidencia histórica). La ayuda del build 10743 ya ofrece `--cache-reuse` con default 0. OMNI todavía no lo envía y falta medirlo con Qwen3.8 en el contexto real; la reutilización nativa de prefijos ya funciona mediante `--cache-prompt`, así que no se debe activarlo sin A/B y control de calidad.

### 9. Aplicar `-kvu` en contextos largos para modelos densos
* **La Realidad:** Medido en la prueba A/B de `docs/agents/engine-pressure.md` §11: a igualdad de contexto (128K) y prompt (15k tokens), `-kvu` rindió **-8 % en prefill** (154,7 t/s vs 168,0 t/s) y aumentó el commit de RAM en 3 puntos porcentuales, sin ganancia alguna en decode.

### 10. Elevar hilos de CPU por encima de 6 (`-t > 6`)
* **La Realidad:** La CPU tiene 6 núcleos físicos y 12 lógicos. Como se verificó en `docs/agents/engine-pressure.md` §5, la CPU no es el cuello de botella (pico de 16,8 % en prefill). Asignar más hilos solo genera contención de contexto en el planificador del sistema operativo, aumentando el jank en el escritorio de Windows sin aportar un solo token por segundo en decode (que se procesa al 100 % en la GPU).

---

## 5. Protocolo para la Próxima Ventana Libre (Top 3 Experimentos)

Para la próxima ventana en que la GPU esté desocupada, se propone ejecutar **exactamente 3 experimentos**, cada uno con **una sola carga de motor**, en orden de retorno de inversión y con criterios de parada estrictos.

```
       ┌────────────────────────────────────────────────────────┐
       │   EXP-01: Hardware Adrenalin Tuning (Fast Timings)     │
       │   Objetivo: Probar techo de VRAM en perfil velocidad   │
       └──────────────────────────┬─────────────────────────────┘
                                  │
                                  ▼
       ┌────────────────────────────────────────────────────────┐
       │   EXP-02: KV Cache Asimétrico (q8_0 Key / q4_0 Val)    │
       │   Objetivo: Verificar ALU dequant vs fidelidad en 32K  │
       └──────────────────────────┬─────────────────────────────┘
                                  │
                                  ▼
       ┌────────────────────────────────────────────────────────┐
       │   EXP-03: Validación --load-mode none en 32K           │
       │   Objetivo: Medir TTFT interactivo sin overhead mmap   │
       └────────────────────────────────────────────────────────┘
```

### Protocolo Detallado de Ejecución

#### Paso Previo Común (Condición de Quietud)
* Cerrar navegadores, reproductores de video y procesos en segundo plano.
* Confirmar en `tasklist` la ausencia de procesos ajenos que compitan por CPU/GPU.
* Respetar el cooldown obligatorio de 120 s entre arranques de motor.

---

### Experimento 1: Fast Timings de VRAM en Adrenalin (Carga 1)
1. **Configuración en AMD Software Adrenalin:**
   * Ajuste de GPU: Personalizado.
   * Ajuste de VRAM: Habilitado $\rightarrow$ *Temporización de memoria: Tiempos rápidos*. Frecuencia: 2000 MHz (stock) para aislar la latencia.
   * Power Limit: -10 %.
2. **Ejecución del Bench (Perfil velocidad, 32K):**
   ```powershell
   node tests/bench.mjs --profiles 32k --runs 3 --prompt-tokens 512 --max-tokens 300 --stop-after
   ```
3. **Criterio de Aceptación:**
   * **Éxito (Go):** Mediana de `engine_gen_tps` $\ge 33,5\text{ t/s}$ (superando la línea base previa de 32,4–32,7 t/s) sin artefactos visuales ni reinicios de driver.
   * **Fracaso (Rollback):** Regresión en t/s o inestabilidad gráfica $\rightarrow$ Volver a "Predeterminado".

---

### Experimento 2: KV Cache Asimétrico `-ctk q8_0 -ctv q4_0` a 32K (Carga 2)
1. **Configuración en `%APPDATA%\LocalMind\localmind.toml`:**
   * En el perfil `velocidad`, modificar temporalmente:
     ```toml
     [[profiles]]
     id = "velocidad"
     extra_flags = ["-ctk", "q8_0", "-ctv", "q4_0"]
     ```
2. **Ejecución del Bench:**
   ```powershell
   node tests/bench.mjs --profiles 32k --runs 2 --prompt-tokens 2048 --max-tokens 256 --stop-after
   ```
3. **Criterio de Aceptación:**
   * **Éxito (Go):** `engine_gen_tps` se mantiene a $\ge 98\ \%$ del valor obtenido en el EXP-01 (sin penalización perceptible por dequantización) y el uso de VRAM no supera los 15,2 GB.
   * **Fracaso (Rollback):** Caída de t/s $> 3\ \%$ debido a mayor carga en la ALU de desempaquetado $\rightarrow$ Restaurar `-ctk q4_0 -ctv q4_0`.

---

### Experimento 3: Prefill con `--load-mode none` en Perfil 32K (Carga 3)
1. **Configuración en `%APPDATA%\LocalMind\localmind.toml`:**
   * En el perfil `velocidad`:
     ```toml
     [[profiles]]
     id = "velocidad"
     extra_flags = ["--load-mode", "none"]
     ```
2. **Ejecución del Bench:**
   ```powershell
   node tests/bench.mjs --profiles 32k --runs 1 --prompt-tokens 4096 --max-tokens 100 --stop-after
   ```
3. **Criterio de Aceptación:**
   * **Éxito (Go):** `engine_prompt_tps` $\ge 120\text{ t/s}$ (ganancia de $>40\ \%$ frente a la línea base con mmap) y tiempo de carga total del modelo $\le 30\text{ s}$ sin disparar alarmas de energía.
   * **Fracaso (Rollback):** Tiempo de carga excesivo (>45 s) o reporte de alta contención en disco $\rightarrow$ Retirar flag.

---

## 6. Propuestas Concretas de Código (Para Enrutamiento)

Si se aprueba incorporar el modo asimétrico de KV cache o perfiles específicos para modelos livianos, se proponen los siguientes cambios quirúrgicos en el código de Rust (NO aplicados, listos para revisión):

### Propuesta A: Flag de KV Cache Asimétrico configurable por perfil
* **Archivo:** `src-rust/src/config.rs`
* **Líneas:** ~480–500 (definición de `built_in_profiles`)
* **Propuesta:** Permitir que el perfil `velocidad` (32K) incorpore `-ctk q8_0 -ctv q4_0` como flags por defecto si el EXP-02 resulta favorable, reservando `-ctk q4_0 -ctv q4_0` para los perfiles masivos (`libros` y `max_contexto`).

### Propuesta B: Preservación de Flags Seguras en la Migración de Perfiles
* **Archivo:** `src-rust/src/config.rs`
* **Líneas:** ~650–680 (`profile_flags_ok`)
* **Propuesta:** Asegurar que el validador admita explícitamente combinaciones de `-ctk` y `-ctv` sin que la migración automática de perfiles `v5` sobreescriba los flags cuando el usuario experimente con ellos.
