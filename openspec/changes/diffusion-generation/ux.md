# `generate` — Diseño de experiencia

> Fase: **diseño de UX**. Fecha: 2026-09-30. Cambio: `diffusion-generation`.
> **No implementado.** `ui.html` no se toca con este artefacto. La prosa va en español;
> los identificadores, rutas de API y nombres de fichero van en inglés.
> **Este documento decide**, no propone. Donde algo queda abierto, está marcado como pregunta
> para el dueño y no como tarea.

---

## 0. La tesis

La especificación actual de `image-generation-iteration` tiene 20 escenarios y describe **lo que
el motor sabe hacer**: posiciones de cola, respuestas de `capabilities`, recuentos de lote, tipos
de upscaler. Es una especificación técnica con un placeholder de UX pegado al final.

Eso está al revés. El orden correcto es: **primero qué quiere hacer la persona, después qué tiene
que exponer el motor para que eso sea posible.**

El resultado de invertirlo es este documento, y termina en una lista corta de huecos reales
(§8) — de los cuales el más importante es uno que la especificación actual no anticipa: **el
motor no permite cancelar un trabajo que ya está generando**, y por tanto el botón "Cancelar"
que la UX natural quiere ofrecer sería un botón que falla.

Tres frases resumen el diseño:

1. **El generador es un electrodoméstico, no un chat.** Se enciende, tarda, y mientras está
   encendido el chat no existe. La UI debe comportarse como la de un electrodoméstico: decir
   cuánto tarda **antes**, y no sorprender al usuario después.
2. **El usuario no escribe semillas.** Escribe texto y ve una imagen. La semilla existe, se
   guarda y se muestra, pero la acción del usuario es **"Volver a esta"**, no "introducir 12345".
3. **El libro de semillas no tiene nombre en la UI.** Su cara visible es un botón. Si el
   ledger aparece con nombre propio en pantalla, el usuario tiene que aprender un concepto que
   no le sirve para nada.

---

## 1. Lo que el diseño da por cierto (verificado, no supuesto)

Nada de esta sección es opinión; es el contrato que el resto del documento diseña alrededor.

| Hecho | Dónde está verificado |
|---|---|
| La GPU es un recurso único. El LLM (~6 GB) y Qwen-Image-2.1 (~13 GB) no coexisten. | `design.md` §3, medido sobre 16 GB |
| Cambiar de modelo exige **reiniciar el proceso**: `new_sd_ctx` se construye antes de `svr.listen` y no hay endpoint de recarga | `exploration.md`, `routes_sdcpp.cpp` |
| `seed` es **request-only**: ni el trabajo ni ningún endpoint devuelven la semilla usada. `-1` la pierde para siempre | `api.md`, `routes_sdcpp.cpp`; obs 1213 |
| `capabilities` anuncia `cancel_generating` pero `make_*_features_json` lo fija a **`false`**: solo se cancelan trabajos **en cola**. Un trabajo en curso devuelve `409` | `make_img_gen_defaults_json`; obs 1213 |
| `GET /jobs/{id}` da `status`, `queue_position` y `result`. **No hay progreso intra-trabajo**, ni `step`, ni `eta` | obs 1213 |
| `sd-server` no tiene historial ni galería; su UI embebida tampoco (`c.history` / `c.gallery` = 0 coincidencias en `sdcpp-webui`) | obs 1212 |
| `qwen-image-2.1` es el **único** preset de Nivel 1 y por tanto el default. No hay fallback permisivo | `presets.rs:641`, spec `image-generation-iteration` |
| La UI **no usa modales**: *"All user-facing errors go here; the UI never blocks on a modal dialog"* | `ui.html:2621-2622` |
| La UI es **framework-free**, con i18n `I18N.es` / `I18N.en` y `data-i18n` | `ui.html:2111-2540` |
| El estado de esta máquina **hoy**: `bin/sd-cpp/` existe (21 ficheros, `sd-server.exe` de 1,15 MB). `models/diffusion/` **no existe** | inspección del disco |

Esa última fila reordena el problema: **el estado de primera ejecución real en esta máquina no es
"motor no instalado"**, es **"motor instalado, pesos ausentes"**. Las dos pantallas se diseñan
igualmente, pero la segunda es la que el dueño verá primero, y es la que por tanto se diseña con
más detalle.

---

## 2. La pantalla

### 2.1 Dónde vive la pestaña

Barra de navegación, **entre Chat y Modelos**:

```
Panel · Chat · **Generar** · Modelos · Uso · Logs · Specs · Ajustes
```

Motivo: es la segunda cosa que se abre después del Panel, y agruparla con Chat separa
"hablar con el modelo" de "producir una imagen". Nada más cambia en la barra.

Claves i18n: `nav.generate` = `Generar` / `Generate`. Panel `#tab-generate`, `role="tabpanel"`,
`aria-labelledby="nav-generate"` — igual que las otras siete.

Y el recordatorio que no es opcional: `"generate"` hay que añadirlo a `TABS` en
`tests/ui-tabs.mjs:25`, hoy hardcodeado a siete. Si no, el arnés sigue verde sin cubrir la
pestaña nueva.

### 2.2 Estructura

Dos columnas con el grid que ya existe (`.dashboard-layout` / `.dash-col`, usado por Panel).
Izquierda fija y estrecha: se escribe. Derecha ancha: se mira.

