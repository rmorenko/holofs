
# Тестовые сценарии

Ручной end-to-end чек-лист для holofs. Охватывает все основные подсистемы —
CRUD, иерархический каталог, HTTP Range, голографическую деградацию,
перцептуальный поиск, эскроу, устойчивость и i18n. Каждый сценарий
содержит команды, ожидаемый результат и маркеры успеха.

> Ориентировано на 1.0.0. Флаги CLI, переменные окружения и
> пути отражают код на момент написания; если что-то расходится, сверьтесь
> с [docs/ru/operations.md](./operations.md) или
> `cargo run -p holofs-web -- --help`.

## Содержание

1. [Поднять кластер](#1-поднять-кластер)
2. [Базовый CRUD объектов](#2-базовый-crud-объектов)
3. [Иерархический каталог](#3-иерархический-каталог)
4. [HTTP Range на GET](#4-http-range-на-get)
5. [Голографическая деградация](#5-голографическая-деградация)
6. [Перцептуальный поиск и diff](#6-перцептуальный-поиск-и-diff)
7. [Inspect: визуальный аудит шардов](#7-inspect-визуальный-аудит-шардов)
8. [Holographic Key Escrow](#8-holographic-key-escrow)
9. [Встроенный просмотрщик документации](#9-встроенный-просмотрщик-документации)
10. [i18n: переключение языков](#10-i18n-переключение-языков)
11. [Персистентность и перезапуск](#11-персистентность-и-перезапуск)
12. [Мультипроцессный кластер](#12-мультипроцессный-кластер)
13. [TLS / mTLS на проводе](#13-tls--mtls-на-проводе)
14. [Метрики, логи, SSE](#14-метрики-логи-sse)
15. [Регрессионные проверки](#15-регрессионные-проверки)

---

## 1. Поднять кластер

**Цель.** Поднять встроенный кластер (40 узлов в одном процессе, 4 зоны)
и убедиться, что каждый узел жив, а каталог пуст.

```sh
rm -rf ./holofs-data    # чистый старт
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --log info --log-format text \
    --no-seed
```

Ожидаемые строки лога:

```
INFO holofs_web: starting holofs-web version=1.0.0 addr=127.0.0.1:8787
INFO holofs_web::bootstrap: embedded cluster ready n_nodes=40 n_zones=4
INFO holofs_web::bootstrap: catalog loaded objects=0
INFO holofs_web: listening addr=127.0.0.1:8787
```

**Маркеры успеха.**

- `GET http://127.0.0.1:8787/` возвращает HTML каталога (пустая сетка).
- `GET /api/stats` возвращает `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}`.
- 40 портов слушают на 9100..9139 (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Без `--no-seed` каталог засеивается двумя демо-картинками
(`photo.png`, `mandala.png`); удобно для последующих сценариев, но
неудобно для чистых CRUD-тестов.

---

## 2. Базовый CRUD объектов

**Цель.** Покрыть все четыре поддерживаемых типа — image / audio / text / opaque
— плюс перцептуальный edge-case: кросс-форматный dedup.

```sh
# image (PNG → image kind, DWT + RLNC по 4 слоям)
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/photo.png

# text (UTF-8 → text kind, chunked + partial recovery)
printf "Hello, holofs!\nLine 2\n" | curl -X PUT --data-binary @- \
    http://127.0.0.1:8787/note.txt

# opaque (произвольный binary → 1 RLNC-слой, без DWT)
dd if=/dev/urandom of=/tmp/blob.bin bs=1024 count=100 2>/dev/null
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/blob.bin

# audio (WAV → audio kind, 1D DWT по каналам)
# (пропустите, если под рукой нет wav-файла)
curl -X PUT --data-binary @track.wav http://127.0.0.1:8787/track.wav
```

Каждый PUT возвращает JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`.

```sh
# GET — побайтовое восстановление (декодирование полного качества)
curl -s -o /tmp/got.png http://127.0.0.1:8787/photo.png
cmp assets/sample.png /tmp/got.png && echo "image roundtrip OK"

# Preview = только L0
curl -s -o /tmp/prev.png http://127.0.0.1:8787/preview/photo.png
file /tmp/prev.png   # должен быть PNG image

# Статистика каталога
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool
```

**Маркеры успеха.**

- Каждый PUT возвращает `201 Created` с ненулевым `object_id`.
- GET возвращает исходные байты PNG/WAV/text, побайтово точно для image
  и opaque (text допускает потерю целого chunk-а, но никогда байтов внутри chunk-а).
- `/api/stats.objects_by_kind` отражает счётчики по типам.
- `curl -X DELETE http://127.0.0.1:8787/photo.png` возвращает
  `200 {"deleted":"photo.png",…}` и `objects_total` уменьшается.

### Кросс-форматный dedup

```sh
# один и тот же кадр как PNG и BMP — data_cid одинаковый
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct > 0`, потому что lossless-форматы дают одинаковый
`data_cid` → шарды на диске дедуплицируются.

---

## 3. Иерархический каталог

**Цель.** Проверить mkdir, навигацию по подкаталогам, корректные отказы
при коллизии, rename, rmdir.

```sh
# строим дерево
curl -X POST -d "parent=&name=photos"          http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos&name=2026"      http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos/2026&name=raw"  http://127.0.0.1:8787/api/mkdir

# загружаем глубоко
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/photos/2026/raw/img.png

# забираем обратно
curl -o /tmp/got.png http://127.0.0.1:8787/photos/2026/raw/img.png
cmp assets/sample.png /tmp/got.png && echo "deep path roundtrip OK"

# отказы
curl -i -X POST -d "parent=does/not/exist&name=x" \
    http://127.0.0.1:8787/api/mkdir         # 400 parent missing
curl -i -X DELETE http://127.0.0.1:8787/api/rmdir/photos
                                            # 409 directory not empty
curl -i -X DELETE http://127.0.0.1:8787/photos
                                            # 409 is a directory

# rename (переносит всех потомков)
curl -X POST -d "from=photos&to=archive" http://127.0.0.1:8787/api/mv
curl -s http://127.0.0.1:8787/archive/2026/raw/img.png -o /tmp/r.png
cmp assets/sample.png /tmp/r.png && echo "rename moved descendants"

# rmdir снизу вверх
curl -X DELETE http://127.0.0.1:8787/archive/2026/raw/img.png
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026/raw
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive
```

**Проверка UI.** Откройте `http://127.0.0.1:8787/?p=photos/2026/raw` —
breadcrumb должен читаться как `home / photos / 2026 / raw`, плитка
img.png кликабельна, форма «+ folder» работает.

**Зарезервированные сегменты.** `PUT /health/foo`, `PUT /api/foo`,
`PUT /help/foo`, `PUT /inspect-zoom/foo` возвращают `400` — эти
маршруты нельзя перекрыть.

---

## 4. HTTP Range на GET

**Цель.** Подтвердить, что частичные GET работают — необходимо для
скраббинга аудио, возобновляемых больших загрузок, будущего seek видео.

```sh
# blob на 1000 байт
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

# за пределы EOF — 416
curl -i -H "Range: bytes=5000-" http://127.0.0.1:8787/range-test
# 416 Range Not Satisfiable, content-range: bytes */1000

# multi-range не поддерживается — деградирует до 200 (полное тело)
curl -i -H "Range: bytes=0-9,20-29" http://127.0.0.1:8787/range-test
# 200 OK, 1000 байт
```

**Маркеры успеха.** Коды статуса и `Content-Range` совпадают с таблицей
выше; выбранные срезы побайтово точны (шаблон `0..255 × 4` даёт
`00 01 02 03` для `bytes=256-259`).

**Сценарий с реальным медиа.**

```html
<!-- откройте в браузере, убедитесь, что перемотка работает -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

Браузер отправляет `Range` при каждой перемотке. Лог gateway показывает
ответы `206 Partial Content`.

---

## 5. Голографическая деградация

**Цель.** Флагманский трюк — когда большая доля кластера умирает,
файл всё ещё декодируется **в пониженном разрешении**. Управляйте
через UI на `/health/<name>`.

1. Загрузитесь с засеянным `photo.png` (уберите `--no-seed`).
2. Откройте `http://127.0.0.1:8787/health/photo.png`. Вы получите
   таблицу marginов по `(канал, слой)`, Монте-Карло-прогоны при
   10/25/50/75% потерь и сценарий отказа целой зоны.
3. Откройте `http://127.0.0.1:8787/health`. Сетка из 40 узлов с
   кнопками **kill** / **revive**.
4. Убивайте узлы по одному и следите за `/health/photo.png`:
   - 10–20% потерь: margin положительный везде, PSNR ~99 дБ.
   - 30–40% потерь: margin L3 (детали) → 0, PSNR падает до ~30 дБ —
     картинка становится размытее.
   - 50–60% потерь: умирает L2, остаются только L0+L1 — виден только
     грубый силуэт.
   - 75% потерь: все слои мертвы — выход коллапсирует в шум.
5. Между шагами делайте `GET /photo.png` и смотрите на PNG глазами.

**Маркеры успеха.**

- Ниже K-порога каждого слоя — полный файл.
- Выше L3, но ниже L2 — узнаваемая картинка без высоких частот
  (размытее).
- Таблица margin обновляется через SSE (`/api/health/events`) —
  числа сдвигаются после kill без перезагрузки страницы.

**Восстановление.** Нажмите **revive** на убитых узлах. За 1–2 цикла
health-монитора (`HOLOFS_MONITOR_INTERVAL`, по умолчанию 15 с)
срабатывает автопочинка и margin возвращается.

### Отказ целой зоны

Каждый узел несёт `zone` (0..3). Убейте **все 10 узлов** одной зоны:
объект всё ещё декодируется до L2 благодаря zone-aware placement
(`ceil(n/z)` шардов на зону).

---

## 6. Перцептуальный поиск и diff

**Цель.** Найти похожие объекты по 16-байтовому перцептуальному
хешу + наблюдать dedup через diff.

```sh
# загружаем две похожие версии одной картинки
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/orig.png
convert assets/sample.png -gaussian-blur 0x2 /tmp/blurry.png
curl -X PUT --data-binary @/tmp/blurry.png http://127.0.0.1:8787/blurry.png

# top-10 соседей
curl -s 'http://127.0.0.1:8787/api/fingerprint/orig.png' | python3 -m json.tool
# →  {"name":"orig.png","fingerprint":"a3f0…","kind":"image"}

# UI: откройте /similar/orig.png — соседи отсортированы по L1-расстоянию.
```

**Per-chunk diff.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

UI рисует зелёные клетки (совпадающие chunk-и) и красные (разные).
Две идентичные копии → 100% зелёного плюс большой `storage_saved_kb`.

---

## 7. Inspect: визуальный аудит шардов

**Цель.** Убедиться, что сетка показывает все 444 шарда (3 канала × 4 слоя
× 26..64 на слой) без пропусков. Регрессионная проверка.

1. Откройте `http://127.0.0.1:8787/inspect/mandala.png`.
2. Прокрутите — для каждого канала (R, G, B) вы должны увидеть 4 секции
   (слои 0..3), каждая с правильным числом миниатюр:
   - L0 — 64 шарда (16 систематических + 48 RLNC)
   - L1 — 40 шардов (16 + 24)
   - L2 — 26 шардов (16 + 10)
   - L3 — 18 шардов (16 + 2)
3. **Все 444 миниатюры должны отрисоваться** (без битых плейсхолдеров
   `<img>`). Раньше ~8% отваливалось под конкурентной нагрузкой.
4. Кликните любую миниатюру → попадёте на `/inspect-zoom/<c_l_idx>/<name>`
   с большой PNG, hex-коэффициентами, payload-ом.

**Цветовая кодировка.** Систематические шарды (первые K=16 каждого
слоя) — с зелёной рамкой и несут осмысленный payload (структура
видна). RLNC — оранжевая рамка, payload похож на шум.

**Стресс-тест.** Откройте 4 вкладки браузера с `/inspect/photo.png`
одновременно — каждая рендерится полностью. В логе gateway не должно
быть строк `status=404` для `/api/shard/...`.

---

## 8. Holographic Key Escrow

**Цель.** Пороговая схема в стиле Шамира — разбить произвольный файл
на N долей с порогом K, восстановить из любых K.

```sh
echo "my secret seed phrase" > /tmp/secret.txt

# 3-of-5
curl -F "file=@/tmp/secret.txt" -F "k=3" -F "n=5" \
    -o /tmp/split.html http://127.0.0.1:8787/escrow/split

# HTML несёт ссылки /escrow/download/<eid>_<idx>.holoshare
grep -oE '/escrow/download/[a-f0-9]+_[0-9]+\.holoshare' /tmp/split.html \
    | head -5

# скачиваем любые 3 доли
for idx in 0 2 4; do
    eid=$(grep -oE '[a-f0-9]{32}' /tmp/split.html | head -1)
    curl -o /tmp/share_${idx}.holoshare \
        "http://127.0.0.1:8787/escrow/download/${eid}_${idx}.holoshare"
done

# восстанавливаем из 3 долей
curl -F "shares=@/tmp/share_0.holoshare" \
     -F "shares=@/tmp/share_2.holoshare" \
     -F "shares=@/tmp/share_4.holoshare" \
     -o /tmp/recovered.txt http://127.0.0.1:8787/escrow/recover
cmp /tmp/secret.txt /tmp/recovered.txt && echo "escrow roundtrip OK"
```

**Маркеры успеха.**

- Любые **K** из N долей восстанавливают файл точно (побайтово).
- **K-1** долей не восстанавливают (recover возвращает 400).
- Доли **не хранятся в кластере** — они пропадают при перезапуске
  gateway. Скачивайте сразу после split; иначе
  `/escrow/download/...` вернёт `410 Gone`.

**UI.** `http://127.0.0.1:8787/escrow` содержит формы split и recover.

---

## 9. Встроенный просмотрщик документации

**Цель.** Проверить просмотрщик документов, рендеринг Mermaid и KaTeX.

1. Откройте `http://127.0.0.1:8787/help`. В левой боковой панели 7
   документов, справа отображается `README.md`.
2. Нажмите **Architecture**: `/help/architecture` открывается с
   корректно отрендеренной диаграммой **Mermaid** (граф зависимостей
   crate-ов) — SVG рисуется на клиенте через `mermaid.min.js`.
3. Нажмите **Theory**: множество **KaTeX**-формул (`$x^2 + y^2$`,
   `$$E = mc^2$$` и т.д.) — все отрендерены.
4. Внизу боковой панели переключатель языков
   (en, ru, de, fr, es). Нажмите **Русский** — документ перерисуется
   из `docs/ru/<slug>.md`. Mermaid и KaTeX продолжают работать
   (формулы и диаграммы — это код, не переводится).
5. Где локализованный вариант отсутствует, gateway отдаёт английский
   (`docs/<slug>.md`) с `locale: en` в мета-строке.

**Маркеры успеха.**

- Все 7 документов открываются на всех 5 языках без 404.
- Mermaid-диаграммы — реальные SVG, а не сырой код в `<div>`.
- KaTeX-формулы выглядят как набранная математика, а не TeX-исходник.
- Боковая панель подсвечивает активный документ (класс `.active`).

---

## 10. i18n: переключение языков

**Цель.** UI работает на 5 языках на каждом маршруте.

1. Откройте любую страницу (`/`, `/help`, `/escrow`).
2. Справа в топбаре компактный переключатель:
   `en · ru · de · fr · es`.
3. Пройдитесь по:
   - `?lang=ru` → «каталог», «состояние», «эскроу», «помощь».
   - `?lang=de` → «Katalog», «Zustand», «Treuhand», «Hilfe».
   - `?lang=fr` → «catalogue», «santé», «séquestre», «aide».
   - `?lang=es` → «catálogo», «estado», «depósito», «ayuda».
4. URL переписывается через `rewrite_lang` — путь и другие query-параметры
   (`?p=…`, `?a=&b=…`) сохраняются.

**Неизвестная локаль.** `?lang=ja` или что угодно ещё откатывается на английский.

---

## 11. Персистентность и перезапуск

**Цель.** Убедиться, что данные переживают перезапуск.

```sh
# 1. засеиваем кластер
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
SERVER_PID=$!
sleep 5

curl -X POST -d "parent=&name=keep" http://127.0.0.1:8787/api/mkdir
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/keep/img.png

# 2. останавливаем
kill $SERVER_PID; wait $SERVER_PID 2>/dev/null

# 3. проверяем состояние на диске
ls -la ./holofs-data/catalog.bin
ls ./holofs-data/node_00 | head -5    # должны быть .shard файлы

# 4. перезапускаем
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
sleep 5

# 5. объект и каталог вернулись
curl -s 'http://127.0.0.1:8787/api/list_dir' \
    -X POST -d '' -H 'Content-Type: application/json' \
    --data-raw '{"prefix":""}'

curl -o /tmp/after-restart.png http://127.0.0.1:8787/keep/img.png
cmp assets/sample.png /tmp/after-restart.png && echo "persistence OK"
```

**Маркеры успеха.**

- `catalog.bin` (~КБ) и шарды (`node_*/<hex>/<hex>.shard`) целы.
- После перезапуска `GET` возвращает исходное побайтово.
- Идентичности узлов (`node_*/identity.key`) стабильны — pubkey-и
  совпадают со значениями до перезапуска.

---

## 12. Мультипроцессный кластер

**Цель.** Проверить «настоящий» распределённый режим — узлы как
отдельные процессы.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

Скрипт:

1. Запускает 8 процессов `holofs-node` со storage под
   `.cluster-data/node-N`.
2. Собирает их Ed25519 pubkey-и.
3. Генерирует admin keypair и подписывает whitelist.
4. Запускает gateway с `--whitelist`.

В другом терминале:

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# шарды распределены по 8 процессам
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Маркеры успеха.**

- Сумма шардов по узлам ≈ 444 (×3 канала × количество на слой).
- Ctrl-C по скрипту останавливает все 8 узлов и gateway.
- Повторный запуск того же скрипта (без стирания `.cluster-data/`)
  восстанавливает прежнее состояние — данные на диске целы.

---

## 13. TLS / mTLS на проводе

**Цель.** Включить опциональный TLS на трафике gateway ↔ узел.

```sh
# embedded-режим — самоподписанный CA генерируется автоматически
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

Лог: `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Взаимная аутентификация:

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**Что проверить.**

- Трафик на 9100..9139 больше не plain TCP — `tcpdump` на loopback
  показывает TLS-хендшейки (`16 03 ...`).
- PUT/GET/inspect работают так же, как без TLS.
- Без `--tls` соединения остаются plain — обратно совместимо.

Детали PKI и flow распределённого режима с сертификатами оператора
см. в [docs/ru/operations.md](./operations.md).

---

## 14. Метрики, логи, SSE

**Prometheus.**

```sh
curl http://127.0.0.1:8787/metrics
```

Ожидаемо:

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

**Структурированные логи.**

```sh
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data --log-format json --log info
```

Каждая строка — JSON с `timestamp`, `level`, `target`, `fields`.
Удобно для journald / fluentd / Vector / Loki.

**SSE-стрим health.**

```sh
curl -N http://127.0.0.1:8787/api/health/events
```

Примерно каждые 3 секунды приходит фрейм `event: health\ndata: {…}\n\n`
со снимком в JSON — именно он приводит в движение живой дашборд `/health`.

---

## 15. Регрессионные проверки

Три быстрые пробы, целящиеся в недавно исправленные проблемы.
Запускайте после любых изменений gateway или ingest-пайплайна.

### 15.1 Range на медиа

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Ожидаемо: `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. Не 200, не 416.

### 15.2 Inspect не теряет шарды

Откройте `http://127.0.0.1:8787/inspect/mandala.png` в браузере. Все
444 миниатюры должны отрисоваться. В логе:

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Ожидаемо **0**. Раньше это было ~41.

### 15.3 Крупные multipart-загрузки

```sh
# 3 МБ файл через escrow
dd if=/dev/urandom of=/tmp/3mb.bin bs=1024 count=3000 2>/dev/null
curl -i -F "file=@/tmp/3mb.bin" -F "k=3" -F "n=5" \
    http://127.0.0.1:8787/escrow/split 2>&1 | head -1

# 30 МБ через PUT
dd if=/dev/urandom of=/tmp/30mb.bin bs=1024 count=30000 2>/dev/null
curl -i -X PUT --data-binary @/tmp/30mb.bin \
    http://127.0.0.1:8787/big.bin 2>&1 | head -1
```

Оба должны вернуть `200`/`201`, а не `400 multipart read: Error parsing`.

---

## 16. Регрессионные проверки (продолжение)

### 16.1 Similar scope

Три «таблетки» scope-а вверху `/similar/<name>`: **все файлы** /
**текущая папка** / **текущая папка (рекурсивно)**.

```sh
# без ограничений (legacy-дефолт — top-10 по всему каталогу)
curl -s 'http://127.0.0.1:8787/similar/check.txt' \
  | grep -oE 'top similar \(<!>[0-9]+'

# только файлы внутри той же родительской папки
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=folder' \
  | grep -oE 'top similar \(<!>[0-9]+'

# поддерево родителя (root → весь каталог, эквивалентно `all`)
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=tree' \
  | grep -oE 'top similar \(<!>[0-9]+'
```

Scope липкий — клик по соседу переводит на его собственный URL
`/similar` с сохранением `?scope=` (и `?lang=`).

### 16.2 Фильтр каталога + удаление файла

Server-side фильтр на `/` и `/?p=<prefix>` через три query-параметра:
`q` (glob по имени, `*` = wildcard, сравнение по basename, регистронезависимо),
`from`, `to` (`YYYY-MM-DD`, диапазон по `created_at_unix`).

```sh
# все PNG-файлы
curl -s 'http://127.0.0.1:8787/?q=*.png' \
  | grep -oE 'class="tree-leaf' | wc -l

# комбинация: текстовые файлы, добавленные в 2026
curl -s 'http://127.0.0.1:8787/?q=*.txt&from=2026-01-01' \
  | grep -oE 'class="tree-leaf' | wc -l
```

Древовидный вид сохраняет каталоги-предки любого оставшегося листа,
чтобы пути оставались навигабельными. Legacy-записи с
`created_at_unix=0` (HOLOFSM6/HOLOFSM7) всегда проходят любой
фильтр по дате.

Удаление файла — form-POST-зеркало существующего `rmdir_form`:

```sh
curl -s -X PUT 'http://127.0.0.1:8787/tmp-delete-me.txt' \
  -H 'Content-Type: text/plain' --data 'will be deleted'
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST 'http://127.0.0.1:8787/api/rm' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data 'path=tmp-delete-me.txt&return_to=/'
# ожидаем 303 (редирект на return_to при успехе)
```

Строки-листья дерева рендерят небольшую кнопку `✕` с подтверждением.

### 16.3 Локализованный date picker

Нативный `<input type="date">` в панели фильтра несёт атрибут `lang`,
совпадающий с локалью страницы; в Chromium-браузерах flatpickr-overlay
(грузится с jsdelivr) заменяет нативный picker, чтобы календарь
всегда говорил на языке страницы, а не на языке ОС.

Откройте `/?lang=ru`, кликните поле даты — заголовок календаря
на русском. Переключитесь на `/?lang=fr`, повторите — французский.
`value=…` в обе стороны — `YYYY-MM-DD` независимо от локали.

### 16.4 Smoke-тест MCP-сервера

Запустите кластер с токеном, чтобы включить write-инструменты:

```sh
HOLOFS_MCP_TOKEN=devtoken ./holofs-web \
  --storage ./holofs-data --addr 127.0.0.1:8787 \
  --log warn --log-format text
```

Инициируйте MCP-сессию и перечислите все инструменты:

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

Ожидаемо 12 имён: `diff_objects`, `find_similar`, `get_cluster_health`,
`get_object_health`, `inspect_object`, `inspect_shard`, `list_catalog`,
`mkdir`, `mv_object`, `put_object_text`, `read_object_text`, `rmdir`.

Auth-гейтинг:

```sh
# нет заголовка → 401
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

Поверхность resources:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | grep -oE '"uri":"holofs:///[^"]+"' | head -5
```

Про подключение к Claude Code см. [api.md § 5](./api.md#5-mcp-сервер).

---

## 17. Wavelet-операции

Обе операции работают поверх существующего MCP-эндпоинта (`/mcp`) —
используйте ту же сессию, что и в § 16.4. Установите
`HOLOFS_MCP_TOKEN` перед стартом кластера, чтобы форма `save_as`
работала.

### 17.1 Wavelet mix

Собрать гибридный PNG из двух совместимых изображений, сохранить
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

# Забрать и осмотреть гибрид — должно быть обычным PNG.
curl -s -o /tmp/hybrid.png 'http://127.0.0.1:8787/hybrid.png'
file /tmp/hybrid.png
```

`file /tmp/hybrid.png` должен сообщить о реальном PNG-изображении
ожидаемых размеров.

Ошибки совместимости — несовместимые формы / k / параметры на слой
дают `BadRequest`:

```sh
# Смешивание image с text → BadRequest от проверки kind.
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"check.txt","split":0}}}' \
  | grep -oE '"message":"[^"]*"' | head -1
```

### 17.2 Фильтр по слоям для аудио

Отрендерить аудиообъект, сохранив только бас (L0), сохранить как
новую запись каталога:

```sh
# Предполагается, что какой-то `track.wav` уже загружен ранее.
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

`keep_layers:[]` или сброс всех слоёв → `BadRequest` (выход был бы
тишиной).

### 17.3 Inline-режим «без копии»

Опустите `save_as`, чтобы получить байты inline как base64-blob — полезно,
когда LLM должна посмотреть на результат, не оставляя артефакта в
каталоге:

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

`bytes_len` сообщает размер PNG; `saved_as` должно отсутствовать.

---

## 18. Быстрый старт с образцовым деревом

`tools/test-data/` содержит четыре части, которые из свежего чекаута
доводят до «каждая фича проиграна, каждая страница наполнена» без
необходимости вручную готовить входные файлы:

```
tools/test-data/
├── generate-samples.py    # детерминированный Python 3.10+ без зависимостей
├── clean-cluster.sh       # стирает catalog + shards + embeddings + versions
├── upload-samples.sh      # PUT-ит образцовое дерево с сохранением иерархии
└── run-tests.sh           # end-to-end smoke по стадиям
```

### 18.1 Сгенерировать дерево

```sh
python3 tools/test-data/generate-samples.py
# → wrote 38 samples (1,862,535 bytes) under <repo>/samples
```

Вывод живёт под `./samples/` (в .gitignore). Все байты детерминированы —
повторный запуск с теми же аргументами даёт идентичные файлы, так что
version-controlled тесты можно закрепить за конкретными хешами.

Иерархия:

```
samples/
  photos/{landscapes,abstract,brand-pairs}/*.png
  audio/{music,effects,silence}/*.wav
  docs/{notes,spec,legal}/{*.txt,*.md,*.json}
  binaries/{archives,blobs}/{*.zip,*.tar,*.bin}
```

Папка brand-pairs содержит намеренные почти-дубликаты
(`logo-N.png` + `logo-N-wm.png`), чтобы колонка robust-copy на
`/similar` давала хиты.

### 18.2 Чистый рестарт

```sh
tools/test-data/clean-cluster.sh
# (FORCE=1 пропускает подтверждение)

./target/release/holofs-web \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --enable-embed \
    --enable-versions &
```

Без `--enable-embed` страница `/search` рендерит баннер «embed
disabled». Без `--enable-versions` PUT-ы, замещающие существующий
объект, вычищают прежние шарды (без архива).

### 18.3 Залить дерево

```sh
tools/test-data/upload-samples.sh
```

Скрипт сначала делает mkdir на все префикс-папки (чтобы `/?p=<dir>`
работал сразу), затем PUT-ит каждый файл. В конце запрашивает
`/api/stats` и печатает новые тоталы каталога — ожидаем
`objects_total = 38 + <directory_markers>` (mkdir-ы скрипта тоже
считаются как записи каталога).

### 18.4 End-to-end smoke

```sh
tools/test-data/run-tests.sh
```

Что он обходит:

| Стадия  | Проверка                                                    |
|---------|-------------------------------------------------------------|
| 9       | `/` + `/?p=<folder>` для каждого подкаталога                |
| 12.6    | `/mix?a=<image>` рендерит композитор wavelet-mix            |
| 12.7    | `/health/<name>` per-file метрики                           |
| 12.7    | `/about` маркетинговая страница                             |
| 12.8/9  | `/search` UI + `/api/search?band=<any|coarse|mid|full>`     |
| 13.0    | `/similar/<brand-pair logo>` включает колонку robust-copy   |
| 13.1    | `/holo/<name>` + `/preview/stream/<name>` multipart         |
| 13.2    | `/api/spotlight.png?mode=spatial`                           |
| 14.1    | `/api/spotlight.png?mode=coeff`                             |
| 13.4    | PUT дважды → `/versions/<name>` показывает архивированный ряд |
| 14.0/3  | `POST /api/gc` возвращает `GcReport` JSON                   |

Каждая проверка печатает `✓` / `✗`; exit-код скрипта ненулевой,
если хотя бы одна проверка провалилась.

---

## 19. Страница per-file метрик

**Цель**: убедиться, что блок «Unique metrics» под `/health/<name>`
заполняется корректно.

**Шаги**:

1. Выберите любое изображение из образцового дерева, например
   `photos/landscapes/mountain.png`.
2. Откройте `http://127.0.0.1:8787/health/photos/landscapes/mountain.png`
   в браузере, либо дёрните API напрямую curl-ом:

   ```sh
   # POST — эндпоинт — leptos server fn, аргумент name едет в теле
   # формы, не в query. GET возвращает 405 Method Not Allowed.
   curl -s -X POST -d 'name=photos/landscapes/mountain.png' \
        http://127.0.0.1:8787/api/file_metrics \
        | python3 -m json.tool
   ```

**Ожидаемый payload**: `FileMetricsView` с:

- `total_shards_in_file` ≈ `unique_shards_in_file` (дедупликация на PUT
  не сжимает внутри RLNC-кодирования одного файла).
- `catalog_total_shards` ≥ `total_shards_in_file`.
- `originality_pct` где-то в `[0, 100]`; изображение из образцового
  дерева без общей структуры должно быть близко к 100.
- `originality_per_layer` — это `Vec<f32>` с `nlayers` записями.
- `layer_energy` заполнено для image / audio; `None` для text / opaque.
- `audio_bands` присутствует только при `kind == "audio"`.
- `neighbours` пуст, если каталог не содержит те же байты под другим
  именем.

**Проверка brand-pair**: против `photos/brand-pairs/logo-1.png` массив
`neighbours[]` должен содержать `photos/brand-pairs/logo-1-wm.png` как
**верхнюю** запись (наивысший `shared_total`) с ненулевым
`shared_per_layer[0]` — то есть систематические шарды слоя 0
(LL / грубого) выживают побайтово несмотря на уголок с watermark-ом.
Прочие изображения в каталоге показывают `shared_per_layer[0] == 0`.
Именно это перекрытие слоя 0 питает счёт robust-copy. Про формульный
caveat на синтетических тест-данных см. § 21.

---

## 20. CLIP-семантический поиск + bands

**Пререк**: сервер запущен с `--enable-embed`. На первом вызове gateway
скачивает ~155 МиБ весов CLIP из HuggingFace в
`~/.cache/huggingface/hub`; последующие перезапуски мгновенны.

**Пакетный индекс** (нужен один раз после чистого рестарта):

```sh
curl -s -X POST http://127.0.0.1:8787/api/embed_all
# → {"new":<N>,"skipped":<M>}
```

`new` считает записи каталога, которые впервые получили embedding;
`skipped` — картинки, чей `(data_cid, band)` уже был в
`embeddings.bin` (то же содержимое, загруженное под разными путями).

**Запрос по бэндам**:

```sh
for band in any coarse mid full; do
  echo "--- band=$band ---"
  curl -s "http://127.0.0.1:8787/api/search?q=mountain&band=$band&limit=3" \
    | python3 -m json.tool
done
```

**Ожидаемые исходы**:

- `band=any` возвращает бэнд с наивысшим счётом на файл (дедуп по имени).
- `band=coarse` ранжирует по силуэту / цветовому blob-у — фотографии
  ландшафтов с горизонтом должны всплывать вверх.
- `band=full` ранжирует по текстуре — шумовые / pixel-block абстракции
  перетасовываются.
- `band=mid` посередине — градиентные картинки должны получить хороший
  скор.

**UI-поверхность**: `/search?q=mountain&band=any` показывает card-grid,
где грубая миниатюра каждой карточки cross-fade-ится в полное
разрешение. Карточка несёт цветной бэйдж бэнда (синий = coarse,
фиолетовый = mid, розовый = full).

---

## 21. Колонка robust-copy на `/similar`

**Цель**: детектировать пары «структура совпадает, детали отличаются»
(сигнатура watermark / re-encode / лёгкий ретуш).

**Шаги**:

1. Откройте `/similar/photos/brand-pairs/logo-1.png`.
2. Прокрутите до таблицы «shard overlaps».

**Ожидаемо**:

- `photos/brand-pairs/logo-1-wm.png` — **верхний сосед** (наивысший
  `shared shards`) — подтверждает механизм: локализованный
  watermark в правом нижнем углу сохраняет большую часть
  систематических шардов LL (слой 0), так что 39+ из этих 192 шардов
  слоя 0 хешируются идентично между базой и watermark-вариантом.
  Никакая несвязанная картинка (mandala, gradient, другой brand) не
  делит ни одного шарда слоя 0.
- `low-band %` > 0 (перекрытие слоя 0).

**Caveat по счёту** (ограничение синтетических тест-данных, а не баг
фичи): числовое значение `robust copy?` на посеянном образцовом
дереве **отрицательное** для каждой brand-пары, и +30 глиф-предупреждение
watermark-а здесь не загорается. Причина: gateway апскейлит 256×256
сэмпл-PNG до своего рабочего разрешения 512×512 перед кодированием;
bilinear/bicubic апсэмплинг делает самый мелкий Haar-бэнд (слой 3)
почти полностью нулевым для каждой гладкой синтетической картинки.
K=16 систематических шардов над этими нулями хешируются в одинаковое
«all-zero» значение через **все** картинки образцового дерева, так
что каждая пара получает baseline ~36% `high-band %`, который заваливает
формулу скоры. На реальных фотографиях с богатыми высокими частотами
скор чисто пересекает +30; на этом тест-сете относитесь к
**верхнему ранку + ненулевому перекрытию слоя 0** как к сигналу
успеха, а не к абсолютному числу.

Дёрнуть подлежащую server-функцию через страницу (только браузеры):

```sh
curl -s 'http://127.0.0.1:8787/similar/photos/brand-pairs/logo-1.png' \
  | grep -oE 'robust_copy_score":-?[0-9.]+'
```

---

## 22. Streaming-голограмма

**Цель**: подтвердить, что `/preview/stream/<name>` возвращает multipart-
тело, а браузерная страница `/holo/<name>` работает.

**Curl-проба**:

```sh
curl -sI 'http://127.0.0.1:8787/preview/stream/photos/abstract/mandala-a.png'
# Content-Type должен быть: multipart/x-mixed-replace; boundary=hololayer-```

**Браузер**:

1. Откройте `/holo/photos/abstract/mandala-a.png`.
2. Force-reload (Cmd+Shift+R), чтобы обойти per-(name, layer) PNG-кеш.
3. Смотрите, как картинка наглядно резчает — первый кадр за ~десятки
   миллисекунд, каждый следующий добавляет детали одного DWT-слоя.

**Caveat**: последующие визиты попадают в кеш и ощущаются мгновенно.
JavaScript-free `<img>`-своп опирается на `multipart/x-mixed-replace`,
который Chrome и Firefox обрабатывают корректно.

---

## 23. Режимы holographic spotlight

**Цель**: отрендерить один и тот же ROI двумя способами и визуально
сравнить.

```sh
img=photos/landscapes/mountain.png
for mode in spatial coeff; do
  curl -s -o "/tmp/spot-$mode.png" \
       "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=$mode"
done
file /tmp/spot-*.png
md5 /tmp/spot-*.png    # ожидаем разные хеши
```

**Ожидаемо**: два PNG одинаковых размеров, но разных байтов.

- `spatial` сохраняет область вне ROI как размытую-но-видимую
  L0-реконструкцию.
- `coeff` держит пиксели вне ROI близко к чёрному (Haar reverse-map
  зануляет каждый коэффициент, не касающийся ROI).

**Заголовки**:

```sh
curl -sI \
  "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=coeff" \
  | grep -i 'x-holofs'
```

`x-holofs-roi-px` эхает pixel-ROI после clamp-а; `x-holofs-decode-ms`
сообщает серверную работу; `x-holofs-bytes-downloaded` информативно
(это превратится в реальную экономию трафика для `?mode=coeff` на
replicated-объектах).

**UI**: `/spotlight?a=<image>` даёт переключатель режимов + пресеты
ROI + форму пользовательских координат.

---

## 24. Per-object versioning

**Пререк**: сервер запущен с `--enable-versions`. Версионированные PUT-ы
ПРОПУСКАЮТ обычное вычищение шардов, так что хранилище растёт
монотонно, пока флаг включён. Запустите `/api/gc` (сценарий 25) для
освобождения.

**Шаги**:

1. Выберите целевое имя, например
   `samples/photos/abstract/mandala-a.png`, которое вы уже загрузили.
2. Загрузите другое изображение по тому же пути:

   ```sh
   curl -sf -X PUT \
        --data-binary @samples/photos/abstract/mandala-b.png \
        http://127.0.0.1:8787/photos/abstract/mandala-a.png
   ```

3. Осмотрите историю:

   ```sh
   open 'http://127.0.0.1:8787/versions/photos/abstract/mandala-a.png'
   ```

   Ожидайте как минимум один архивированный ряд с текущей датой.
   Префикс CID должен совпадать с исходной загрузкой, а не с заменой.

4. Нажмите «restore» на архивированном ряду. Подтвердите в диалоге.

   ```sh
   # Или через curl:
   curl -X POST \
        -d 'name=photos/abstract/mandala-a.png&id=v<TS>_<CIDSHORT>' \
        http://127.0.0.1:8787/api/restore
   ```

5. Забрать картинку заново:

   ```sh
   md5 <(curl -sf http://127.0.0.1:8787/photos/abstract/mandala-a.png)
   ```

**Ожидаемо**: MD5 после восстановления совпадает с MD5 до замены;
замена теперь сама архивирована (restore обратим).

---

## 25. GC орфанных шардов + GC embedding-ов

**Цель**: убедиться, что gateway возвращает шарды, на которые больше не
ссылается ни один живой manifest или архив версий, И вычищает
устаревшие embedding-и из `embeddings.bin`.

**Шаги**:

1. Проведите один цикл PUT-замены (сценарий 24), чтобы в кластере
   появились потенциально orphan-шарды.
2. Удалите side-файлы версий для этого имени (имитирует оператора,
   удаляющего историю):

   ```sh
   rm -rf holofs-data/versions/photos__abstract__mandala-a.png
   ```

   (Скрипт `clean-cluster.sh` делает то же самое оптом.)

3. Запустите GC:

   ```sh
   curl -s -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool
   ```

**Ожидаемо**:

- `purged_total` > 0 (прежние шарды теперь без ссылок).
- `embeddings_dropped` > 0, если в индексе жили устаревшие CID-ы.
- `embeddings_kept` совпадает с числом живых `(data_cid, band)` записей.
- У каждого узла `ok: true`, поле `error` не установлено.
- `duration_ms` обычно < 100 мс на dev-кластере.

**Проверка конкурентности** (опционально): запустите долгий PUT + GC
параллельно и убедитесь, что оба успешны. Барьер RwLock в `Gateway`
должен их сериализовать — GC подождёт завершения PUT, затем
выполнится один.

```sh
( curl -sf -X PUT --data-binary @samples/photos/landscapes/ocean.png \
       http://127.0.0.1:8787/race-test.png ) &
sleep 0.2
( curl -sf -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool ) &
wait
# Оба должны завершиться; `duration_ms` GC включит время ожидания.
```

---

## 26. Сценарии надёжности

### 26.1 Счётчики auto-repair-on-read

Цель: убедиться, что retry-плечо `decode_with_autorepair` двигает
счётчики в `/api/stats` только когда есть что чинить.

```sh
# Baseline — свежий кластер, здоров.
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total, .auto_repair_failures_total'
# 0
# 0

# Лёгкая деградация — убить 3 из 40 узлов (сильно ниже избыточности слоя 3).
for i in 0 1 2; do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -s http://127.0.0.1:8787/photo.png -o /dev/null
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total'
# Всё ещё 0 — auto-repair НЕ должен срабатывать при лёгких потерях.

# Тяжёлая деградация — убить 60% кластера.
for i in $(seq 3 24); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -s http://127.0.0.1:8787/photo.png -o /dev/null
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total, .auto_repair_failures_total'
# Хотя бы один счётчик ДОЛЖЕН быть ≥ 1.
```

Автоматически покрыто `crates/holofs-e2e/tests/auto_repair_e2e.rs`.

### 26.2 Фоновый scrub проактивно чинит

Цель: доказать, что scrub ловит смещение placement раньше, чем
пользователи.

```sh
# Установить scrub на 15 с для демо (по умолчанию 600 с).
HOLOFS_SCRUB_INTERVAL=15 \
  cargo run --release --bin holofs-web
# дождитесь первого тика:
sleep 20
curl -s http://127.0.0.1:8787/api/stats | jq '.scrub_runs_total'
# 1+ — scrub_repairs_total остаётся 0 на здоровом кластере.
```

Более шумное демо — в
`crates/holofs-e2e/tests/reliability_repair.rs::prometheus_metrics_expose_auto_repair_gauges`.

### 26.3 Cluster-degraded → 503, а не паника

Цель: `place_shard` раньше падал ассертом на пустом live-наборе,
роняя gateway. Теперь PUT против полностью упавшего кластера
возвращает чистый 503.

```sh
# Убить каждый узел.
for i in $(seq 0 39); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -i -X PUT --data-binary @some.png http://127.0.0.1:8787/test.png
# HTTP/1.1 503 Service Unavailable
# content-type: text/plain
# cluster has no live nodes
```

После оживления узлов (`POST /admin/node` — toggle) тот же PUT
успешен с 2xx.

Покрыто `crates/holofs-e2e/tests/cluster_degraded.rs`.

### 26.4 Удаление версий + retention-cap

Цель: per-name история не растёт бесконечно.

```sh
HOLOFS_VERSIONS_KEEP_LAST=2 \
  cargo run --release --bin holofs-web -- --enable-versions

# PUT-ить четыре разных изображения под одним именем.
for body in a.png b.png c.png d.png; do
  curl -X PUT --data-binary @$body http://127.0.0.1:8787/test.png
done

# /api/versions_list — максимум 2 архива, сколько бы PUT-ов ни прошло.
curl -s -X POST -d "name=test.png" http://127.0.0.1:8787/api/versions_list | jq '.versions | length'
# 2

# Ручное удаление одного архива — счётчик падает до 1.
ID=$(curl -s -X POST -d "name=test.png" http://127.0.0.1:8787/api/versions_list | jq -r '.versions[0].id')
curl -X POST -d "name=test.png&id=$ID&return_to=/" http://127.0.0.1:8787/api/versions/delete
```

Покрыто `crates/holofs-e2e/tests/versions_lifecycle.rs`.

### 26.5 cd-в-папку в дереве каталога

Цель: клик по «open →» на папке показывает ТОЛЬКО её содержимое
на верхнем уровне, с breadcrumb-ом для навигации вверх.

```sh
# Засеять вложенное дерево (стандартный upload-скрипт):
tools/test-data/upload-samples.sh

# Откройте каталог на /. Разверните `photos/`, затем нажмите «open →» на
# `landscapes-xl`. URL становится `/?p=photos/landscapes-xl`, а дерево
# теперь показывает шесть picsum JPEG как верхнеуровневые записи — без
# соседних папок.
xdg-open http://127.0.0.1:8787/?p=photos/landscapes-xl  # linux
open http://127.0.0.1:8787/?p=photos/landscapes-xl      # macos
```

Inline формы upload + mkdir на каждой строке `<details>` укладывают
файлы в ту папку, на которую вы смотрели; форма загрузки корневого
тулбара привязывается к текущему префиксу `?p=<path>`.

Покрыто ручным smoke-ом в § 18 плюс тестами рендеринга каталога
под `crates/holofs-e2e/tests/ui_catalog.rs`.

### 26.6 Синтетические PNG-образцы декодируются чисто

Цель: баг 22-из-29-битых ушёл.

```sh
tools/test-data/clean-cluster.sh           # свежее хранилище
HOLOFS_NO_SEED=true \
  cargo run --release --bin holofs-web &
sleep 4
python3 tools/test-data/generate-samples.py
tools/test-data/upload-samples.sh

# Пройтись по каждому PNG / JPG под samples/ и сделать GET.
broken=0
for f in $(find samples -type f \( -name '*.png' -o -name '*.jpg' \)); do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:8787/${f#samples/}")
  [ "$code" != "200" ] && broken=$((broken+1))
done
echo "broken=$broken"
# broken=0
```

Детерминированный LSB-jitter, вбрасываемый `write_png`, гарантирует,
что высокочастотные DWT-шарды уникальны на файл даже на самых гладких
синтетических генераторах.

---

## Wrap-up

Чистый стоп:

```sh
pkill -f 'target/release/holofs-web'
# или Ctrl-C в терминале, где запущен кластер
```

Чистая стирка — сбросить всё состояние:

```sh
tools/test-data/clean-cluster.sh
# или вручную:
rm -rf ./holofs-data ./.cluster-data
```

Если что-то ведёт себя не так, сравните с описаниями выше и загляните:

- [docs/ru/operations.md](./operations.md) — конфигурация и эксплуатация
- [docs/ru/architecture.md](./architecture.md) — data flow PUT → GET
- [docs/ru/api.md](./api.md) — HTTP-API, формат wire-протокола, новые эндпоинты
- [docs/ru/threat-model.md](./threat-model.md) — покрываемые угрозы
- `tools/test-data/README.md` — использование образцового дерева и smoke-раннера
