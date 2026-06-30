# Documentation holofs


> ⚠ **Translation may be stale.** This file was last synced before Stage 12-15 (versioning + deletion, HNSW-backed semantic search, /spotlight ROI, streaming /holo, /diff, /similar, auto-repair-on-read, background scrub, typed RPC layer with timeouts + NoLiveNodes panic-fix, per-folder inline upload). The English source under [../](../) is the canon for any new feature; the [Unreleased] block of [../../CHANGELOG.md](../../CHANGELOG.md) lists every delta this translation does not yet cover.


| Document                          | Public visé                                  |
|-----------------------------------|----------------------------------------------|
| [theory.md](./theory.md)          | ingénieurs, chercheurs — fondements mathématiques |
| [architecture.md](./architecture.md) | mainteneurs — structure du système, flux de données |
| [api.md](./api.md)                | intégrateurs — API HTTP, protocole filaire, formats de manifest |
| [operations.md](./operations.md)  | opérateurs — déployer, surveiller, restaurer |
| [threat-model.md](./threat-model.md) | auditeurs de sécurité — adversaires supposés, atténuations |

GitHub rend les expressions mathématiques (`$…$` / `$$…$$`) via KaTeX depuis 2022. Les blocs Mermaid
sont rendus nativement sous forme de diagrammes.

Les images sources des diagrammes non mathématiques se trouvent dans [`images/`](./images/).