```
┌─ Generar ─────────────────────────────────────────────────────────────┐
│ Escribe lo que quieres ver. Tarda minutos, no segundos.                │
├──────────────────────────────┬────────────────────────────────────────┤
│ [modelo + licencia]          │  Resultado                             │
│  Qué quieres ver             │  [imagen | vacío | generando | error]  │
│  [textarea]                  │                                        │
│  [Qué NO quieres ver ▾]      │  [Volver a esta] [Descargar] [Ampliar]│
│                              │  ▸ Cómo se generó                      │
│  Tamaño   [Cuadrado 768 ▾]   │                                        │
│  Imágenes [1 ▾]              ├────────────────────────────────────────┤
│                              │  Tus imágenes                          │
│  semilla 12345 · [otra]      │  [thumb][thumb][thumb][thumb] …        │
│                              │  ← scroll                              │
│  ┌────────────────────────┐  │                                        │
│  │ Generar                │  │                                        │
│  └────────────────────────┘  │                                        │
│  Tarda ~2 min.               │                                        │
│  <línea de coste de GPU>     │                                        │
│  ▸ Detalles de la generación │                                        │
└──────────────────────────────┴────────────────────────────────────────┘
```

Clases que ya existen y se reutilizan tal cual — **no se crea vocabulario visual nuevo**:
`.section-shell` `.section-head` `.section-title` `.section-sub` `.dashboard-layout` `.dash-col`
`.card` `.card-header` `.card-title` `.card-meta` `.card-body` `.field-group` `.field-label`
`.field-hint` `.empty-state` `.error-banner` `.btn` `.btn-primary` `.btn-secondary` `.btn-danger`
`.tool-btn` `.model-badge` `.spinner` `.info-card` `.info-layout`.

Las únicas clases nuevas que hacen falta son tres, y son tres porque son tres cosas nuevas de
verdad: `.thumb-strip` (la tira), `.img-result` (el marco de la imagen) y `.cost-line`
(el bloque de coste + estimación). Nada más.

### 2.3 La copia, de arriba abajo, literal

Todo esto va en `I18N` **dos veces**, `es` y `en`. El par es la convención existente
(`ui.html:2111`); añadir solo `es` deja la pestaña a medio traducir cuando el usuario cambia de
idioma, y **ninguna puerta actual lo detecta** (ver §10).

**Cabecera de la pestaña**
```
Generar
Escribe lo que quieres ver. Tarda minutos, no segundos.
```

**Chips de modelo** — arriba de la columna izquierda
```
Modelo: Qwen-Image-2.1    [ no comercial ]
Qwen-Image-2.1 · licencia no comercial: no la uses para trabajo ni para contenido que monetices.
Ver la licencia
```

La segunda línea es **corta a propósito**. No es el texto legal completo —ese vive en
`docs/licencias.md` y en "Ver la licencia"—; es el aviso que se ve sin hacer nada. Su trabajo es
que nadie pueda decir después que no sabía.

**Campo de descripción**
```
Qué quieres ver
Un escritorio de madera con un vaso de cristal y luz de tarde…
```

**Prompt negativo, plegado**
```
Qué NO quieres ver            ▾
texto, marcas de agua, caras deformadas…
```

"Qué NO quieres ver" y no "Prompt negativo", porque el usuario no sabe lo que es un prompt
negativo y sí sabe qué es no querer eso. El término técnico queda en el `title` del campo, para
quien lo busque.

**Tamaño** — cuatro presets fijos, todos por debajo del techo declarado
```
Tamaño
  Cuadrado 768 · 768 × 768
  Vertical 640 · 640 × 896
  Horizontal 640 · 896 × 640
  Panorámico 448 · 896 × 448
```

**Número de imágenes**
```
Imágenes
1 · 2 · 3 · 4
Varias imágenes con la misma descripción y semillas distintas.
```

El máximo sale de `capabilities.limits.max_batch_count`, nunca de un número escrito en el
cliente. Si el motor bajase el tope, la lista baja sola.

**Semilla**
```
semilla 12345        [otra]
Si cambias cualquier otra cosa, solo cambia esa cosa. Si cambias la semilla, cambia todo.
```

**Botón**
```
Generar
Tarda ~2 min.
```

**Detalles de la generación** (`<details>`, plegado por defecto)
```
▸ Detalles de la generación
   prompt negativo · sampler · scheduler · pasos · cfg · clip_skip ·
   semilla · modelo · upscaler aplicado
```

**Columna derecha — resultado**
```
Resultado
14:32 · 2 min 10 s

[ la imagen ]

[Volver a esta]  [Descargar]  [Ampliar]

▸ Cómo se generó
```

**Tira**
```
Tus imágenes
[thumb][thumb][thumb][thumb][thumb] …
```

Sin estado vacío con texto propio: el `empty-state` de la columna derecha ya dice
`Todavía no hay imágenes. La primera aparecerá aquí.` y una tira vacía con doce huecos
es ruido.

---

## 3. El flujo principal

1. **Abrir la pestaña.** El estado real del motor se resuelve antes de pintar nada. Si el motor
   ya está en marcha y es dueño de la GPU, se ve la pantalla de listo, con su tira y su último
   resultado. Si no, se ve una pantalla de estado (§4) y **no** un formulario muerta.

2. **Escribir.** Un `textarea`. Ni palabras clave, ni estilos separados, ni campos de samplers.
   El campo se llama "Qué quieres ver" y su placeholder es una frase completa, porque así es
   como la gente describe una imagen.

3. **Elegir tamaño e imágenes.** Dos desplegables, cuatro opciones cada uno. Nada más.

4. **Pulsar `Generar`.** En este instante ocurre, sin diálogo y sinmodal, lo que está
   documentado en §5: si el chat tiene la GPU, se cede; si un agente está trabajando, se le
   ofrece esperar o detener; y en todo caso se ve la línea de coste que explica qué va a pasar.

