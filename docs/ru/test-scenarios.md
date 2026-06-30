# Сценарии тестирования


> ⚠ **Translation may be stale.** This file was last synced before Stage 12-15 (versioning + deletion, HNSW-backed semantic search, /spotlight ROI, streaming /holo, /diff, /similar, auto-repair-on-read, background scrub, typed RPC layer with timeouts + NoLiveNodes panic-fix, per-folder inline upload). The English source under [../](../) is the canon for any new feature; the [Unreleased] block of [../../CHANGELOG.md](../../CHANGELOG.md) lists every delta this translation does not yet cover.


Ручной чек-лист для проверки holofs end-to-end. Покрывает каждую крупную
подсистему — CRUD, иерархический каталог, HTTP Range, голографическую
деградацию, perceptual-поиск, escrow, persistence и i18n. На каждый
сценарий — команды, ожидаемый результат и признаки успеха.

> Версия для prototype v0.4.0. CLI флаги, env-переменные и пути взяты из
> кода на момент написания; если что-то расходится — смотри
> [docs/operations.md](./operations.md) или `cargo run -p holofs-web -- --help`.

## Содержание

1. [Подготовка кластера](#1-подготовка-кластера)
2. [Базовый CRUD объектов](#2-базовый-crud-объектов)
3. [Иерархический каталог (Stage 9)](#3-иерархический-каталог-stage-9)
4. [HTTP Range на GET (Stage 11.1)](#4-http-range-на-get-stage-111)
5. [Голографическая деградация](#5-голографическая-деградация)
6. [Perceptual-поиск и diff](#6-perceptual-поиск-и-diff)
7. [Inspect: визуальный осмотр шардов](#7-inspect-визуальный-осмотр-шардов)
8. [Holographic Key Escrow](#8-holographic-key-escrow)
9. [Документация в браузере (Stage 10)](#9-документация-в-браузере-stage-10)
10. [i18n: переключение языков](#10-i18n-переключение-языков)
11. [Persistence и рестарт](#11-persistence-и-рестарт)
12. [Multi-process кластер](#12-multi-process-кластер)
13. [TLS / mTLS на проводе](#13-tls--mtls-на-проводе)
14. [Метрики, логи, SSE](#14-метрики-логи-sse)
15. [Регрессионные чек-пункты Stage 11](#15-регрессионные-чек-пункты-stage-11)

---

## 1. Подготовка кластера

**Цель:** поднять embedded-кластер (40 нод в одном процессе, 4 зоны),
убедиться что все 40 нод живы и каталог пустой.

```sh
rm -rf ./holofs-data    # чистый старт
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --log info --log-format text \
    --no-seed
```

Ожидаемый лог:

```
INFO holofs_web: starting holofs-web version=0.4.0 addr=127.0.0.1:8787
INFO holofs_web::bootstrap: embedded cluster ready n_nodes=40 n_zones=4
INFO holofs_web::bootstrap: catalog loaded objects=0
INFO holofs_web: listening addr=127.0.0.1:8787
```

**Признаки успеха.**

- `GET http://127.0.0.1:8787/` возвращает HTML с пустым каталогом.
- `GET /api/stats` отдаёт JSON `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}`.
- На 9100..9139 портах слушают 40 нод (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Без `--no-seed` каталог автоматически заполнится двумя сидовыми
картинками (`photo.png`, `mandala.png`); это удобно для других
сценариев, но мешает чистым тестам CRUD.

---

## 2. Базовый CRUD объектов

**Цель:** проверить все четыре поддерживаемых kind: image / audio / text /
opaque, плюс перцовый случай — кросс-форматный дедуп.

```sh
# image (PNG → image kind, DWT + RLNC по 4 слоям)
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/photo.png

# text (UTF-8 → text kind, chunked + partial recovery)
printf "Hello, holofs!\nLine 2\n" | curl -X PUT --data-binary @- \
    http://127.0.0.1:8787/note.txt

# opaque (произвольный бинарь → 1 RLNC слой без DWT)
dd if=/dev/urandom of=/tmp/blob.bin bs=1024 count=100 2>/dev/null
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/blob.bin

# audio (WAV → audio kind, 1D DWT по каналу)
# (если есть подходящий wav-файл; иначе пропусти)
curl -X PUT --data-binary @track.wav http://127.0.0.1:8787/track.wav
```

После каждого PUT приходит JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`.

```sh
# GET — байт-точное восстановление (full-quality decode)
curl -s -o /tmp/got.png http://127.0.0.1:8787/photo.png
cmp assets/sample.png /tmp/got.png && echo "image roundtrip OK"

# Preview только L0
curl -s -o /tmp/prev.png http://127.0.0.1:8787/preview/photo.png
file /tmp/prev.png   # должно быть PNG image

# Стат каталога
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool
```

**Признаки успеха.**

- Все PUT возвращают `201 Created` с непустым `object_id`.
- GET возвращает PNG/WAV/text-байты, побайтово равные исходнику (для
  text допускается изменение из-за UTF-8 chunking — chunks теряются, не
  байты внутри chunk-а).
- `/api/stats.objects_by_kind` отражает счётчики по типу.
- DELETE через `curl -X DELETE http://127.0.0.1:8787/photo.png` отдаёт
  `200 {"deleted":"photo.png",…}` и `objects_total` уменьшается.

### Кросс-форматный дедуп

```sh
# тот же кадр сохранён как PNG и как BMP — data_cid одинаковый
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct` будет > 0 (lossless форматы дают тот же `data_cid` →
шарды на диске не дублируются).

---

## 3. Иерархический каталог (Stage 9)

**Цель:** проверить mkdir, navigate по подпапкам, корректные ошибки при
переполнении, rename, rmdir.

```sh
# создать дерево
curl -X POST -d "parent=&name=photos"          http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos&name=2026"      http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos/2026&name=raw"  http://127.0.0.1:8787/api/mkdir

# положить файл вглубь
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/photos/2026/raw/img.png

# вытащить
curl -o /tmp/got.png http://127.0.0.1:8787/photos/2026/raw/img.png
cmp assets/sample.png /tmp/got.png && echo "deep path roundtrip OK"

# проверка отказов
curl -i -X POST -d "parent=does/not/exist&name=x" \
    http://127.0.0.1:8787/api/mkdir         # 400 parent missing
curl -i -X DELETE http://127.0.0.1:8787/api/rmdir/photos
                                            # 409 directory not empty
curl -i -X DELETE http://127.0.0.1:8787/photos
                                            # 409 is a directory

# rename (с переносом всех потомков)
curl -X POST -d "from=photos&to=archive" http://127.0.0.1:8787/api/mv
curl -s http://127.0.0.1:8787/archive/2026/raw/img.png -o /tmp/r.png
cmp assets/sample.png /tmp/r.png && echo "rename moved descendants"

# rmdir по очереди
curl -X DELETE http://127.0.0.1:8787/archive/2026/raw/img.png
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026/raw
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive
```

**UI-проверка.** Открыть `http://127.0.0.1:8787/?p=photos/2026/raw` —
breadcrumb должен показать `home / photos / 2026 / raw`, плитка с
img.png должна быть кликабельной, форма «+ folder» работает.

**Резервированные сегменты.** `PUT /health/foo`, `PUT /api/foo`,
`PUT /help/foo`, `PUT /inspect-zoom/foo` отвечают `400` — это маршруты,
которые нельзя тенить.

---

## 4. HTTP Range на GET (Stage 11.1)

**Цель:** убедиться, что partial GET работает корректно — это нужно для
audio scrubbing, resume больших скачиваний, будущего video seek.

```sh
# 1000-байтный блоб
printf 'A%.0s' $(seq 1 1000) > /tmp/blob.bin
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/range-test

# полный GET — 200, Accept-Ranges: bytes
curl -I http://127.0.0.1:8787/range-test 2>&1 | grep -i accept-ranges

# первые 10 байт
curl -i -H "Range: bytes=0-9" http://127.0.0.1:8787/range-test
# ожидаем 206 Partial Content, content-range: bytes 0-9/1000

# последние 50 байт
curl -i -H "Range: bytes=-50" http://127.0.0.1:8787/range-test
# content-range: bytes 950-999/1000

# открытый интервал
curl -i -H "Range: bytes=900-" http://127.0.0.1:8787/range-test
# content-range: bytes 900-999/1000

# за пределами файла — 416
curl -i -H "Range: bytes=5000-" http://127.0.0.1:8787/range-test
# 416 Range Not Satisfiable, content-range: bytes */1000

# мульти-диапазон не поддерживается — деградирует до 200 (полное тело)
curl -i -H "Range: bytes=0-9,20-29" http://127.0.0.1:8787/range-test
# 200 OK, 1000 байт
```

**Признаки успеха.** Статусы и `Content-Range` соответствуют таблице
выше; байты слайса побайтово точны (`0..255 × 4` паттерн —
`bytes=256-259` возвращает `00 01 02 03`).

**Реальный сценарий с медиа.**

```html
<!-- открой в браузере, проверь что прогресс-бар работает -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

Браузер автоматически шлёт `Range` при перемотке. В логе гейтвея видны
запросы `206 Partial Content`.

---

## 5. Голографическая деградация

**Цель:** ключевая фишка проекта — даже когда большая часть кластера
мертва, картинка восстанавливается **в пониженном разрешении**. Проверять
через UI `/health/<name>`.

1. Поднять кластер с сидовым `photo.png` (без `--no-seed`).
2. Открыть `http://127.0.0.1:8787/health/photo.png`. Здесь — таблица
   margin по `(channel, layer)`, Monte-Carlo сценарии 10/25/50/75%
   потерь, и таблица отказа целой зоны.
3. Открыть `http://127.0.0.1:8787/health`. Это grid из 40 узлов с
   кнопками **kill** / **revive**.
4. Убить узлы постепенно (нажимая kill), смотреть как меняется отчёт
   `/health/photo.png`:
   - 10–20% потерь: margin всех слоёв положительный, PSNR ~99 dB.
   - 30–40% потерь: margin L3 (детали) → 0, PSNR падает до ~30 dB —
     картинка становится мутнее.
   - 50–60% потерь: L2 пробит, остаются только L0+L1 — грубая форма.
   - 75% потерь: все слои пробиты — деградация в шум.
5. Между шагами скачать `GET /photo.png` и визуально посмотреть на PNG.

**Признаки успеха.**

- При потерях ниже порога K каждого слоя — полный файл.
- Выше порога L3 (но ниже L2) — узнаваемая картинка с потерянными
  высокими частотами (мутнее).
- Margin-таблица обновляется через SSE (`/api/health/events`) — после
  kill цифры меняются без перезагрузки страницы.

**Восстановление.** Нажать **revive** на убитых узлах. Через 1-2 цикла
health-монитора (`HOLOFS_MONITOR_INTERVAL`, default 15s) запускается
авто-репайр и margin возвращается.

### Отказ целой зоны

Каждая нода имеет `zone` (0..3). Убить **все 10 нод** одной зоны:
объект всё ещё декодируется до L2 благодаря zone-aware placement
(`ceil(n/z)` шардов на зону).

---

## 6. Perceptual-поиск и diff

**Цель:** найти похожие объекты по 16-байтному перцовому хэшу + увидеть
дедуп через diff.

```sh
# Залить две похожие версии одной картинки
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/orig.png
convert assets/sample.png -gaussian-blur 0x2 /tmp/blurry.png
curl -X PUT --data-binary @/tmp/blurry.png http://127.0.0.1:8787/blurry.png

# top-10 похожих
curl -s 'http://127.0.0.1:8787/api/fingerprint/orig.png' | python3 -m json.tool
# →  {"name":"orig.png","fingerprint":"a3f0…","kind":"image"}

# UI: открой /similar/orig.png — список соседей с L1-distance.
```

**Per-chunk diff.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

UI рисует зелёные клетки (одинаковые chunks) и красные (разные).
Для идентичных копий — 100% зелёных + большая `storage_saved_kb`.

---

## 7. Inspect: визуальный осмотр шардов

**Цель:** убедиться, что grid отображает все 444 шарда (3 канала × 4
слоя × 26..64 шардов) без пропусков. Регрессионный чек для Stage 11.2.

1. Открыть `http://127.0.0.1:8787/inspect/mandala.png`.
2. Прокрутить — для каждого канала (R, G, B) должно быть 4 секции
   (layers 0..3), каждая с правильным количеством миниатюр:
   - L0 — 64 шарда (16 systematic + 48 RLNC)
   - L1 — 40 шардов (16 + 24)
   - L2 — 26 шардов (16 + 10)
   - L3 — 18 шардов (16 + 2)
3. **Все 444 миниатюры должны нарисоваться** (не должно быть «битых»
   `<img>` с alt-текстом). До Stage 11.2 под нагрузкой ~8% выпадали.
4. Кликни любую миниатюру → попадёшь на zoom-страницу
   `/inspect-zoom/<c_l_idx>/<name>` с большой PNG, hex coeffs, payload.

**Цветовая разметка.** Systematic шарды (первые K=16 в каждом слое)
обведены зелёной рамкой и содержат «осмысленный» payload (видна
структура). RLNC — оранжевая рамка, payload выглядит как шум.

**Стресс-тест.** Открой одновременно 4 вкладки с `/inspect/photo.png` —
все 4 должны нарисоваться полностью. Лог гейтвея не должен содержать
строк со `status=404` для `/api/shard/...`.

---

## 8. Holographic Key Escrow

**Цель:** Shamir-style threshold scheme — разбить произвольный файл на N
долей с порогом K, восстановить из любых K.

```sh
echo "my secret seed phrase" > /tmp/secret.txt

# split 3-из-5
curl -F "file=@/tmp/secret.txt" -F "k=3" -F "n=5" \
    -o /tmp/split.html http://127.0.0.1:8787/escrow/split

# в HTML ответе — ссылки /escrow/download/<eid>_<idx>.holoshare
grep -oE '/escrow/download/[a-f0-9]+_[0-9]+\.holoshare' /tmp/split.html \
    | head -5

# скачать 3 любые доли
for idx in 0 2 4; do
    eid=$(grep -oE '[a-f0-9]{32}' /tmp/split.html | head -1)
    curl -o /tmp/share_${idx}.holoshare \
        "http://127.0.0.1:8787/escrow/download/${eid}_${idx}.holoshare"
done

# recover из 3 долей
curl -F "shares=@/tmp/share_0.holoshare" \
     -F "shares=@/tmp/share_2.holoshare" \
     -F "shares=@/tmp/share_4.holoshare" \
     -o /tmp/recovered.txt http://127.0.0.1:8787/escrow/recover
cmp /tmp/secret.txt /tmp/recovered.txt && echo "escrow roundtrip OK"
```

**Признаки успеха.**

- Любые **K** из N долей восстанавливают файл точно (byte-perfect).
- **K-1** долей не дают восстановить (recover отдаёт 400).
- Доли **не хранятся в кластере** — после рестарта гейтвея исчезают.
  Скачать сразу после split, иначе `/escrow/download/...` отвечает
  `410 Gone`.

**UI.** `http://127.0.0.1:8787/escrow` — обе формы split + recover.

---

## 9. Документация в браузере (Stage 10)

**Цель:** проверить in-app docs viewer, рендер Mermaid и KaTeX.

1. Открыть `http://127.0.0.1:8787/help`. Слева — sidebar с 7 документами,
   справа — `README.md`.
2. Кликнуть **Architecture**: должен открыться `/help/architecture` с
   правильно отрисованным **Mermaid**-графом (граф крейтов) — это SVG,
   рисует на клиенте через `mermaid.min.js`.
3. Кликнуть **Theory**: на странице много **KaTeX**-формул
   (`$x^2 + y^2$`, `$$E = mc^2$$`, etc.) — все должны быть отрисованы.
4. В sidebar внизу — переключатель языков (en, ru, de, fr, es).
   Кликнуть **Русский** — документ перерендерится из `docs/ru/<slug>.md`.
   Mermaid и KaTeX продолжают работать (формулы и графы — это код, он
   не переводится).
5. Если на каком-то языке документа нет — гейтвей отдаёт английскую
   версию (`docs/<slug>.md`) с пометкой `locale: en` в meta-строке.

**Признаки успеха.**

- Все 7 документов открываются на всех 5 языках без 404.
- Mermaid-графы — реальные SVG, не голый код в `<div>`.
- KaTeX формулы стилизованы как математика, не как сырой TeX.
- Sidebar подсвечивает активный документ (`.active` класс).

---

## 10. i18n: переключение языков

**Цель:** UI на 5 языках работает на каждом маршруте.

1. Открыть любую страницу (`/`, `/help`, `/escrow`).
2. В правой части topbar — компактный switcher: `en · ru · de · fr · es`.
3. Кликать по очереди:
   - `?lang=ru` → «каталог», «состояние», «эскроу», «помощь».
   - `?lang=de` → «Katalog», «Zustand», «Treuhand», «Hilfe».
   - `?lang=fr` → «catalogue», «santé», «séquestre», «aide».
   - `?lang=es` → «catálogo», «estado», «depósito», «ayuda».
4. URL преобразуется через `rewrite_lang` — путь и остальные query
   параметры (`?p=…`, `?a=&b=…`) сохраняются.

**Неизвестный язык.** `?lang=ja` или любой не из списка — fallback на
английский.

---

## 11. Persistence и рестарт

**Цель:** убедиться, что данные переживают рестарт.

```sh
# 1. Засеять кластер
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
SERVER_PID=$!
sleep 5

curl -X POST -d "parent=&name=keep" http://127.0.0.1:8787/api/mkdir
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/keep/img.png

# 2. Стопнуть
kill $SERVER_PID; wait $SERVER_PID 2>/dev/null

# 3. Проверить, что на диске всё на месте
ls -la ./holofs-data/catalog.bin
ls ./holofs-data/node_00 | head -5    # должны быть .shard файлы

# 4. Рестарт
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
sleep 5

# 5. Объект и каталог восстановились
curl -s 'http://127.0.0.1:8787/api/list_dir' \
    -X POST -d '' -H 'Content-Type: application/json' \
    --data-raw '{"prefix":""}'

curl -o /tmp/after-restart.png http://127.0.0.1:8787/keep/img.png
cmp assets/sample.png /tmp/after-restart.png && echo "persistence OK"
```

**Признаки успеха.**

- `catalog.bin` (~KB) и шарды (`node_*/<hex>/<hex>.shard`) на месте.
- После рестарта `GET` отдаёт байт-в-байт исходник.
- Identity нод (`node_*/identity.key`) стабильны — pubkey тот же, что и
  до рестарта.

---

## 12. Multi-process кластер

**Цель:** проверить «настоящий» распределённый режим — узлы как
отдельные процессы.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

Скрипт:

1. Запускает 8 `holofs-node`-процессов с storage в `.cluster-data/node-N`.
2. Собирает их Ed25519 pubkey.
3. Генерирует admin-keypair, подписывает whitelist.
4. Запускает гейтвей с `--whitelist`.

В другом терминале:

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# Шарды распределены по 8 процессам
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Признаки успеха.**

- Сумма шардов по нодам ≈ 444 (×3 канала × разное число на слой).
- Ctrl-C на скрипте останавливает все 8 нод и гейтвей.
- После повторного запуска того же скрипта (без чистки `.cluster-data/`)
  состояние восстанавливается — данные на месте.

---

## 13. TLS / mTLS на проводе

**Цель:** включить опциональный TLS на трафике gateway ↔ node.

```sh
# embedded режим — self-signed CA генерируется автоматически
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

В логе: `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Mutual auth:

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**Что проверять.**

- Трафик на 9100..9139 уже не plain TCP — `tcpdump` на loopback видит TLS
  handshake (`16 03 ...`).
- PUT/GET/inspect работают так же, как и без TLS.
- Без `--tls` соединения остаются plain — обратная совместимость.

Подробнее о PKI и distributed-режиме с операторскими сертами — в
[docs/operations.md](./operations.md).

---

## 14. Метрики, логи, SSE

**Prometheus.**

```sh
curl http://127.0.0.1:8787/metrics
```

Ожидаем:

```
# TYPE holofs_nodes_total gauge
holofs_nodes_total 40
holofs_nodes_live 40
holofs_objects_total{kind="image"} 2
holofs_shards_total 888
holofs_dedup_savings_pct 0.00
holofs_node_admin_killed{node="n0",addr="127.0.0.1:9100",zone="0"} 0
...
```

**Структурные логи.**

```sh
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data --log-format json --log info
```

Каждая строка — JSON с полями `timestamp`, `level`, `target`, `fields`.
Удобно для journald / fluentd / Vector / Loki.

**SSE-стрим health.**

```sh
curl -N http://127.0.0.1:8787/api/health/events
```

Каждые ~3 секунды приходит `event: health\ndata: {…}\n\n` с JSON
снимком — это то, что обновляет дашборд `/health` в реальном времени.

---

## 15. Регрессионные чек-пункты Stage 11

Эти три проверки целятся в недавно зафикшенные проблемы — стоит пройти
после каждого изменения в гейтвее или ingest-пайплайне.

### 11.1 Range на media

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Должно быть `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. Не 200, не 416.

### 11.2 Inspect не теряет шарды

Открыть `http://127.0.0.1:8787/inspect/mandala.png` в браузере. Все
444 миниатюры должны нарисоваться. В логе:

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Должно быть **0**. До Stage 11.2 здесь было ~41.

### 11.3 Большие multipart-аплоады

```sh
# 3 MB файл через escrow
dd if=/dev/urandom of=/tmp/3mb.bin bs=1024 count=3000 2>/dev/null
curl -i -F "file=@/tmp/3mb.bin" -F "k=3" -F "n=5" \
    http://127.0.0.1:8787/escrow/split 2>&1 | head -1

# 30 MB через PUT
dd if=/dev/urandom of=/tmp/30mb.bin bs=1024 count=30000 2>/dev/null
curl -i -X PUT --data-binary @/tmp/30mb.bin \
    http://127.0.0.1:8787/big.bin 2>&1 | head -1
```

Оба должны вернуть `200`/`201`, не `400 multipart read: Error parsing`.

---

## 16. Регрессионные чек-пункты Stage 11.16 – 12

### 16.1 Scope похожих (Stage 11.16)

Три пилюли scope сверху `/similar/<name>`: **все файлы** / **текущая
папка** / **текущая папка (рекурсивно)**.

```sh
# без ограничения (легаси — топ-10 по всему каталогу)
curl -s 'http://127.0.0.1:8787/similar/check.txt' \
  | grep -oE 'top similar \(<!>[0-9]+'

# только файлы из той же родительской директории
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=folder' \
  | grep -oE 'top similar \(<!>[0-9]+'

# поддерево родителя (для корня — весь каталог, как `all`)
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=tree' \
  | grep -oE 'top similar \(<!>[0-9]+'
```

Scope «липкий» — клик по соседу перебрасывает на его `/similar` с
сохранением `?scope=` (и `?lang=`).

### 16.2 Фильтр каталога + удаление файлов (Stage 11.17)

Server-side фильтр на `/` и `/?p=<prefix>` через три query-параметра:
`q` (glob по имени, `*` = подстановка, по basename, без учёта регистра),
`from`, `to` (`YYYY-MM-DD`, диапазон по `created_at_unix`).

```sh
# все PNG-файлы
curl -s 'http://127.0.0.1:8787/?q=*.png' \
  | grep -oE 'class="tree-leaf' | wc -l

# комбо: тексты, добавленные в 2026
curl -s 'http://127.0.0.1:8787/?q=*.txt&from=2026-01-01' \
  | grep -oE 'class="tree-leaf' | wc -l
```

Tree-view сохраняет родительские директории отфильтрованных листьев,
чтобы пути оставались навигируемыми. Легаси-записи с
`created_at_unix=0` (HOLOFSM6/HOLOFSM7) всегда проходят дату.

Удаление файла — form-POST зеркало существующего `rmdir_form`:

```sh
curl -s -X PUT 'http://127.0.0.1:8787/tmp-delete-me.txt' \
  -H 'Content-Type: text/plain' --data 'will be deleted'
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST 'http://127.0.0.1:8787/api/rm' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data 'path=tmp-delete-me.txt&return_to=/'
# ожидаем 303 (redirect на return_to при успехе)
```

В строке файла дерева рисуется маленькая кнопка `✕` с подтверждением.

### 16.3 Локализованный date picker (Stage 11.18)

Нативный `<input type="date">` фильтра несёт атрибут `lang` равный
локали страницы; в Chromium-браузерах его подменяет оверлей flatpickr
(загружается с jsdelivr), чтобы календарь всегда говорил на языке
страницы, а не системы.

Открыть `/?lang=ru`, кликнуть по полю даты — заголовок календаря по
русски. Переключиться на `/?lang=fr`, повторить — по французски. Само
`value=…` всё равно идёт как `YYYY-MM-DD`, независимо от локали.

### 16.4 Smoke-тест MCP-сервера (Stage 12)

Запустить кластер с токеном, чтобы включить write-инструменты:

```sh
HOLOFS_MCP_TOKEN=devtoken ./holofs-web \
  --storage ./holofs-data --addr 127.0.0.1:8787 \
  --log warn --log-format text
```

Инициализировать MCP-сессию и получить список инструментов:

```sh
TOKEN=devtoken
SID=$(curl -si -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},
    "clientInfo":{"name":"curl","version":"1"}}}' \
  | grep -i 'mcp-session-id:' | awk '{print $2}' | tr -d '\r')

curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}' > /dev/null

curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | grep -oE '"name":"[^"]+"' | sort
```

Ожидаем 12 имён: `diff_objects`, `find_similar`, `get_cluster_health`,
`get_object_health`, `inspect_object`, `inspect_shard`, `list_catalog`,
`mkdir`, `mv_object`, `put_object_text`, `read_object_text`, `rmdir`.

Auth-гейтинг:

```sh
# без заголовка → 401
curl -s -o /dev/null -w 'no-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'

# валидный bearer → 200
curl -s -o /dev/null -w 'with-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'
```

Поверхность Resources:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | grep -oE '"uri":"holofs:///[^"]+"' | head -5
```

Подключение в Claude Code — см. [api.md §5](./api.md#5-mcp-сервер-stage-12).

---

## 17. Wavelet-операции (Stage 12.5)

Обе операции работают через тот же MCP-эндпоинт (`/mcp`) — оставайся
в той же сессии что и в §16.4. Перед стартом кластера задай
`HOLOFS_MCP_TOKEN` чтобы заработал `save_as`.

### 17.1 Wavelet mix

Построить гибридный PNG из двух совместимых изображений и сохранить
в каталог как `hybrid.png`:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"photo_gray.png","split":2,
      "save_as":"hybrid.png"}}}' \
  | grep -oE '"saved_as":"[^"]*"|"width":[0-9]+|"height":[0-9]+'

# Скачать гибрид — обычный PNG.
curl -s -o /tmp/hybrid.png 'http://127.0.0.1:8787/hybrid.png'
file /tmp/hybrid.png
```

`file /tmp/hybrid.png` должен показать настоящий PNG с ожидаемыми
размерами.

Ошибки совместимости — несовпадение формы / k / per-layer параметров
дают `BadRequest`:

```sh
# Смешать image с text → BadRequest от проверки kind.
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"check.txt","split":0}}}' \
  | grep -oE '"message":"[^"]*"' | head -1
```

### 17.2 Audio layer filter

Отрендерить аудио с одной только басовой компонентой (L0) и
сохранить как новую запись каталога:

```sh
# Предполагает что какой-то `track.wav` уже загружен.
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{
    "name":"audio_filter","arguments":{
      "path":"track.wav","keep_layers":[0],
      "save_as":"track_bass_only.wav"}}}' \
  | grep -oE '"saved_as":"[^"]*"|"kept_layers":\[[^]]*\]'
```

`keep_layers:[]` или маска со всеми false → `BadRequest` (выход был
бы тишиной).

### 17.3 Inline-режим "без копии"

Опусти `save_as` чтобы получить байты обратно как base64-блоб —
удобно когда хочешь чтобы LLM посмотрел на результат, не оставляя
артефактов в каталоге:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"photo_blurry.png","split":1}}}' \
  | grep -oE '"bytes_len":[0-9]+|"saved_as":"[^"]*"' | head -2
```

`bytes_len` показывает размер PNG; `saved_as` должно отсутствовать.

---

## 18. Quickstart с генератором тестового набора

`tools/test-data/` содержит готовый набор для прогона всех новых
фич без подготовки фикстур вручную:

```
tools/test-data/
├── generate-samples.py    # детерминированный генератор, только stdlib Python 3.10+
├── clean-cluster.sh       # стирает catalog + shards + embeddings + versions
├── upload-samples.sh      # PUT-ит весь tree с сохранением иерархии
└── run-tests.sh           # end-to-end smoke по 12.6–15.0
```

### 18.1 Сгенерировать набор

```sh
python3 tools/test-data/generate-samples.py
# → wrote 38 samples (1,862,535 bytes) under <repo>/samples
```

Что получится:

```
samples/
  photos/{landscapes,abstract,brand-pairs}/*.png
  audio/{music,effects,silence}/*.wav
  docs/{notes,spec,legal}/{*.txt,*.md,*.json}
  binaries/{archives,blobs}/{*.zip,*.tar,*.bin}
```

Папка `brand-pairs/` содержит пары `logo-N.png` + `logo-N-wm.png`
— это намеренные near-duplicates, чтобы robust-copy на
`/similar/<name>` дал реальные хиты.

### 18.2 Полный чистый рестарт

```sh
tools/test-data/clean-cluster.sh
# (FORCE=1 пропускает подтверждение)

./target/release/holofs-web \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --enable-embed \
    --enable-versions &
```

Без `--enable-embed` `/search` показывает баннер "embed disabled".
Без `--enable-versions` PUT-замена удаляет старые шарды.

### 18.3 Залить набор

```sh
tools/test-data/upload-samples.sh
```

Скрипт сначала mkdir-ит каждый prefix, потом PUT-ит каждый файл,
в конце дёргает `/api/stats` и печатает итоги.

### 18.4 End-to-end smoke

```sh
tools/test-data/run-tests.sh
```

Покрывает Stage 9 (каталог), 12.6 (mix), 12.7 (metrics + /about),
12.8/9 (search), 13.0 (robust-copy), 13.1 (streaming),
13.2/14.1 (spotlight modes), 13.4 (versions), 14.0/3 (GC).
Финал: `N / M passed`, exit-code 0 только если всё зелёное.

---

## 19. Per-file metrics (Stage 12.7)

`GET /health/<name>` показывает блок "Unique metrics": dedup,
originality (overall + по слоям), layer-energy distribution,
audio bands (для аудио), top-N соседей с per-layer breakdown.

Curl backing API:

```sh
# Endpoint — leptos server fn; параметр `name` идёт телом формы,
# а не query-параметром. GET вернёт 405 Method Not Allowed.
curl -s -X POST -d 'name=photos/landscapes/mountain.png' \
     http://127.0.0.1:8787/api/file_metrics \
     | python3 -m json.tool
```

Ожидаемое: `total_shards_in_file ≈ unique_shards_in_file`,
`originality_pct` в `[0, 100]`, `originality_per_layer` длиной
`nlayers`, `layer_energy` заполнено для image/audio,
`audio_bands` только для `kind == "audio"`, `neighbours` пустой
если у файла нет дубликатов в каталоге.

На `brand-pairs/logo-1.png` в `neighbours[]` ожидается
`brand-pairs/logo-1-wm.png` как **верхняя строка** (наибольший
`shared_total`) с ненулевым `shared_per_layer[0]` — LL-band
(layer-0) систематические шарды переживают локализованный угловой
watermark и совпадают побайтно. У остальных файлов
`shared_per_layer[0] == 0`. Это и есть пейлоад для Stage 13.0
robust-copy. Про caveat на формулу score см. §21.

---

## 20. CLIP search + bands (Stages 12.8/12.9/13.3)

**Prereq:** сервер запущен с `--enable-embed`. Первый запрос
скачивает ~155 MiB весов CLIP в `~/.cache/huggingface/hub`.

Bulk-индекс после чистого рестарта:

```sh
curl -s -X POST http://127.0.0.1:8787/api/embed_all
# → {"new":<N>,"skipped":<M>}
```

Per-band query:

```sh
for band in any coarse mid full; do
  echo "--- band=$band ---"
  curl -s "http://127.0.0.1:8787/api/search?q=mountain&band=$band&limit=3" \
    | python3 -m json.tool
done
```

Ожидаемое:

- `any` — dedup по имени, лучший band на файл.
- `coarse` — ранжирует по силуэту/цвету, landscapes сверху.
- `full` — по фактуре, noise/pixel-blocks подтянутся.
- `mid` — gradient-картинки.

UI: `/search?q=mountain&band=any` — гридка карточек с
band-бейджем (синий = coarse, фиолетовый = mid, розовый = full).

---

## 21. Robust-copy (Stage 13.0)

Цель: проверить детект "structure matches, detail differs"
(водяные знаки / перекодировки / лёгкая ретушь).

1. Открой `/similar/photos/brand-pairs/logo-1.png`.
2. В таблице "shard overlaps" ожидаем:
   - `logo-1-wm.png` — **верхняя строка** (наибольший
     `shared shards`). Локализованный watermark в правом нижнем углу
     не трогает большинство LL (layer-0) систематических шардов, и
     ~39+ из 192 layer-0 шардов совпадают между оригиналом и
     watermarked-вариантом. У других (не-brand) соседей `shared
     per_layer[0] == 0`.
   - `low-band %` > 0 (layer-0 overlap есть).

**Оговорка про сам score** (ограничение тестовых данных, не баг
фичи): на засеянном sample-tree `robust copy?` остаётся **отрицательным**
для всех brand-pairs, и жёлтый ⚠ при ≥ +30 не загорается. Причина —
gateway upscale-ит 256×256 PNG до своего рабочего 512×512 перед
кодированием; билинейный/бикубический upscale делает финест-band
(layer 3) почти всюду нулевым для любой гладкой синтетики. K=16
систематических шардов над этими нулями хешируются в одно и то же
значение для **всех** картинок в наборе — это даёт baseline ~36 %
`high-band %`, который перевешивает score. На реальных фото с
текстурой score спокойно переваливает +30; на этом наборе
рассматривай **верхнее место по shared + ненулевой layer-0 overlap**
как success-маркер, а не абсолютное число.

---

## 22. Streaming hologram (Stage 13.1)

```sh
curl -sI 'http://127.0.0.1:8787/preview/stream/photos/abstract/mandala-a.png'
# Content-Type: multipart/x-mixed-replace; boundary=hololayer-2026-06-25
```

В браузере: `/holo/photos/abstract/mandala-a.png`, **force-reload**
(Cmd+Shift+R) чтобы обойти PNG-кэш. Картинка должна заметно
"фокусироваться" по мере прихода новых multipart-частей.

---

## 23. Spotlight modes (Stages 13.2 + 14.1)

```sh
img=photos/landscapes/mountain.png
for mode in spatial coeff; do
  curl -s -o "/tmp/spot-$mode.png" \
       "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=$mode"
done
md5 /tmp/spot-*.png   # хеши должны отличаться
```

- `spatial`: вокруг ROI — размытое L0, видно контекст.
- `coeff`: вокруг ROI близко к чёрному с чётким Haar-блочным
  бордером (зануление коэффициентов вне ROI).

Headers: `x-holofs-roi-px`, `x-holofs-decode-ms`,
`x-holofs-bytes-downloaded`. Последний станет реальной экономией
трафика после Stage 15.1 (per-block encoding для replicated
объектов).

UI: `/spotlight?a=<image>` — пиллы выбора mode + preset-кнопки +
custom-coordinates форма.

---

## 24. Per-object versioning (Stage 13.4)

**Prereq:** `--enable-versions`. Storage растёт пока флаг включён;
`POST /api/gc` чистит.

1. Залей `mandala-a.png` (через scenario 18).
2. Перезалей под тем же именем другую картинку:

   ```sh
   curl -sf -X PUT \
        --data-binary @samples/photos/abstract/mandala-b.png \
        http://127.0.0.1:8787/photos/abstract/mandala-a.png
   ```

3. `/versions/photos/abstract/mandala-a.png` должна показать
   архивную строку.
4. Кликни "restore" (или curl `POST /api/restore` с `name=` и
   `id=`). После этого:

   ```sh
   md5 <(curl -sf http://127.0.0.1:8787/photos/abstract/mandala-a.png)
   ```

   должен совпасть с исходным хешем — restore обратим, замена
   ушла в архив.

---

## 25. GC: shards + embeddings + concurrency (Stages 14.0/3/4)

1. После PUT-replace (см. 24) удали version side-files:

   ```sh
   rm -rf holofs-data/versions/photos__abstract__mandala-a.png
   ```

2. Запусти GC:

   ```sh
   curl -s -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool
   ```

Ожидаемое:

- `purged_total > 0`.
- `embeddings_dropped > 0` если stale-CID-ы были в индексе.
- Каждая нода `ok: true`, без `error`.
- `duration_ms` обычно < 100 ms на dev-кластере.

Concurrency-check: запусти PUT и GC параллельно через `&`. RwLock
барьер должен сериализовать их — GC дождётся PUT, потом сделает
своё дело.

---

## Завершение

Чистый стоп:

```sh
pkill -f 'target/release/holofs-web'
# или Ctrl-C в терминале, где запущен кластер
```

Чистый wipe — удалить весь state:

```sh
tools/test-data/clean-cluster.sh
# или вручную:
rm -rf ./holofs-data ./.cluster-data
```

Если что-то ведёт себя неожиданно — сравни поведение с описанным здесь
и загляни в:

- [docs/operations.md](./operations.md) — конфигурация и эксплуатация
- [docs/architecture.md](./architecture.md) — поток данных PUT → GET
- [docs/api.md](./api.md) — HTTP API, формат wire-протокола, новые эндпоинты
- [docs/threat-model.md](./threat-model.md) — какие угрозы покрыты
- `tools/test-data/README.md` — usage сэмпл-tree + smoke-runner
