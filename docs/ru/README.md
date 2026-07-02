# Документация holofs


> ⚠ **Translation may be stale.** This file was last synced before Stage 12-15 (versioning + deletion, HNSW-backed semantic search, /spotlight ROI, streaming /holo, /diff, /similar, auto-repair-on-read, background scrub, typed RPC layer with timeouts + NoLiveNodes panic-fix, per-folder inline upload) **and before v0.6.0** (Phase R1 gateway module fan-out + Phase N1-N8 reliability layer: SIGTERM graceful shutdown, supervised background tasks, per-bucket backpressure, fail-loud persist, persistent reputation, admin bearer-token auth, per-route handler timeouts, six new `/metrics` counters). The English source under [../](../) is the canon for any new feature; the [Unreleased] block of [../../CHANGELOG.md](../../CHANGELOG.md) lists every delta this translation does not yet cover.


| Документ                            | Аудитория                                              |
|-------------------------------------|--------------------------------------------------------|
| [theory.md](./theory.md)            | инженеры, исследователи — математические основы        |
| [architecture.md](./architecture.md) | сопровождающие — структура системы, потоки данных     |
| [api.md](./api.md)                  | интеграторы — HTTP API, проводной протокол, форматы manifest |
| [operations.md](./operations.md)    | операторы — развёртывание, мониторинг, восстановление  |
| [threat-model.md](./threat-model.md) | специалисты по безопасности — предполагаемые противники, меры защиты |

GitHub отображает математику (`$…$` / `$$…$$`) через KaTeX с 2022 года. Блоки Mermaid отрисовываются нативно как диаграммы.

Исходные изображения для не-математических диаграмм находятся в [`images/`](./images/).