5. **Ver el estado de espera** (§4.4). No un spinner mudo: tiempo transcurrido, y una frase que
   explica que puede irse.

6. **Ver la imagen.** Con su duración, su hora y tres acciones.

7. **Refinar.** La imagen que más le gusta se queda en la tira. Se pulsa **"Volver a esta"** en
   cualquiera de sus dos sitios —la acción bajo la imagen, o la tarjeta de la tira— y el
   compositor se repone entero: texto, tamaño, número, semilla. Se cambia una palabra y se
   vuelve a generar. La primera imagen **no se mueve**: entra una nueva al lado.

Ese paso 7 es el producto entero. Todo lo demás existe para que sea posible sin pensar.

---

## 4. Los nueve estados

Una sola región de estado, siempre en el mismo sitio (arriba de la columna izquierda, encima
del compositor). Nunca un modal, nunca un `toast` para algo que el usuario tiene que leer. El
`toast` —`ui.html:2623`, 4,5 s— queda solo para errores de un segundo, no para estado.

| # | Estado | Título en pantalla | Acción única |
|---|---|---|---|
| 1 | Motor no instalado | El generador no está instalado | Instalar el generador |
| 2 | Motor instalado, sin pesos | Faltan los pesos del modelo | Descargar los pesos |
| 3 | Listo | — (formulario normal) | Generar |
| 4 | Generando | Generando… | Dejar terminar |
| 5 | Terminada | — (la imagen) | Volver a esta |
| 6 | Falló | No se pudo generar | Reintentar con la misma semilla |
| 7 | El chat tiene la GPU | El chat está usando la GPU | Generar igualmente |
| 8 | Hay un agente corriendo | OMP está trabajando | Esperar a que termine |
| 9 | Licencia no comercial | (línea permanente, no estado) | Ver la licencia |

Los estados 7 y 8 **no son pantallas**: son el estado 3 con la línea de coste cambiada (§5). Se
listan aparte porque son los que el dueño **nombró** en el encargo y porque el tratamiento es
distinto en cada dirección.

### 4.1 Motor no instalado

> **El generador no está instalado**
>
> LocalMind necesita un motor de imagen aparte del que usa para el chat. Son unos 30 MB.
>
> `bin/sd-cpp/sd-server.exe` no está.
>
> **[Instalar el generador]**   [Ver cómo]

Una sola acción primaria. La descarga reusa el patrón que ya existe en Modelos
(`models.dlTitle`, barra de progreso con porcentaje y ETA). El `Ver cómo` es texto, no botón:
es una explicación, y un botón de explicación es ruido.

Este estado **no es el de esta máquina** —`bin/sd-cpp/` está instalado— pero sí es el de una
instalación nueva, y se diseña igual.

### 4.2 Motor instalado, sin pesos · **el estado real de hoy**

> **Faltan los pesos del modelo**
>
> El motor está instalado y funciona. Lo que falta es el modelo en sí: son unos 20 GB.
>
> ```
> Falta el componente «diffusion_model»
>   Busqué en  models\diffusion\
> Falta el componente «text_encoder»
>   Busqué en  models\diffusion\
> ```
>
> Necesitas **20,4 GB** libres en `C:`.
>
> **[Descargar los pesos]**   [Ya los tengo: elegir carpeta]

La lista de componentes que faltan **no es decorativa**: reutiliza literalmente el bloque que
`presets.rs` ya sabe renderizar (`validate_preset` → `MissingComponent::message()`,
`design.md` §5.6). Que la UI tenga que traducir ese mensaje es señal de que falta una capa, no de
que sobre una.

La segunda acción —"Ya los tengo"— es obligatoria y va al mismo peso visual que la primera. El
dueño puede tener ya los pesos en un disco externo, y obligarle a descargarlos sería la decisión
más insultante posible en una app local.

### 4.3 Listo

El formulario de §2.3, la tira, y el último resultado si lo hay. Sin banner, sin anuncio. Si todo
va bien, la UI **no dice que va bien**: aparece callada, que es lo que hace una app que funciona.

El único elemento no vacío es la línea bajo el botón:
```
Tarda ~2 min.
```

Y si aún no hay historial suficiente para estimar (§8, G5):
```
Tarda minutos.
```

Un número inventado sería una mentira, y un minuto sin generar es exactamente cuando más caro
sería mentir.

### 4.4 Generando

```
Generando…
00:47

Puedes cambiar de pestaña. Si cierras LocalMind, esta imagen se pierde —
pero su texto y su semilla quedan guardados, y repetirla da exactamente lo mismo.

                                    Dejar terminar
```

Tres decisiones detrás de ese texto:

**Tiempo transcurrido, no porcentaje.** El motor no lo da (§8, G3). Un `47 s` es un hecho; un
`62 %` sería una interpolación inventada. Cuando exista progreso real se añadirá una segunda
línea sin tocar nada de esto.

**"Dejar terminar", no "Cancelar".** Esta es la decisión más contraintuitiva del documento y la
que máspellizco justifica. El motor **no puede cancelar un trabajo que ya está generando**: lo
dice con un `409`. Un botón "Cancelar" aquí sería un botón que funciona una vez de cada dos y
que, la mitad de las veces, deja al usuario creyendo que paró algo que sigue corriendo. El
usuario merece un botón cuyo rótulo describa lo que va a pasar. "Dejar terminar" no promete nada
que el motor no cumpla.

Consecuencia que hay que aceptar: **el usuario no puede escapar de un render de 15 minutos** en
la UI. Solo puede cerrar LocalMind, y el texto de arriba le dice exactamente qué pasa si lo hace.

