# Test run protocols

Dated end-to-end manual test reports against `docs/test-scenarios.md`.
Each report records the build, cluster baseline, per-section results,
and any doc drift / data drift / non-blocking observations uncovered
on that run.

## Index

| Date       | Build       | Focus                                            |
|------------|-------------|--------------------------------------------------|
| 2026-06-28 | `5566e79`   | Stages 12.7 → 15.1 (connection pool validation)  |

## Authoring conventions

* File name: `YYYY-MM-DD-<short-scope>.md`.
* Top matter must record: date, operator, build commit, gateway
  command-line, sample-tree state.
* Each `§N` section in `docs/test-scenarios.md` either gets a
  PASS/FAIL line in the results table or an explicit note that it
  was out of scope.
* Doc drift / data drift / open observations go in a numbered list
  at the end so future runs can cite them by number.
