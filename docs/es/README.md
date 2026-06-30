# Documentación de holofs


> ⚠ **Translation may be stale.** This file was last synced before Stage 12-15 (versioning + deletion, HNSW-backed semantic search, /spotlight ROI, streaming /holo, /diff, /similar, auto-repair-on-read, background scrub, typed RPC layer with timeouts + NoLiveNodes panic-fix, per-folder inline upload). The English source under [../](../) is the canon for any new feature; the [Unreleased] block of [../../CHANGELOG.md](../../CHANGELOG.md) lists every delta this translation does not yet cover.


| Documento                          | Audiencia                                  |
|-----------------------------------|-------------------------------------------|
| [theory.md](./theory.md)          | ingenieros, investigadores — fundamentos matemáticos |
| [architecture.md](./architecture.md) | mantenedores — estructura del sistema, flujo de datos |
| [api.md](./api.md)                | integradores — API HTTP, protocolo de cable, formatos de manifest |
| [operations.md](./operations.md)  | operadores — desplegar, monitorizar, recuperar |
| [threat-model.md](./threat-model.md) | revisores de seguridad — adversarios asumidos, mitigaciones |

GitHub renderiza matemáticas (`$…$` / `$$…$$`) mediante KaTeX desde 2022. Los bloques Mermaid
se renderizan nativamente como diagramas.

Las imágenes fuente de los diagramas no matemáticos se encuentran en [`images/`](./images/).