**La promesa de "queda guardado" es una promesa real.** El ledger se escribe **antes** del `POST`
—`generation-seed-ledger` §"Escritura del ledger ANTES del `POST"`— así que el texto y la
semilla están en disco mientras el render corre. Esta línea de la UI no es unerefuerzo: es la
propiedad de supervivencia a crash, traducida a lenguaje de persona. Por eso puede decir lo que
dice.

Si el trabajo está **en cola** (aún no generando), entonces sí aparece "Cancelar", porque ahí el
motor sí cancela. El rótulo cambia con el estado real, no con el estado supuesto.

### 4.5 Terminada

La imagen, su hora, su duración real, y tres acciones:

```
[Volver a esta]   [Descargar]   [Ampliar]
```

"Ampliar" — y no "Upscale" — llama a `POST /sdcpp/v1/upscale`, que **no vuelve a pasar por el
modelo**. El detalle importa: la especificación dice que los dos caminos de upscale (el hires fix
latente, que regenera, y el de la API, que solo escala) deben distinguirse porque no son lo
mismo, y el usuario solo distingue dos cosas por lo que **ocurre**, no por lo que se llama. En
"Cómo se generó" aparece `upscaler aplicado: ninguno`, lo que evita el malentendido de creer
que hubo un paso que no hubo.

### 4.6 Falló

El fallo se explica **en el motivo real**, no en "algo falló". Cuatro motivos, cuatro textos,
cuatro acciones:

| Motivo | Título | Cuerpo | Acción |
|---|---|---|---|
| VRAM insuficiente | No se pudo generar | La GPU no tiene memoria suficiente para esta imagen. | Generar a 768 × 768 |
| Pesos ausentes | No se pudo generar | Faltan pesos del modelo. | Descargar los pesos |
| Motor caído | El generador se detuvo | El proceso del motor terminó antes de terminar la imagen. | Reintentar |
| Cancelado en cola | Cancelada | No se generó nada. Tu texto y tu semilla siguen guardados. | Reintentar con la misma semilla |

Todas Offercen **reintentar con la misma semilla**, que es lo que el ledger permite y lo que un
diseño basado en `seed: -1` no podría ofrecer. En la fila de VRAM, la acción **además** trae la
resolución buena ya rellenada: el fallo enseña el camino corto.

Debajo, en todo caso:
```
▸ Detalle técnico
```

Un usuario que lee stack traces existe, y esconderlo le obliga a abrir Logs y correlacionar
horas. El log técnico **está**, plegado.

### 4.7 El chat tiene la GPU

No es un estado: es el estado de listo con la línea de coste sustituida (§5). Lo que cambia:

```
Generar
Tarda ~2 min.

Generar apaga el chat. No se pierde la conversación:
vuelve solo cuando el generador esté 15 min sin generar nada.
```

Y **simultáneamente**, en la pestaña Chat, un aviso persistente con enlace de vuelta —que la
especificación ya exige y que no se re-decide aquí:

> El chat está en pausa: la GPU la está usando el generador de imágenes. **[Volver al generador]**

Las dos mitades del mismo hecho, en las dos pestañas donde se nota. Un usuario que está en Chat
cuando arranca una generación necesita la otra mitad, o se queda con un chat muerto sin explicación...

### 4.8 Hay un agente corriendo

El caso más delicado de los nueve, porque hay trabajo humano en juego.

```
Generar
Tarda ~2 min.

OMP está trabajando con el chat. Puedes esperar a que termine
o detener el motor —eso corta su sesión y pierde lo que no haya guardado—.

