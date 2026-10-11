# Guía de documentación

Elige una ruta según lo que quieras hacer. La documentación detallada vive aquí y en las carpetas `tests/` y `packaging/`.

## Si vas a usar OMNI

1. [Manual de usuario](MANUAL.md): instala OMNI, importa un modelo y completa el primer inicio.
2. [Empaquetado](../packaging/README.md): conoce la instalación, las actualizaciones y los pasos de recuperación.

## Si vas a desarrollar o mantener el proyecto

| Documento | Para qué sirve |
|---|---|
| [Especificación del sistema](SRS.md) | Requisitos, contratos, componentes y comportamiento esperado. |
| [Pruebas](../tests/README.md) | Suites disponibles, datos de prueba y ejecución local. |
| [Registro de cambios](../CHANGELOG.md) | Cambios publicados y versiones. |
| [Diseño de red](FASE-B-NET-DESIGN.md) | Decisiones de conexión entre Oráculo y Guest. |
| [Plan de simplificación](ROOT-SIMPLIFICATION-PLAN.md) | Trabajo de reducción y organización de las raíces del proyecto. |
| [Auditoría de rendimiento](PERF-AUDIT.md) | Hallazgos y seguimiento de rendimiento. |

## Investigaciones y referencias

- [`agents/`](agents/): integraciones y comparativas de harnesses para agentes.
- [`models/`](models/): evaluaciones de modelos en hardware local.
- [`perf/`](perf/): mediciones y ajustes de rendimiento, incluida la guía para AMD.
- [`srs/`](srs/): índice HTML y respuestas complementarias a la especificación.
- [`ui/`](ui/): capturas, auditorías, notas y herramientas de inspección de la interfaz.

## Mapa del código

- [`../src-rust/`](../src-rust/): aplicación nativa, gateway local y gestión del motor.
- [`../ui.html`](../ui.html): interfaz principal.
- [`../packaging/`](../packaging/): scripts del ZIP portable y del instalador.
- [`../.github/workflows/`](../.github/workflows/): integración continua y publicación de releases.
