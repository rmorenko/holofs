# Документация holofs


> ⚠ **Translation may be stale.** This file was last synced before Stage 12-15 (versioning + deletion, HNSW-backed semantic search, /spotlight ROI, streaming /holo, /diff, /similar, auto-repair-on-read, background scrub, typed RPC layer with timeouts + NoLiveNodes panic-fix, per-folder inline upload). The English source under [../](../) is the canon for any new feature; the [Unreleased] block of [../../CHANGELOG.md](../../CHANGELOG.md) lists every delta this translation does not yet cover.


| Документ                            | Аудитория                                              |
|-------------------------------------|--------------------------------------------------------|
| [theory.md](./theory.md)            | инженеры, исследователи — математические основы        |
| [architecture.md](./architecture.md) | сопровождающие — структура системы, потоки данных     |
| [api.md](./api.md)                  | интеграторы — HTTP API, проводной протокол, форматы manifest |
| [operations.md](./operations.md)    | операторы — развёртывание, мониторинг, восстановление  |
| [threat-model.md](./threat-model.md) | специалисты по безопасности — предполагаемые противники, меры защиты |

GitHub отображает математику (`$…$` / `$$…$$`) через KaTeX с 2022 года. Блоки Mermaid отрисовываются нативно как диаграммы.

Исходные изображения для не-математических диаграмм находятся в [`images/`](./images/).