[Esperar]        [Detener y generar]
```

El motor **no** lo expulsa solo: la política de cesión es "solo ante inactividad"
(`design.md` §3.2), y un turno de agente es actividad. Ofrecer "Detener y generar" como botón
primario sería proposer que el usuario destruya trabajo para ahorrarse una espera. Por eso es el
**segundo** botón, y por eso lleva el coste escrito dentro del propio rótulo.

Si aun así se pulsa, la confirmación es **en línea**, en el mismo sitio, con el botón
sustituido por su propio confirmación, no un modal:

```
[Detener y generar]  →  [Sí, detener OMP]   Cancelar
```

Es la única confirmación en dos pasos de la pestaña, y es la única que se justifica: las demás
preguntas son sobre el estado de una máquina, y esta es sobre el trabajo de una persona.

### 4.9 La licencia no comercial

No es un estado, es una **línea permanente** bajo el chip de modelo (§2.3), visible siempre que
`qwen-image-2.1` esté seleccionado. Visible antes de generar, sin haber generado nada, en la
propia pestaña, no en Ajustes. Es la obligación legal, no decoración.

Encima de la línea, y solo la primera vez por sesión, se añade una frase al texto ya existente —
sin bloque, sin modal:

> Modelos sin licencia comercial: lo que generes es para uso personal. [Entendido]

Y vuelve a aparecer **cada sesión**, no una vez para siempre, porque es una licencia y no una
preferencia.

---

## 5. El trato de la GPU, dicho antes de confirmar

Este es el problema central del encargo, y la respuesta tiene tres partes.

### 5.1 La parte que ya está resuelta: es un coste, no un riesgo

El intercambio GPU es **determinista**. No es un 60 % de probabilidad de que el chat se caiga: es
que se cae, siempre, porque 6 + 13 > 16. Y la consecuencia es reversible y automática: el LLM
vuelve solo cuando el generador lleva 15 minutos sin generar nada (`sd_idle_timeout_secs`, por
`design.md` §3.4).

Un coste determinista y reversible **no necesita un diálogo de confirmación**. Necesita estar
escrito. Los diálogos son para lo que no se sabe: para "esto borra tu carpeta", para "esto cuesta
dinero". Poner un "¿Seguro?" delante de un intercambio que ya está escrito en la interfaz
entrenaría al usuario a hacer clic sin leer —que es exactamente cómo se pierde una carpeta—,
todo para protegerlo de algo que tiene un botón "Volver" al lado.

### 5.2 El mecanismo: una línea de coste, y su reconocimiento una sola vez

**Dos elementos, siempre en el mismo sitio, bajo el botón `Generar`.**

**La línea de coste.** Siempre presente cuando hay algo que Costs, nunca cuando no:

| Condición | Texto |
|---|---|
| El generador ya tiene la GPU | *(nada — no hay nada que avisar)* |
| El chat tiene la GPU, inactivo | `Generar apaga el chat. Vuelve solo cuando el generador esté 15 min sin generar nada.` |
| Un agente está trabajando | `OMP está trabajando. Puedes esperar o detener el motor —eso corta su sesión—.` |

Es texto, no modal, sin colores, sin badges. Vive debajo del botón porque es la última cosa que se lee
antes de pulsar, y porque comparte sitio con la estimación de tiempo: **las dos cosas que
importan antes de pulsar, en el mismo sitio**. La de arriba es la máquina; la de abajo es el
trabajo.

**El reconocimiento, una vez por sesión.** La primera vez que se cumple una condición de coste,
la línea **se expande** en el mismo sitio —sigue sin modal, sigue sin bloquear— con la
explicación completa y un `tool-btn`:

> **Primera vez que esto pasa**
> El chat y el generador de imágenes no pueden funcionar a la vez: cada uno necesita la GPU
> entera. Al generar, LocalMind apaga el chat. Tus conversaciones no se borran, y el chat
> vuelve solo cuando el generador lleve un rato sin usarse.
>
> [Entendido]

A partir de ahí, y hasta que se cierre la app, esa línea vuelve a su versión de una frase y no
vuelve a aparecer.

- **Medio:** `sessionStorage`, clave `lm_gen_handover_ack_v1`. No `localStorage`. El recuerdo de
  un aviso se desvanece; preguntar una vez por sesión es barato y es lo correcto.
- **Se olvida sola** al cerrar la app. No hay ningún botón "no volver a preguntar", y no se
  necesita uno: la ausencia del botón es la razón por la que la excepción no seaccumula.
- **Una dirección por sesión.** DeJAR el generador y volver al chat no necesita su propio
  reconocimiento: en Chat el aviso es un banner con enlace de vuelta, que se lee sin commitment.

### 5.3 Lo que se descarta, y por qué

| Descartado | Por qué |
|---|---|
| Modal de confirmación en cada generación | Entrena al clic automático. Es la peor opción posible aquí (§5.1) |
| Casilla "recordar mi decisión" | Un modal con casilla es un modal con una casilla. El problema no es la cantidad de modales, es que modal |
| Aviso de escritorio (toast de Windows) | Se leen en el escritorio, no donde se está mirando, y desaparecen solos |
| Aviso cada vez, incluso con el motor ya arrancado | Cuando el generador ya tiene la GPU **no hay intercambio**. Avisar de un coste que no se paga es ruido, y el ruido enseña a ignorar los avisos |
| Botón "No mostrar más" | Si el aviso desaparece solo al día siguiente, el botón no añade control: añade una opción malYe |
| Barra de progreso del canje | La cesión dura segundos. Una barra de progreso de algo instantáneo es theater |

### 5.4 La estimación de tiempo

Se muestra **siempre, antes del clic**, en `field-hint` bajo el botón. Y es honesta:

- Con **menos de 3** generaciones completadas medidas a la misma resolución → `Tarda minutos.`
- Con **3 o más** → `Tarda ~2 min.`

La mediana se guarda en el ledger (§8, G5). Sin histórico, no hay número, y sin número se dice
lo único que se sabe. Un generador que promete "42 s" la primera vez y tarda 4 minutos pierde
la confianza que este diseño tarda quince estados en construir.

---

## 6. Iterar

### 6.1 Volver a esta

El botón. En dos sitios, idénticos:

- Bajo el resultado terminado: `[Volver a esta]`
- Sobre cada miniatura de la tira, al pasar el ratón o al enfocarla con teclado.

Y hace exactamente una cosa:

1. Rellena el compositor con **el `draft` completo** —prompt, negativo, tamaño, cantidad, sampler,
   scheduler, pasos, cfg, clip_skip, semilla, modelo, upscaler.
2. Muestra la línea de semilla con **su** semilla, no una nueva.
3. Escribe una nota arriba del compositor:

> Editando una imagen del 14:32. Cambia lo que quieras y pulsa Generar: sale una nueva, y esta se
> queda donde está.

Ese tercer punto es el que hace que la acción sea **no destructiva**. La tee Worry miedo de
perder el resultado que le gustó, y ese miedo es lo que hace que la gente genere en vez de
iterar. Decirlo en el sitio, en el momento, es más barato que cualquier diseño de versiones.

### 6.2 Qué está siempre visible y qué no

| Siempre visible | Un clic (`▸ Cómo se generó`) |
|---|---|
| La imagen | sampler |
| El texto (está en el compositor) | scheduler |
| Tamaño y número | pasos, cfg, clip_skip |
| La hora y cuánto tardó | LoRA aplicadas |
| La semilla (una línea, en el compositor) | upscaler aplicado |
| | id del modelo |

La regla que decide dónde cae cada cosa: **lo que responde a "quiero esto otra vez" siempre; lo
que responde a "quiero entender por qué salió así", un clic.**

Se prueba con la pregunta real del usuario. Cuando una imagen le gusta, lo que quiere saber es
"¿qué escribí y cuánto tardó?". Eso es lo que está siempre. "¿Qué `cfg` tenía?" es la pregunta
del segundo ajuste, y por eso tiene un clic detrás —no poruellesta, sino porque si `cfg` está
siempre visible, todo lo demás se vuelve invisible por comparación.

Excepción deliberada: **la semilla sí está siempre visible**, aunque su explicación no. Es la
única cosa que no se puede recuperar si se pierde, y por eso tiene línea propia. Ver §7.

### 6.3 La tira: el historial, y no una galería

Tira horizontal de miniaturas de 96 px, la más reciente a la izquierda, con desplazamiento
horizontal. Cada miniatura lleva su semilla en la esquina inferior derecha, en monoespaciada y
a 10 px: visible de un vistazo si se busca, invisible si no.

Cada tarjeta lleva al elegirla:

```
"Volver a esta" · "Descargar"
```

Lo que **no** tiene la tira, y es una decisión:

- **Sin agrupar por serie.** El ledger es plano y un registro por generación
  (`generation-seed-ledger` §"Ubicación, forma, esquema"), y la tira lo refleja: una lista de
  imágenes, no un árbol. Una serie implicaría un concepto que el usuario tendría que aprender
  para nada, y el ledger explícitamente no lo tiene.
- **Sin buscar ni filtrar.** Con el tope por defecto de 2000 imágenes, "encontrar una imagen de
  hace tres semanas" **no es un caso de uso**: nadie recuerda así sus imágenes, y lo que se
  recuerda es *la que le gustó*, que estaba en pantalla hace un rato. La tira cubre el caso real.
  Si aparece, es porque los 2000 ya no son suficientes, y entonces es una decisión de producto.
- **Sin galería como pestaña.** La historia vive en la pantalla donde se trabaja. Una pestaña
  "Galería" sería otro sitio al que ir.

### 6.4 Las tres transformaciones, y cómo se distinguen

La especificación exige dos caminos distintos y no confundirlos. El usuario los vive así:

| Acción | Qué ocurre | Cómo se dice |
|---|---|---|
| Cambiar un parámetro y regenerar | Vuelve a pasar por el modelo | `Generar` — normal |
| Metamorfosis (`init_image`) | Vuelve a pasar por el modelo,Transformando la anterior | Botón Transformar, en el resultado |
| Ampliar (`upscale`) | **No** vuelve a pasar por el modelo | Botón Ampliar, en el resultado |

El criterio no es el nombre de la función, es la pregunta del usuario: **"¿esto vuelve a
calcular o solo amplía?"**. "Ampliar" responde que no; "Transformar" responde que sí. Y la nota
de la fila 3 —que el upscaler aplicado se registra en "Cómo se generación"— es la que evita que
alguien piense que hubo un paso de modelo que no hubo.

---

## 7. El papel del ledger en la UI

**Regla: el ledger no tiene nombre en la interfaz.** No aparece la palabra "ledger", ni
"libro de semillas", ni "registro", ni ningún sustantivo que haya que aprender. En la pantalla
solo existen:

- **"Volver a esta"** — la acción
- **`semilla 12345`** — el dato
- **La tira** — el historial

El ledger es la razón por la que las tres existen. Si se le pone nombre, se convierte en un
concepto de la aplicación; si no, sigue siendo lo que es: infraestructura que se nota
únicamente cuando funciona.

### 7.1 Cómo se hace que "Volver a esta" sea lo obvio

Cuatro decisiones, y todas apuntan en la misma dirección: **el resultado no se mueve nunca**.

1. **La imagen que gusta, no se sustituye.** Generar de nuevo **añade** a la tira. La que le
   gusta sigue ahí, en el mismo sitio, hasta que se Apague. Un diseño donde el resultado se
   reemplaza obliga a decidir *antes* de ver si lo nuevo es mejor, y eso no es iterar.

2. **"Volver a esta" está en la miniatura, no solo en la imagen grande.** La acción aparece sobre
   la miniatura, porque es ahí donde el usuario está mirando cuando le gusta una. Si estuviera
   solo bajo la imagen grande, tendría que hacer el viaje de vuelta a ella primero — y ahí es
   donde la interacción se pierde.

3. **La nota del compositor aparece al rehidratar, no después.** La frase "cambia lo que quieras
   y sale una nueva, y esta se queda" está **en el compositor, en el momento de rehidratar**. No
   en un tooltip, no en la documentación. Es la preocupación que frena la iteración, y donde aparece es
   lo que decide si el usuario itera o abandona.

4. **El fallo ofrece la misma puerta.** "Reintentar con la misma semilla" es "Volver a esta" en
   el peor momento posible: no salió nada, y sin embargo el texto y la semilla están intactos. Que
   esa acción exista demuestra el ledger entero, porque **no hay imagen** y aun así se puede
   volver.

### 7.2 Por qué la semilla se ve pero no se toca

La línea `semilla 12345 · [otra]` está siempre visible, con el número en monoespaciada.

- **Se muestra** porque el usuario de LocalMind **es técnico**: arranca agentes, lee logs, y un
  número de cinco dígitos no le asusta. Ocultárselo sería paternalismo, y el paternalismo en una
  herramienta local es un insulto.
- **No se escribe** porque escribirla es la forma lenta de lo que hace el botón `otra`.
- **La explicación va debajo**, siempre: *"Si cambias cualquier otra cosa, solo cambia esa cosa.
  Si cambias la semilla, cambia todo."* Esa es la Idea entera del control de semilla, en una
  línea, en el sitio donde se usa.

El botón `otra` no genera un número y ya: **repone el foco en el texto**, que es lo que el
usuario quiere hacer después de cambiar la semilla. Cambiar la semilla sin tocar el prompt es
un trabajo perdido.

---

## 8. Lo que el motor debe exponer

El delta, del lado del UX al lado del motor. La columna de la derecha es la que hay que leer
primero: **solo dos de estas diez son huecos de verdad**, y las otras ocho son-properties del
motor que hay que conocer para no prometer lo que no se puede.

| # | Lo que la UX necesita | Qué hay hoy | Veredicto |
|---|---|---|---|
| G1 | Poder volver a una imagen | `seed` no vuelve nunca; `batch_count` solo informa `index` | **Resuelto en cliente**: LocalMind genera y guarda la semilla antes de enviar. La UX depende **por completo** de esto y de nada más |
| G2 | Poder detener un render que no convence | `capabilities` anuncia `cancel_generating` pero el JSON lo fija a `false`; un trabajo **en curso** devuelve `409` | **Hueco real, y el más grande.** Obliga a que la UI ofrezca "Dejar terminar" (§4.4). Ofrecer "Cancelar" ahí sería un botón que falla |
| G3 | Saber cuánto falta | `GET /jobs/{id}` da `status`, `queue_position` y `result`. **Ni `step`, ni `progress`, ni `eta`** | El único progreso real es el **stderr** del proceso, que LocalMind ya encamina a `/api/events`. La UI muestra tiempo transcurrido y no inventa un porcentaje |
| G4 | Un historial que sobreviva a la app | Ni `sd-server` ni su UI embebida tienen historial | **Hueco real.** La tira es 100 % LocalMind. No es opcional: sin ella, "Volver a esta" solo funciona con el último resultado en memoria |
| G5 | Un tiempo estimado | No lo da el motor, y **hoy no existe histórico** | **Hueco real, pequeño**: hace falta `duration_ms` en el ledger, medido por LocalMind. Es el **único dato nuevo** que esta UX exige al ledger |
| G6 | Saber con certeza qué modelo está cargado | `capabilities` **no** dice qué modelo está en memoria | La UI afirma el preset desde su propio catálogo. Si alguien arranca `sd-server` a mano con otro modelo, la UI afirmaría algo falso. **Se acepta en v1**: la respuesta correcta es el `diffusion_banner` de D-44, que nombra **lo que se pasó**, no lo que se configuró |
| G7 | Recuperar parámetros si `%APPDATA%` se pierde | `embed_image_metadata` escribe los parámetros en el PNG (`tEXt`) | No se usa en v1 —el ledger ya los tiene— pero se anota como **segunda vía**. Si algún día el ledger se pierde, la imagen sigue siendo legible |
| G8 | Poder ofrecer 1024 × 1024 | `max_resolution_mp: 1.0` en `presets.rs:149`. **Hoy es un campo declarado y sin consumidor** | **Conflicto, y necesita decisión del dueño.** 1024 × 1024 = **1,0486 MP > 1,0**, y 1024² es la resolución **nativa** de Qwen-Image-2.1. El techo declarado excluye la resolución propia del modelo. El diseño ofrece 768² / 640×896 / 896×640 / 896×448 y **no** ofrece 1024². Ver §10 |
| G9 | Avisar de que no hay espacio en disco | Ni `sd-server` ni LocalMind lo saben | Los pesos son datos del bundle en el catálogo; el tamaño se muestra desde ahí. LocalMind **no consulta** el espacio libre en v1 |

### 8.1 Las tres preguntas de G8, para el dueño

1. **¿El techo es 1,0 MP o 1,05 MP?** Con 1,0 el usuario **no puede generar a la resolución
   nativa del único modelo que tiene**, y el selector empieza por 768², que es una resolución
   deliberadamente bajita.
2. **¿Se sube el techo o se sube el selector?** Subir el techo a 1,05 declara que la atención
   MATH SDPA aguanta 1024² en esta máquina —y eso **no se ha medido**, porque no se ha generado
   nada todavía. Bajando el techo no se arregla el problema: se consagra.
3. **¿Se mide antes de decidir?** La respuesta honesta es sí, y es trabajo de Fase 3, no de este
   diseño. Hasta entonces el diseño ofrece 768² como su valor por defecto, que es lo que
   cualquier<Qwen-Image> ofrece de todos modos en las primeras etapas.

**Este documento no decide G8.** Decide que el selector no ofrece una resolución que el techo
declara imposible.

---

## 9. Fuera de alcance para v1

Lo que **no** se construye, para que esta pestaña no crezca hasta convertirse en un editor de
flujos de nodos. Cada línea dice por qué, porque "no está en alcance" sin motivo se vuelve a
discutir en tres meses.

| Fuera de alcance | Por qué |
|---|---|
| **Pestaña de vídeo** | H3 sigue siendo spike. Un informe negativo es un entregable válido; una UI para él, no |
| **Comparación A/B de dos prompts** | Se comparan generando dos veces con la misma semilla y mirándolas en la tira. La comparación lado a lado necesita un layout que no existe y un modelo mental que no aporta |
| **Inpainting con máscara, face-swap, workflows de nodos** | Estilo ComfyUI. Un formato de flujo es un proyecto de producto entero |
| **Importar una imagen del disco como `init_image`** | En v1 la metamorfosis solo puede partir de una imagen **del ledger**. El flujo de importación de ficheros es un proyecto en sí mismo, y sin él el bucle de iteración sigue cerrado |
| **Gestionar LoRAs** | El motor las anuncia en `capabilities`, y la UI las muestra en "Cómo se generó" si las hubo, pero **no se añaden ni se quitan** en v1 |
| **Un "barrido de semillas" como función** | **Es el control de Imágenes.** Dos conceptos para lo mismo, y el segundo es el que obliga a explicar qué es una semilla |
| **Galería como pestaña propia** | La tira es el historial (§6.3) |
| **Buscar y filtrar el historial** | Con el tope de 2000, nadie busca una imagen de hace tres semanas (§6.3) |
| **Reordenar, renombrar, etiquetar, fijar** | El historial es una lista ordenada por fecha y nada más. La primera columna de un gestor de ficheros es donde empiezan los veinte años de deuda |
| **Editar el prompt con el LLM** ("mejóralo") | Sería lo primero que pediría un usuario. Y es **imposible** en esta máquina: el LLM y la difusión no coexisten, así que "mejorar el texto y generar" son dos cesiones de GPU y dos recargas de 13 GB por iteración. La iteración dejaría de ser un bucle de un minuto. **No se diseña hasta que la exclusividad cambie** |
| **Seleccionar entre varios presets a la vez** | Un motor, un modelo. El aviso de reinicio por cambio de preset sí entra (§10) |
| **Compartir o subir a servicios externos** | Local es local. Es también la razón por la que no hay telemetría |
| **Traducir a otros idiomas más allá de es/en** | La infra i18n ya es de dos idiomas; añadir un tercero es decisión de producto |

---

## 10. Lo que este diseño obliga a hacer después

Consecuencias concretas, para que al implementar no se las descubra tarde:

1. **`tests/ui-tabs.mjs:25` pasa de 7 a 8 entradas.** Sin esto el arnés sigue verde sin cubrir
   la pestaña nueva, que es el mismo modo de fallo del P0 ya ocurrido: cobertura aparente sobre
   comportamiento no ejercido.

2. **Toda string visible va dos veces**, en `I18N.es` e `I18N.en` (`ui.html:2111`). Añadir solo
   `es` deja la pestaña a medio traducir al cambiar de idioma, y **ninguna puerta offline lo
   detecta** —`ui-render.mjs` es un stub DOM. Merece una comprobación de paridad de claves; hoy
   no existe y este cambio es el que la necesita.

3. **Cero modales**, por la regla ya escrita en `ui.html:2621`. Cualquier "dialog" que aparezca
   al implementar este diseño es un error de diseño, no un detalle de implementación.

4. **Cuatro o cinco clases CSS nuevas y nada más**: `.thumb-strip`, `.img-result`,
   `.cost-line`, `.seed-line`, `.state-block`. Todo lo demás se reusa. Si al implementar
   aparecen veinte, se está construyendo una aplicación nueva dentro de una pestaña.

5. **El `status-strip` global** (`ui.html:1462`) es persistente y muestra un único motor. Con dos
   motores tiene que poder hablar de los dos, o se contradice con la pestaña que lo dice. **No se
   toca en este cambio**: se documenta como dependencia de Fase 3, con la decisión pendiente de si
   el strip refleja el lease o solo el motor de texto.

6. **El aviso en Chat** —`spec.md` ya lo exige— es la otra mitad de §4.7 y hay que implementarlo
   aunque no se diseñe aquí: sin él, el usuario que está en Chat cuando arranca una generación
   ve su conversación morirse sin explicación.

7. **El banner de arranque al cambiar de preset** (`spec.md`) entra en esta pestaña, con el
   aviso **antes** de actuar, y con el mismo tratamiento no-modal: la línea de coste del §5
   cambia a "cambiar de modelo reinicia el generador" cuando el preset seleccionado no es el que
   está cargado. Es la misma línea cumpliendo un segundo trabajo, que es la señal de que la
   decisión de no tener modales fue la correcta.

8. **G8 necesita respuesta antes de implementar el selector de tamaño.** No es un detalle de
   validación: decide las cuatro opciones que ve el usuario.

---

## 11. Las decisiones, una línea cada una

1. La pestaña se llama **Generar** y va entre Chat y Modelos.
2. El compositor tiene **cuatro controles visibles**: texto, tamaño, número de imágenes,
   Generar. Todo lo demás, detrás de "Detalles de la generación".
3. El intercambio de GPU **se dice, no se confirma**: línea de coste siempre visible + una
   explicación expandida **una vez por sesión** en `sessionStorage`. **Ningún modal.**
4. "Dejar terminar" en lugar de "Cancelar", porque **el motor no puede cancelar lo que está
   generando**. Es la decisión más counterintuitive del documento y la más honesta.
5. El ledger **no tiene nombre en la UI**. Su cara es un botón: **"Volver a esta"**.
6. La semilla **se muestra y no se escribe**, con su explicación de una línea debajo.
7. El resultado **no se sustituye nunca**: iterar añade a la tira.
8. Siempre visible: imagen, texto, tamaño, hora, duración, semilla. Un clic: todo lo demás.
9. La serie está **plana**, sin grupos ni árbol.
10. La estimación se dice **antes** de pulsar, y sin número hasta que hay tres mediciones.
11. La licencia no comercial es una **línea permanente** bajo el chip de modelo, no un estado.
12. La tira no tiene búsqueda, ni filtros, ni favoritos. Todavía.
13. El fallo **explica el motivo real** y ofrece el reintento con la misma semilla.
14. El progreso es **tiempo transcurrido**, nunca un porcentaje inventado.
15. Fuera de alcance: vídeo, A/B, máscaras, nodos, LoRAs, galería, edición con LLM.

---

*Fin del diseño de UX. Fase `ux`, 2026-09-30. No se modificó `ui.html` ni ningún código de
producto. Pendiente de decisión del dueño: **G8**, el techo de 1,0 MP que excluye la resolución
nativa 1024 × 1024 del único modelo disponible.*