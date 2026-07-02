# holofs-Dokumentation


> ⚠ **Translation may be stale.** This file was last synced before Stage 12-15 (versioning + deletion, HNSW-backed semantic search, /spotlight ROI, streaming /holo, /diff, /similar, auto-repair-on-read, background scrub, typed RPC layer with timeouts + NoLiveNodes panic-fix, per-folder inline upload) **and before v0.6.0** (Phase R1 gateway module fan-out + Phase N1-N8 reliability layer: SIGTERM graceful shutdown, supervised background tasks, per-bucket backpressure, fail-loud persist, persistent reputation, admin bearer-token auth, per-route handler timeouts, six new `/metrics` counters). The English source under [../](../) is the canon for any new feature; the [Unreleased] block of [../../CHANGELOG.md](../../CHANGELOG.md) lists every delta this translation does not yet cover.


| Dokument                          | Zielgruppe                                  |
|-----------------------------------|---------------------------------------------|
| [theory.md](./theory.md)          | Ingenieure, Forschende — mathematische Grundlagen |
| [architecture.md](./architecture.md) | Maintainer — Systemstruktur, Datenfluss |
| [api.md](./api.md)                | Integratoren — HTTP-API, Wire-Protokoll, Manifest-Formate |
| [operations.md](./operations.md)  | Betreibende — Deployment, Monitoring, Wiederherstellung |
| [threat-model.md](./threat-model.md) | Sicherheitsprüfer — angenommene Angreifer, Gegenmaßnahmen |

GitHub rendert Mathematik (`$…$` / `$$…$$`) seit 2022 über KaTeX.
Mermaid-Blöcke werden nativ als Diagramme dargestellt.

Quellbilder für Nicht-Mathematik-Diagramme befinden sich in [`images/`](./images/).
