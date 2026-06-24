//! Stage 10: UI internationalisation.
//!
//! Lightweight hand-rolled i18n with a single in-memory translation map.
//! Translations live in [`TRANSLATIONS`] (one row per English key, one
//! column per locale); helper [`t`] looks up the right string for the
//! current request, falling back to English when a translation is
//! missing.
//!
//! The current locale is exposed via Leptos context as [`LocaleSignal`]
//! and read by components through [`current_locale`]. Locale resolution
//! priority at request time:
//! 1. `?lang=<code>` in the URL (explicit user choice in the link),
//! 2. `holofs_lang` cookie (sticky from a previous switch),
//! 3. `Accept-Language` header (browser default),
//! 4. `"en"` (final fallback).
//!
//! Switching languages is a full navigation: the locale-switcher posts to
//! `/i18n/set?lang=<code>` which sets the cookie and 303-redirects back to
//! the referer. Client-side switching without a reload was deliberately
//! left out — it would require turning every translation site into a
//! reactive signal lookup, which is heavyweight for a small UI.

use leptos::prelude::*;

/// Reactive locale handle — `provide_context`'d at the top of the app and
/// read by [`current_locale`] / `t!()` everywhere else. Holds a
/// [`Signal<String>`] (the type-erased reactive container) so the producer
/// can be a `Memo`, a derived signal, or a plain `ReadSignal` without
/// changing this struct.
#[derive(Clone, Copy)]
pub struct LocaleSignal(pub Signal<String>);

/// Read the active locale code (`"en"`, `"ru"`, …). Falls back to `"en"`
/// when no [`LocaleSignal`] has been provided (e.g. unit tests).
pub fn current_locale() -> String {
    use_context::<LocaleSignal>()
        .map(|s| s.0.get())
        .unwrap_or_else(|| "en".to_string())
}

/// `true` for any locale we ship UI / docs translations for.
pub fn is_known_locale(l: &str) -> bool {
    matches!(l, "en" | "ru" | "de" | "fr" | "es")
}

/// Pick the best locale for the request. Caller is expected to feed the
/// `?lang=`, cookie, and `Accept-Language` values it has access to;
/// missing inputs are passed as `None`. Returns one of the five known
/// locales; never returns an unknown code.
pub fn resolve_locale(
    query_lang: Option<&str>,
    cookie_lang: Option<&str>,
    accept_language: Option<&str>,
) -> String {
    if let Some(q) = query_lang {
        if is_known_locale(q) {
            return q.to_string();
        }
    }
    if let Some(c) = cookie_lang {
        if is_known_locale(c) {
            return c.to_string();
        }
    }
    if let Some(al) = accept_language {
        for piece in al.split(',') {
            // `xx[-YY][;q=0.8]` — we only care about the primary subtag.
            let tag = piece.split(';').next().unwrap_or("").trim();
            let primary = tag.split('-').next().unwrap_or("").to_ascii_lowercase();
            if is_known_locale(&primary) {
                return primary;
            }
        }
    }
    "en".to_string()
}

/// Static translation table. **Keep keys stable** — they're the strings
/// the codebase looks up, not the English text itself. The English column
/// is also the implicit fallback when other columns are blank.
///
/// Adding a new key: append a `("key", ["en", "ru", "de", "fr", "es"])`
/// row. Adding a new locale: extend the inner array and update
/// [`is_known_locale`] + `LOCALE_ORDER` / `LOCALE_LABEL`.
pub const TRANSLATIONS: &[(&str, [&str; 5])] = &[
    // ---- topbar nav ------------------------------------------------------
    ("nav.catalog",
        ["catalog", "каталог", "Katalog", "catalogue", "catálogo"]),
    ("nav.health",
        ["health", "состояние", "Zustand", "santé", "estado"]),
    ("nav.escrow",
        ["escrow", "эскроу", "Treuhand", "séquestre", "depósito"]),
    ("nav.help",
        ["help", "помощь", "Hilfe", "aide", "ayuda"]),

    // ---- catalog page ----------------------------------------------------
    ("catalog.empty",
        ["Empty directory.",
         "Каталог пуст.",
         "Verzeichnis ist leer.",
         "Répertoire vide.",
         "Directorio vacío."]),
    ("catalog.empty_hint",
        ["Use the form above to create a sub-folder, or curl -X PUT to upload an object.",
         "Используйте форму выше, чтобы создать подпапку, или curl -X PUT для загрузки объекта.",
         "Erstelle einen Unterordner über das Formular oder lade per curl -X PUT ein Objekt hoch.",
         "Utilisez le formulaire ci-dessus pour créer un sous-dossier, ou curl -X PUT pour téléverser.",
         "Use el formulario para crear una subcarpeta, o curl -X PUT para subir un objeto."]),
    ("catalog.loading",
        ["loading catalog…", "загружаем каталог…", "Katalog wird geladen…",
         "chargement du catalogue…", "cargando catálogo…"]),
    ("catalog.load_failed",
        ["failed to load directory:", "не удалось загрузить каталог:",
         "Verzeichnis konnte nicht geladen werden:",
         "échec du chargement du répertoire :",
         "no se pudo cargar el directorio:"]),
    ("catalog.tree_intro",
        ["Click a folder to expand or collapse it. Use the [open →] link on any branch to jump into the focused view with upload / mkdir / delete controls.",
         "Кликайте на папку чтобы раскрыть или свернуть её. По ссылке [open →] на любой ветке откроется focus-вид с формами upload / mkdir / delete.",
         "Klicken Sie auf einen Ordner, um ihn auf- oder zuzuklappen. Über den [open →]-Link an einem Zweig gelangen Sie in die fokussierte Ansicht mit Upload- / mkdir- / delete-Steuerung.",
         "Cliquez sur un dossier pour le déplier ou le replier. Le lien [open →] de chaque branche ouvre la vue focalisée avec les contrôles upload / mkdir / delete.",
         "Haga clic en una carpeta para expandirla o contraerla. El enlace [open →] de cada rama abre la vista enfocada con los controles upload / mkdir / delete."]),
    ("catalog.back_to_tree",
        ["back to tree", "к дереву", "zurück zur Baumansicht",
         "retour à l'arbre", "volver al árbol"]),
    // Stage 11.21 / 11.22 — lazy tree
    ("tree.load_more",
        ["load more", "загрузить ещё", "mehr laden",
         "charger plus", "cargar más"]),
    ("tree.zoom_in",
        ["zoom in", "увеличить", "vergrößern", "agrandir", "ampliar"]),
    ("tree.zoom_out",
        ["zoom out", "уменьшить", "verkleinern", "réduire", "reducir"]),
    ("tree.expand_subtree",
        ["expand this folder", "развернуть эту папку",
         "diesen Ordner ausklappen", "déplier ce dossier",
         "expandir esta carpeta"]),
    ("tree.collapse_subtree",
        ["collapse this folder", "свернуть эту папку",
         "diesen Ordner einklappen", "replier ce dossier",
         "contraer esta carpeta"]),
    ("tree.empty_folder",
        ["(empty)", "(пусто)", "(leer)", "(vide)", "(vacío)"]),
    ("tree.expand_all",
        ["expand all", "раскрыть всё", "alle öffnen",
         "tout déplier", "expandir todo"]),
    ("tree.collapse_all",
        ["collapse all", "свернуть всё", "alle schließen",
         "tout replier", "contraer todo"]),
    ("tree.sort_by",
        ["sort", "сортировка", "Sortierung", "tri", "ordenar"]),
    ("tree.sort.name",
        ["name", "имя", "Name", "nom", "nombre"]),
    ("tree.sort.size",
        ["size", "размер", "Größe", "taille", "tamaño"]),
    ("tree.sort.kind",
        ["kind", "тип", "Art", "type", "tipo"]),
    ("tree.sort.date",
        ["date", "дата", "Datum", "date", "fecha"]),
    ("theme.toggle",
        ["toggle theme", "сменить тему", "Thema wechseln",
         "changer le thème", "cambiar tema"]),
    ("tree.new_folder",
        ["new folder", "новая папка", "neuer Ordner",
         "nouveau dossier", "carpeta nueva"]),

    // ---- mkdir / folder controls ----------------------------------------
    ("mkdir.placeholder",
        ["new folder name", "имя новой папки", "Name des neuen Ordners",
         "nom du nouveau dossier", "nombre de la carpeta nueva"]),
    ("mkdir.submit",
        ["+ folder", "+ папка", "+ Ordner", "+ dossier", "+ carpeta"]),
    ("upload.choose_file",
        ["choose file", "выбрать файл", "Datei wählen",
         "choisir un fichier", "elegir archivo"]),
    ("upload.no_file",
        ["no file selected", "файл не выбран", "keine Datei gewählt",
         "aucun fichier sélectionné", "ningún archivo seleccionado"]),
    ("upload.drop_here",
        ["drop a file here, or", "перетащите файл сюда, или",
         "Datei hierher ziehen, oder",
         "déposez un fichier ici, ou",
         "arrastre un archivo aquí, o"]),
    ("upload.name_placeholder",
        ["catalog name (optional)", "имя в каталоге (необязательно)",
         "Katalogname (optional)", "nom dans le catalogue (optionnel)",
         "nombre en el catálogo (opcional)"]),
    ("upload.submit",
        ["upload", "загрузить", "hochladen", "téléverser", "subir"]),
    ("upload.hint",
        ["Images and audio become holograms with graceful degradation; PDFs, ZIPs and other binaries are stored as opaque erasure-coded blobs.",
         "Изображения и аудио становятся голограммами с плавной деградацией; PDF, ZIP и прочие бинарники хранятся как opaque erasure-coded блобы.",
         "Bilder und Audio werden Hologramme mit weicher Degradation; PDFs, ZIPs und andere Binärdateien werden als opaque erasure-coded Blobs gespeichert.",
         "Les images et l'audio deviennent des hologrammes à dégradation progressive ; les PDF, ZIP et autres binaires sont stockés comme blobs erasure-coded opaques.",
         "Imágenes y audio se vuelven hologramas con degradación progresiva; PDF, ZIP y otros binarios se almacenan como blobs erasure-coded opacos."]),
    ("folder.open",
        ["open", "открыть", "öffnen", "ouvrir", "abrir"]),
    ("folder.delete",
        ["delete", "удалить", "löschen", "supprimer", "eliminar"]),
    ("folder.confirm_delete",
        ["rmdir: delete this folder?",
         "rmdir: удалить эту папку?",
         "rmdir: diesen Ordner löschen?",
         "rmdir : supprimer ce dossier ?",
         "rmdir: ¿eliminar esta carpeta?"]),
    ("folder.kind_label",
        ["folder", "папка", "Ordner", "dossier", "carpeta"]),

    // ---- breadcrumb / navigation ----------------------------------------
    ("breadcrumb.home",
        ["home", "корень", "Start", "accueil", "inicio"]),

    // ---- object card metadata + actions ---------------------------------
    ("card.shards",
        ["shards", "шарды", "Shards", "fragments", "fragmentos"]),
    ("card.action.full",
        ["full", "полный", "voll", "complet", "completo"]),
    ("card.action.play",
        ["play", "играть", "abspielen", "lire", "reproducir"]),
    ("card.action.open",
        ["open", "открыть", "öffnen", "ouvrir", "abrir"]),
    ("card.action.download",
        ["download", "скачать", "herunterladen", "télécharger", "descargar"]),
    ("card.action.preview",
        ["preview", "превью", "Vorschau", "aperçu", "vista previa"]),
    ("card.action.bass",
        ["bass", "бас", "Bass", "basse", "bajos"]),
    ("card.action.shards_link",
        ["shards →", "шарды →", "Shards →", "fragments →", "fragmentos →"]),
    ("card.action.health",
        ["health", "состояние", "Zustand", "santé", "estado"]),
    ("card.action.similar",
        ["similar", "похожие", "ähnlich", "similaires", "similares"]),

    // ---- catalog filter bar (Stage 11.17) -------------------------------
    ("filter.name_label",
        ["filter by name", "фильтр по имени", "Filter nach Name",
         "filtre par nom", "filtro por nombre"]),
    ("filter.name_placeholder",
        ["name (use * as wildcard)", "имя (* — подстановка)",
         "Name (* als Platzhalter)", "nom (* comme joker)",
         "nombre (* como comodín)"]),
    ("filter.from",
        ["from", "с", "von", "du", "desde"]),
    ("filter.to",
        ["to", "по", "bis", "au", "hasta"]),
    ("filter.apply",
        ["apply", "применить", "anwenden", "appliquer", "aplicar"]),
    ("filter.clear",
        ["clear", "сбросить", "zurücksetzen", "réinitialiser", "limpiar"]),
    ("file.delete_confirm",
        ["Delete this file? This cannot be undone.",
         "Удалить этот файл? Это действие необратимо.",
         "Diese Datei löschen? Das lässt sich nicht rückgängig machen.",
         "Supprimer ce fichier ? L'action est irréversible.",
         "¿Eliminar este archivo? La acción es irreversible."]),
    ("file.delete",
        ["delete", "удалить", "löschen", "supprimer", "eliminar"]),

    // ---- similar page: scope picker (Stage 11.16) ----------------------
    ("similar.scope.label",
        ["scope:", "область:", "Bereich:", "portée :", "ámbito:"]),
    ("similar.scope.all",
        ["all files", "все файлы", "alle Dateien",
         "tous les fichiers", "todos los archivos"]),
    ("similar.scope.folder",
        ["current folder", "текущая папка", "aktueller Ordner",
         "dossier actuel", "carpeta actual"]),
    ("similar.scope.tree",
        ["current folder (recursive)", "текущая папка (рекурсивно)",
         "aktueller Ordner (rekursiv)", "dossier actuel (récursif)",
         "carpeta actual (recursivo)"]),

    // ---- escrow ---------------------------------------------------------
    ("escrow.title",
        ["holographic key escrow",
         "голографический эскроу ключей",
         "holographische Schlüssel-Treuhand",
         "séquestre holographique de clés",
         "depósito holográfico de claves"]),
    ("escrow.split_h",
        ["split a file", "разбить файл", "Datei aufteilen",
         "fractionner un fichier", "dividir un archivo"]),
    ("escrow.recover_h",
        ["recover a file", "восстановить файл", "Datei wiederherstellen",
         "récupérer un fichier", "recuperar un archivo"]),
    ("escrow.btn.split",
        ["split", "разбить", "aufteilen", "fractionner", "dividir"]),
    ("escrow.btn.recover",
        ["recover", "восстановить", "wiederherstellen", "récupérer", "recuperar"]),

    // ---- escrow page intro + form labels (Stage 11.19b) ----------------
    ("escrow.intro_p1",
        ["Split any file into n shares so that any k of them can reconstruct \
          it (Shamir-style secret sharing on RLNC). Distribute the shares \
          across trusted people or places — fewer than k shares leak no \
          information (information-theoretic, not just hard-to-break).",
         "Разбейте любой файл на n долей так, чтобы любые k из них могли его \
          восстановить (Шамировское разделение секрета на RLNC). \
          Распределите доли между доверенными людьми или местами — меньше k \
          долей не дают никакой информации о файле (теоретико-информационно, \
          а не «трудно взломать»).",
         "Eine Datei in n Anteile aufteilen, so dass beliebige k davon sie \
          rekonstruieren können (Shamir-Geheimnisteilung auf RLNC). Anteile \
          an vertraute Personen oder Orte verteilen — weniger als k Anteile \
          verraten keinerlei Information (informationstheoretisch, nicht nur \
          schwer zu brechen).",
         "Fractionner un fichier en n parts pour que k d'entre elles \
          permettent de le reconstruire (partage de secret façon Shamir sur \
          RLNC). Distribuer les parts à des personnes ou lieux de confiance \
          — moins de k parts ne révèlent aucune information (théorie de \
          l'information, pas juste « difficile à casser »).",
         "Divide un archivo en n partes de modo que cualquier k de ellas \
          permitan reconstruirlo (compartición de secreto tipo Shamir sobre \
          RLNC). Distribuye las partes entre personas o lugares de confianza \
          — menos de k partes no filtran información alguna \
          (teoría-informacional, no solo difícil de romper)."]),
    ("escrow.intro_p2",
        ["Shares live in gateway memory only. After the gateway restarts \
          they vanish, so download them immediately.",
         "Доли живут только в памяти gateway. После перезапуска gateway они \
          исчезают, так что скачивайте их сразу.",
         "Anteile leben nur im Speicher der Gateway. Nach einem Neustart \
          sind sie weg — also sofort herunterladen.",
         "Les parts ne vivent qu'en mémoire de la gateway. Au redémarrage \
          elles disparaissent — téléchargez-les tout de suite.",
         "Las partes viven solo en la memoria del gateway. Tras reiniciarlo \
          desaparecen, así que descárgalas de inmediato."]),
    ("escrow.label.file",
        ["file", "файл", "Datei", "fichier", "archivo"]),
    ("escrow.label.k",
        ["k (threshold)", "k (порог)", "k (Schwelle)",
         "k (seuil)", "k (umbral)"]),
    ("escrow.label.n",
        ["n (total shares)", "n (всего долей)", "n (Anteile gesamt)",
         "n (parts totales)", "n (partes totales)"]),
    ("escrow.label.shares",
        [".holoshare files (≥ k)", ".holoshare файлы (≥ k)",
         ".holoshare-Dateien (≥ k)", "fichiers .holoshare (≥ k)",
         "archivos .holoshare (≥ k)"]),
    ("escrow.choose_file",
        ["choose file", "выбрать файл", "Datei wählen",
         "choisir un fichier", "elegir archivo"]),
    ("escrow.choose_shares",
        ["choose .holoshare files", "выбрать .holoshare файлы",
         ".holoshare-Dateien wählen", "choisir des fichiers .holoshare",
         "elegir archivos .holoshare"]),
    ("escrow.no_file",
        ["no file chosen", "файл не выбран", "keine Datei ausgewählt",
         "aucun fichier choisi", "ningún archivo elegido"]),
    ("escrow.no_shares",
        ["no shares chosen", "доли не выбраны", "keine Anteile gewählt",
         "aucune part choisie", "ninguna parte elegida"]),
    ("escrow.drop_file",
        ["drop a file here, or click to choose",
         "перетащите файл сюда или нажмите чтобы выбрать",
         "Datei hier ablegen oder klicken zum Wählen",
         "déposez un fichier ici, ou cliquez pour choisir",
         "arrastra un archivo aquí, o pulsa para elegir"]),
    ("escrow.drop_shares",
        ["drop .holoshare files here, or click to choose",
         "перетащите .holoshare файлы сюда или нажмите чтобы выбрать",
         ".holoshare-Dateien hier ablegen oder klicken zum Wählen",
         "déposez des fichiers .holoshare ici, ou cliquez pour choisir",
         "arrastra archivos .holoshare aquí, o pulsa para elegir"]),
    ("escrow.files_chosen",
        ["{n} files chosen", "выбрано {n} файлов",
         "{n} Dateien gewählt", "{n} fichiers choisis",
         "{n} archivos elegidos"]),

    // ---- escrow split-result page (Stage 11.19b) ------------------------
    ("escrow.result.title",
        ["file split into {n} shares · need {k} to recover",
         "файл разделён на {n} долей · нужно {k} для восстановления",
         "Datei in {n} Anteile geteilt · {k} zur Wiederherstellung nötig",
         "fichier fractionné en {n} parts · {k} requises pour récupérer",
         "archivo dividido en {n} partes · se necesitan {k} para recuperar"]),
    ("escrow.result.source",
        ["source:", "источник:", "Quelle:", "source :", "origen:"]),
    ("escrow.result.id",
        ["escrow_id", "escrow_id", "escrow_id", "escrow_id", "escrow_id"]),
    ("escrow.result.warning",
        ["Download each share immediately and distribute. Shares live in \
          gateway memory and vanish on restart.",
         "Скачайте каждую долю сразу и распределите. Доли живут в памяти \
          gateway и исчезают при перезапуске.",
         "Jeden Anteil sofort herunterladen und verteilen. Anteile leben im \
          Speicher der Gateway und verschwinden beim Neustart.",
         "Téléchargez chaque part immédiatement et distribuez-les. Les \
          parts vivent en mémoire de la gateway et disparaissent au \
          redémarrage.",
         "Descarga cada parte de inmediato y distribúyelas. Las partes \
          viven en la memoria del gateway y desaparecen al reiniciar."]),
    ("escrow.result.col.idx",
        ["#", "#", "#", "#", "#"]),
    ("escrow.result.col.file",
        ["file", "файл", "Datei", "fichier", "archivo"]),
    ("escrow.result.col.size",
        ["size", "размер", "Größe", "taille", "tamaño"]),
    ("escrow.result.action.download",
        ["↓ download", "↓ скачать", "↓ Download",
         "↓ télécharger", "↓ descargar"]),
    ("escrow.result.back",
        ["← back to escrow", "← назад к escrow", "← zurück zum Escrow",
         "← retour à l'escrow", "← volver al escrow"]),
    ("escrow.result.title_tag",
        ["escrow · holofs", "escrow · holofs", "Escrow · holofs",
         "escrow · holofs", "escrow · holofs"]),

    // ---- help page ------------------------------------------------------
    ("help.section.docs",
        ["Documentation", "Документация", "Dokumentation",
         "Documentation", "Documentación"]),
    ("help.section.language",
        ["Language", "Язык", "Sprache", "Langue", "Idioma"]),
    ("help.loading",
        ["loading…", "загрузка…", "Laden…", "chargement…", "cargando…"]),
    ("help.load_failed",
        ["failed to load doc:", "не удалось загрузить документ:",
         "Dokument konnte nicht geladen werden:",
         "échec du chargement du document :",
         "no se pudo cargar el documento:"]),

    // ---- generic --------------------------------------------------------
    ("generic.not_found",
        ["not found", "не найдено", "nicht gefunden",
         "introuvable", "no encontrado"]),

    // ---- /mix page (Stage 12.6) -----------------------------------------
    ("mix.link_label",
        ["mix →", "микс →", "Mix →", "mix →", "mix →"]),
    ("mix.title_prefix",
        ["wavelet mix from", "wavelet-микс от",
         "Wavelet-Mischung von", "mix wavelet à partir de",
         "mezcla wavelet desde"]),
    ("mix.intro",
        ["Pick a second image and a DWT split layer. Layers 0..=split come \
          from the source on the left; layers above the split come from the \
          one you pick. Low layers carry structure, high layers carry fine \
          detail — the smaller the split, the more of B you see.",
         "Выбери второе изображение и DWT split-слой. Слои 0..=split берутся \
          из источника слева; слои выше split — из того, что ты выбрал. \
          Низкие слои несут структуру, высокие — мелкие детали — чем меньше \
          split, тем больше виден B.",
         "Wähle ein zweites Bild und einen DWT-Split-Layer. Layer 0..=split \
          kommen von der Quelle links; Layer über dem Split aus der \
          gewählten. Tiefe Layer tragen Struktur, hohe die feinen Details — \
          je kleiner der Split, desto mehr von B ist sichtbar.",
         "Choisissez une seconde image et un layer DWT de split. Les layers \
          0..=split viennent de la source de gauche ; les layers au-dessus \
          viennent de celle que vous choisissez. Bas layers = structure, \
          hauts = détails — plus le split est petit, plus on voit B.",
         "Elige una segunda imagen y una capa DWT de split. Las capas \
          0..=split vienen del origen de la izquierda; las superiores vienen \
          de la que elijas. Las capas bajas llevan estructura, las altas \
          detalle — cuanto menor el split, más se ve B."]),
    ("mix.field.b",
        ["partner (B)", "партнёр (B)", "Partner (B)",
         "partenaire (B)", "compañero (B)"]),
    ("mix.field.split",
        ["split layer", "split-слой", "Split-Layer",
         "layer de split", "capa de split"]),
    ("mix.field.dest",
        ["save as", "сохранить как", "speichern als",
         "enregistrer sous", "guardar como"]),
    ("mix.pick_b",
        ["— pick an image —", "— выбери изображение —",
         "— Bild auswählen —", "— choisir une image —",
         "— elegir imagen —"]),
    ("mix.apply",
        ["preview", "превью", "Vorschau", "aperçu", "vista previa"]),
    ("mix.save",
        ["save to catalog", "сохранить в каталог",
         "in den Katalog speichern", "enregistrer dans le catalogue",
         "guardar en el catálogo"]),
    ("mix.preview_h",
        ["result preview", "превью результата", "Vorschau des Ergebnisses",
         "aperçu du résultat", "vista previa del resultado"]),
    ("mix.preview_alt",
        ["wavelet mix preview", "превью wavelet-микса",
         "Wavelet-Mix-Vorschau", "aperçu mix wavelet",
         "vista previa mezcla wavelet"]),
    ("mix.missing_a",
        ["No source A — open this page via the `mix →` link on an image row.",
         "Источник A не задан — открой эту страницу через ссылку `mix →` на строке картинки.",
         "Keine Quelle A — diese Seite über den `mix →`-Link einer Bildzeile öffnen.",
         "Source A absente — ouvrir cette page depuis le lien `mix →` sur une ligne image.",
         "Sin origen A — abre esta página desde el enlace `mix →` de una fila de imagen."]),
    ("generic.load_failed",
        ["failed to load:", "не удалось загрузить:", "Laden fehlgeschlagen:",
         "échec du chargement :", "no se pudo cargar:"]),
    ("generic.loading",
        ["loading…", "загрузка…", "Lädt…", "chargement…", "cargando…"]),
    ("generic.back_to_catalog",
        ["catalog", "каталог", "Katalog", "catalogue", "catálogo"]),

    // ---- /similar page (Stage 11.19) ------------------------------------
    ("similar.title_prefix",
        ["find similar to", "похожие на", "ähnlich zu",
         "similaires à", "similares a"]),
    ("similar.loading",
        ["loading similar…", "поиск похожих…", "Lade Ähnliche…",
         "chargement des similaires…", "buscando similares…"]),
    ("similar.method_label",
        ["method:", "метод:", "Methode:", "méthode :", "método:"]),
    ("similar.method.minhash_name",
        ["bottom-K MinHash on 5-shingles",
         "bottom-K MinHash на 5-шинглах",
         "Bottom-K-MinHash auf 5-Shingles",
         "MinHash bottom-K sur 5-shingles",
         "MinHash bottom-K sobre 5-shingles"]),
    ("similar.method.minhash_fp",
        [". fingerprint (first 8 values):",
         ". отпечаток (первые 8 значений):",
         ". Fingerprint (erste 8 Werte):",
         ". empreinte (8 premières valeurs) :",
         ". huella (primeros 8 valores):"]),
    ("similar.method.minhash_blurb",
        ["MinHash catches partial text overlaps: if 60% of document A is \
          contained in document B, Jaccard surfaces it even when a plain \
          SHA-256 wouldn't match. Useful for plagiarism detection, finding \
          drafts, dedup of edited texts.",
         "MinHash находит частичные пересечения текста: если 60% документа A \
          содержится в документе B, Jaccard это покажет, даже если \
          обычный SHA-256 не совпадёт. Полезно для поиска плагиата, \
          черновиков, дедупа редактированных текстов.",
         "MinHash erkennt partielle Textüberlappungen: liegen 60 % von \
          Dokument A in Dokument B, zeigt es der Jaccard-Wert — selbst \
          wenn ein einfacher SHA-256 keine Übereinstimmung findet. Hilft \
          bei Plagiatserkennung, Entwurfssuche, Dedup bearbeiteter Texte.",
         "MinHash détecte les chevauchements partiels : si 60 % du \
          document A se trouve dans le document B, Jaccard le révèle là \
          où un SHA-256 brut échouerait. Utile pour le plagiat, les \
          brouillons, la dédup des textes modifiés.",
         "MinHash detecta solapamientos parciales: si el 60 % del \
          documento A está en el documento B, Jaccard lo encuentra \
          aunque un SHA-256 plano no coincida. Útil para plagio, \
          borradores, dedup de textos editados."]),
    ("similar.method.dhash_name",
        ["per-channel dHash on L0 shards",
         "поканальный dHash на L0-шардах",
         "dHash pro Kanal auf L0-Shards",
         "dHash par canal sur shards L0",
         "dHash por canal en fragmentos L0"]),
    ("similar.method.dhash_fp",
        [". fingerprint (48 bytes, 3 channels × K=16 means):",
         ". отпечаток (48 байт, 3 канала × K=16 средних):",
         ". Fingerprint (48 Byte, 3 Kanäle × K=16 Mittelwerte):",
         ". empreinte (48 octets, 3 canaux × K=16 moyennes) :",
         ". huella (48 bytes, 3 canales × K=16 medias):"]),
    ("similar.method.dhash_blurb",
        ["The fingerprint stacks the mean luminance of K=16 systematic L0 \
          shards for each of the R / G / B channels (DWT LL band for image, \
          bass envelope for audio). Similarity = Hamming distance over 45 \
          dHash bits, re-anchored against the random baseline so unrelated \
          objects clamp to 0 % and identical inputs score 100 %.",
         "Отпечаток складывает среднюю яркость K=16 systematic L0-шардов \
          по каждому каналу R / G / B (DWT LL для изображения, низкочастотная \
          огибающая для аудио). Сходство = Хэммингово расстояние по 45 битам \
          dHash, перенормированное относительно случайной базы — несвязанные \
          объекты сводятся к 0 %, идентичные — к 100 %.",
         "Der Fingerprint stapelt die mittlere Luminanz von K=16 systematischen \
          L0-Shards pro Kanal R / G / B (DWT-LL-Band beim Bild, Bass-Hüllkurve \
          beim Audio). Ähnlichkeit = Hamming-Distanz über 45 dHash-Bits, \
          renormiert gegen die Zufallsbasis: fremde Objekte klemmen auf 0 %, \
          identische auf 100 %.",
         "L'empreinte empile la luminance moyenne de K=16 shards L0 systématiques \
          par canal R / G / B (bande LL DWT pour image, enveloppe basse pour \
          audio). Similarité = distance de Hamming sur 45 bits dHash, recalée \
          sur le bruit aléatoire : les objets non liés tombent à 0 %, \
          identiques à 100 %.",
         "La huella apila la luminancia media de K=16 fragmentos L0 sistemáticos \
          por canal R / G / B (banda LL DWT para imagen, envolvente baja para \
          audio). Similitud = distancia de Hamming en 45 bits dHash, reanclada \
          frente al ruido aleatorio: los objetos sin relación caen a 0 %, \
          los idénticos a 100 %."]),
    ("similar.top_h",
        ["top similar", "топ похожих", "ähnlichste",
         "plus similaires", "más similares"]),
    ("similar.top_empty",
        ["no other objects of the same kind in the catalog",
         "других объектов того же типа в каталоге нет",
         "keine weiteren Objekte gleicher Art im Katalog",
         "aucun autre objet du même type dans le catalogue",
         "no hay otros objetos del mismo tipo en el catálogo"]),
    ("similar.col.name",
        ["name", "имя", "Name", "nom", "nombre"]),
    ("similar.col.similarity",
        ["similarity", "сходство", "Ähnlichkeit", "similarité", "similitud"]),
    ("similar.col.method",
        ["method", "метод", "Methode", "méthode", "método"]),
    ("similar.col.actions",
        ["actions", "действия", "Aktionen", "actions", "acciones"]),
    ("similar.action.open",
        ["open", "открыть", "öffnen", "ouvrir", "abrir"]),
    ("similar.action.shards",
        ["shards", "шарды", "Shards", "fragments", "fragmentos"]),
    ("similar.action.diff",
        ["diff →", "diff →", "Diff →", "diff →", "diff →"]),
    ("similar.overlaps_h",
        ["shard overlaps", "пересечения шардов", "Shard-Überschneidungen",
         "chevauchements de shards", "solapamientos de fragmentos"]),
    ("similar.overlaps_empty",
        ["unique object — no shard hash overlaps with any other",
         "уникальный объект — нет пересечений шардов ни с одним другим",
         "einzigartiges Objekt — keine Shard-Hash-Überschneidung",
         "objet unique — aucun chevauchement de hash de shard",
         "objeto único — sin solapamientos de hash de fragmentos"]),
    ("similar.overlaps_blurb",
        ["dedup at the SHA-256 shard level. An overlap means that part of \
          the data is already physically stored in the cluster (new shards \
          do not duplicate it).",
         "дедупликация на уровне SHA-256 шардов. Пересечение значит, что \
          часть данных уже физически хранится в кластере (новые шарды её \
          не дублируют).",
         "Dedup auf SHA-256-Shard-Ebene. Eine Überschneidung bedeutet, dass \
          ein Teil der Daten bereits physisch im Cluster liegt (neue Shards \
          duplizieren nichts).",
         "dédup au niveau SHA-256 des shards. Un chevauchement signifie \
          qu'une partie des données est déjà stockée physiquement dans le \
          cluster (les nouveaux shards ne la dupliquent pas).",
         "dedup a nivel SHA-256 de fragmentos. Un solapamiento significa que \
          parte de los datos ya está almacenada físicamente en el clúster \
          (los nuevos fragmentos no la duplican)."]),
    ("similar.col.common",
        ["common shards", "общие шарды", "gemeinsame Shards",
         "shards communs", "fragmentos comunes"]),
    ("similar.col.overlap_pct",
        ["% overlap", "% пересечения", "% Überschneidung",
         "% chevauchement", "% solapamiento"]),
    ("similar.footer.all_shards",
        ["← all shards", "← все шарды", "← alle Shards",
         "← tous les shards", "← todos los fragmentos"]),

    // ---- /diff page (Stage 11.19) ---------------------------------------
    ("diff.loading",
        ["loading diff…", "загрузка diff…", "Lade Diff…",
         "chargement du diff…", "cargando diff…"]),
    ("diff.title_prefix",
        ["chunk diff:", "chunk diff:", "Chunk-Diff:",
         "diff de chunks :", "diff de chunks:"]),
    ("diff.intro",
        ["diff = byte-perfect dedup analyzer. Each cell maps to one \
          systematic shard (the first K in each layer). Two cells are green \
          only when the shard hashes match — i.e. the underlying source \
          chunks are byte-for-byte identical in both files. Red = any byte \
          difference.",
         "diff = побайтный анализатор дедупа. Каждая ячейка — один \
          systematic шард (первые K в слое). Зелёные две ячейки только когда \
          совпали хэши шардов — то есть исходные чанки совпадают побайтно в \
          обоих файлах. Красный = любая разница в байтах.",
         "diff = byte-genauer Dedup-Analyser. Jede Zelle = ein systematischer \
          Shard (die ersten K pro Layer). Grün nur, wenn die Shard-Hashes \
          übereinstimmen — die zugrunde liegenden Chunks sind byteweise \
          identisch. Rot = irgendein Byte-Unterschied.",
         "diff = analyseur de dédup au byte près. Chaque case = un shard \
          systématique (les K premiers par couche). Vert seulement quand les \
          hashs correspondent — les chunks source sont identiques au byte \
          près. Rouge = différence de byte.",
         "diff = analizador de dedup byte a byte. Cada celda = un fragmento \
          sistemático (los primeros K por capa). Verde solo cuando los hashes \
          coinciden — los chunks origen son idénticos byte a byte. Rojo = \
          cualquier diferencia."]),
    ("diff.not_visual_warn",
        ["⚠ this is not a visual similarity metric. A Gaussian-blurred \
          copy of the same photo, a desaturated copy, or a re-saved JPEG \
          will all paint mostly red here — different bytes, no dedup \
          savings. For perceptual similarity (which catches blur / \
          desaturation / mild crops) go to /similar/<name>.",
         "⚠ это не метрика визуальной похожести. Размытая копия того же \
          фото, обесцвеченная, или пересохранённый JPEG здесь покрасятся \
          в красный — байты разные, дедупа нет. Для перцептивной похожести \
          (видит размытие / обесцвечивание / небольшие кропы) — /similar/<имя>.",
         "⚠ Dies ist keine visuelle Ähnlichkeitsmetrik. Eine gaußgefilterte \
          Kopie desselben Fotos, eine entsättigte Kopie oder ein neu \
          gespeichertes JPEG erscheinen hier rot — andere Bytes, kein Dedup. \
          Für perzeptuelle Ähnlichkeit (Unschärfe / Entsättigung / leichte \
          Crops) siehe /similar/<name>.",
         "⚠ Ce n'est pas une métrique de similarité visuelle. Une copie \
          floutée de la même photo, désaturée ou re-sauvegardée en JPEG \
          apparaîtra rouge ici — octets différents, aucune dédup. Pour la \
          similarité perceptuelle (flou / désaturation / crops légers) — \
          /similar/<nom>.",
         "⚠ Esta no es una métrica de similitud visual. Una copia con \
          desenfoque gaussiano, una desaturada o un JPEG resavado se \
          pintarán en rojo — bytes distintos, sin dedup. Para similitud \
          perceptual (desenfoque / desaturación / recortes leves) — \
          /similar/<nombre>."]),
    ("diff.useful_blurb",
        ["Useful when: confirming you uploaded the same file twice; spotting \
          shared chunks between near-identical derivatives that survived the \
          codec deterministically; or auditing dedup savings on a workload.",
         "Когда полезно: проверить что один и тот же файл загружен дважды; \
          увидеть общие чанки между почти-идентичными производными, выжившими \
          через детерминированный кодек; аудит экономии на дедупе.",
         "Nützlich um: zu prüfen, ob dieselbe Datei zweimal hochgeladen \
          wurde; gemeinsame Chunks zwischen fast identischen Derivaten zu \
          finden; Dedup-Einsparungen zu auditieren.",
         "Utile pour : confirmer qu'un fichier a été uploadé deux fois ; \
          repérer les chunks communs entre dérivés quasi identiques ; \
          auditer les économies de dédup.",
         "Útil para: confirmar que un archivo se subió dos veces; localizar \
          chunks comunes entre derivados casi idénticos; auditar el ahorro \
          de dedup."]),
    ("diff.text_limit_warn",
        ["⚠ text limitation: fixed-size chunking is used (equal-byte \
          slices). A small change near the start shifts all boundaries — \
          every subsequent chunk differs. A git-style diff would need \
          content-defined chunking (rolling hash / FastCDC) — that's a \
          separate task. Right now text diff works well only for identical \
          copies (100%) and totally different files (0%).",
         "⚠ ограничение для текста: используется фиксированный chunking \
          (равные куски). Маленькое изменение в начале сдвигает все границы — \
          все последующие чанки отличаются. Для git-стиля нужен \
          content-defined chunking (rolling hash / FastCDC) — это отдельная \
          задача. Сейчас text diff работает хорошо только для идентичных \
          копий (100 %) и полностью разных файлов (0 %).",
         "⚠ Texteinschränkung: festes Chunking (gleiche Bytegrößen). Eine \
          kleine Änderung am Anfang verschiebt alle Grenzen — jeder folgende \
          Chunk weicht ab. Ein Git-Diff bräuchte content-defined chunking \
          (Rolling Hash / FastCDC) — separate Aufgabe. Aktuell taugt der \
          Text-Diff nur für identische Kopien (100 %) und völlig \
          unterschiedliche Dateien (0 %).",
         "⚠ limite texte : chunking à taille fixe (tranches d'octets \
          égales). Un petit changement au début décale toutes les limites \
          — chaque chunk suivant diffère. Un diff git-like demanderait du \
          chunking par contenu (rolling hash / FastCDC) — tâche séparée. \
          Pour l'instant le diff texte ne marche bien que pour copies \
          identiques (100 %) et fichiers totalement différents (0 %).",
         "⚠ limitación de texto: chunking de tamaño fijo (cortes iguales). \
          Un cambio pequeño al inicio desplaza todos los límites — cada \
          chunk siguiente difiere. Un diff estilo git necesitaría chunking \
          por contenido (rolling hash / FastCDC) — tarea aparte. De momento \
          el diff de texto solo funciona bien con copias idénticas (100 %) \
          y archivos totalmente distintos (0 %)."]),
    ("diff.stat.identical",
        ["identical chunks", "идентичные чанки", "identische Chunks",
         "chunks identiques", "chunks idénticos"]),
    ("diff.stat.byte_overlap",
        ["byte-dedup overlap", "пересечение байтового дедупа",
         "Byte-Dedup-Überlapp", "chevauchement dédup par byte",
         "solapamiento de dedup por byte"]),
    ("diff.stat.storage_saved",
        ["storage saved", "сэкономлено", "Speicher gespart",
         "stockage économisé", "almacenamiento ahorrado"]),
    ("diff.kind.text_chunks",
        ["text chunks", "текстовые чанки", "Text-Chunks",
         "chunks texte", "chunks de texto"]),
    ("diff.kind.systematic_shards",
        ["systematic shards", "systematic шарды", "systematische Shards",
         "shards systématiques", "fragmentos sistemáticos"]),
    ("diff.storage_explainer_prefix",
        ["storage saving: those", "экономия: эти",
         "Speicherersparnis: diese", "économie : ces",
         "ahorro: estos"]),
    ("diff.storage_explainer_suffix",
        ["common chunks are not physically duplicated in the cluster — \
          nodes keep one copy per hash.",
         "общих чанков не дублируются физически в кластере — ноды держат \
          одну копию на хэш.",
         "gemeinsamen Chunks werden im Cluster nicht physisch dupliziert — \
          jede Node hält eine Kopie pro Hash.",
         "chunks communs ne sont pas dupliqués physiquement dans le cluster \
          — chaque nœud garde une copie par hash.",
         "chunks comunes no se duplican físicamente en el clúster — los \
          nodos guardan una copia por hash."]),
    ("diff.footer.similar_a",
        ["← similar to a", "← похожие на a", "← ähnlich zu a",
         "← similaires à a", "← similares a a"]),

    // ---- /health page (Stage 11.19) -------------------------------------
    ("health.loading",
        ["loading health…", "загрузка состояния…", "Lade Status…",
         "chargement de la santé…", "cargando estado…"]),
    ("health.loading_object",
        ["loading object health…", "загрузка состояния объекта…",
         "Lade Objektstatus…", "chargement de la santé de l'objet…",
         "cargando estado del objeto…"]),
    ("health.cluster_nodes_h",
        ["cluster nodes", "ноды кластера", "Cluster-Nodes",
         "nœuds du cluster", "nodos del clúster"]),
    ("health.objects_h",
        ["objects", "объекты", "Objekte", "objets", "objetos"]),
    ("health.objects_empty",
        ["no objects yet — upload via the catalog page",
         "пока нет объектов — загрузите через страницу каталога",
         "noch keine Objekte — laden Sie über die Katalogseite hoch",
         "aucun objet pour l'instant — chargez via la page catalogue",
         "aún no hay objetos — sube desde la página del catálogo"]),
    ("health.see_catalog",
        ["see catalog", "перейти в каталог", "zum Katalog",
         "voir le catalogue", "ver el catálogo"]),
    ("health.col.address",
        ["address", "адрес", "Adresse", "adresse", "dirección"]),
    ("health.col.zone",
        ["zone", "зона", "Zone", "zone", "zona"]),
    ("health.col.status",
        ["status", "статус", "Status", "statut", "estado"]),
    ("health.col.action",
        ["action", "действие", "Aktion", "action", "acción"]),
    ("health.col.layer",
        ["layer", "слой", "Layer", "couche", "capa"]),
    ("health.col.channel_short",
        ["ch", "кан", "Kn", "ch", "can"]),
    ("health.col.kill_pct",
        ["kill %", "kill %", "Kill %", "kill %", "kill %"]),
    ("health.col.trials",
        ["trials", "испытания", "Versuche", "essais", "ensayos"]),
    ("health.col.nodes_in_zone",
        ["nodes in zone", "нод в зоне", "Nodes in Zone",
         "nœuds dans la zone", "nodos en la zona"]),
    ("health.col.remaining_res",
        ["remaining resolution", "оставшееся разрешение",
         "verbleibende Auflösung", "résolution restante",
         "resolución restante"]),
    ("health.status.disabled",
        ["disabled", "выключено", "deaktiviert", "désactivé", "desactivado"]),
    ("health.status.live",
        ["live", "активно", "aktiv", "actif", "activo"]),
    ("health.action.revive",
        ["revive", "вернуть", "reaktivieren", "réactiver", "reactivar"]),
    ("health.action.kill",
        ["kill", "выключить", "deaktivieren", "désactiver", "matar"]),
    ("health.stat.live_total",
        ["live / total nodes", "активно / всего нод",
         "aktive / gesamt Nodes", "actifs / total nœuds",
         "activos / total nodos"]),
    ("health.stat.admin_disabled",
        ["admin-disabled", "выключено админом", "Admin-deaktiviert",
         "admin-désactivé", "admin-desactivado"]),
    ("health.stat.live_sse",
        ["live · SSE", "live · SSE", "live · SSE", "live · SSE", "live · SSE"]),
    ("health.stat.k_threshold",
        ["k (threshold)", "k (порог)", "k (Schwelle)",
         "k (seuil)", "k (umbral)"]),
    ("health.stat.channels_layers",
        ["channels × layers", "каналы × слои", "Kanäle × Layer",
         "canaux × couches", "canales × capas"]),
    ("health.stat.current_res",
        ["current resolution", "текущее разрешение", "aktuelle Auflösung",
         "résolution actuelle", "resolución actual"]),
    ("health.matrix_h",
        ["redundancy margin by (channel, layer)",
         "запас избыточности по (каналу, слою)",
         "Redundanzmarge nach (Kanal, Layer)",
         "marge de redondance par (canal, couche)",
         "margen de redundancia por (canal, capa)"]),
    ("health.loss_h",
        ["random loss simulation (Monte-Carlo)",
         "симуляция случайных потерь (Монте-Карло)",
         "Zufallsverlust-Simulation (Monte-Carlo)",
         "simulation de pertes aléatoires (Monte-Carlo)",
         "simulación de pérdidas aleatorias (Monte-Carlo)"]),
    ("health.zone_h",
        ["whole-zone failure (anti-affinity)",
         "отказ целой зоны (anti-affinity)",
         "Ausfall ganzer Zone (Anti-Affinity)",
         "panne d'une zone entière (anti-affinity)",
         "fallo de zona completa (anti-affinity)"]),

    // ---- /inspect page (Stage 11.19) ------------------------------------
    ("inspect.loading",
        ["loading shards…", "загрузка шардов…", "Lade Shards…",
         "chargement des shards…", "cargando fragmentos…"]),
    ("inspect.loading_shard",
        ["loading shard…", "загрузка шарда…", "Lade Shard…",
         "chargement du shard…", "cargando fragmento…"]),
    ("inspect.title_suffix",
        ["— what is physically stored on nodes",
         "— что физически хранится на нодах",
         "— was physisch auf den Nodes liegt",
         "— ce qui est physiquement stocké sur les nœuds",
         "— qué se almacena físicamente en los nodos"]),
    ("inspect.intro",
        ["Each cell below = one shard on one cluster node. Systematic \
          shards (first K in each layer) carry a raw data chunk in their \
          payload — it shows in the structure. RLNC shards are random \
          linear combinations over GF(256), visually uniform noise. \
          Knowing only one shard, you cannot recover anything — you need \
          ≥K independent shards of one (channel, layer).",
         "Каждая ячейка ниже = один шард на одной ноде кластера. \
          Systematic шарды (первые K в каждом слое) несут сырой чанк \
          данных в payload — это видно по структуре. RLNC шарды — \
          случайные линейные комбинации над GF(256), визуально \
          однородный шум. Зная только один шард, восстановить ничего \
          нельзя — нужно ≥K независимых шардов одного (канала, слоя).",
         "Jede Zelle = ein Shard auf einem Cluster-Node. Systematische \
          Shards (erste K pro Layer) tragen einen rohen Daten-Chunk in der \
          Nutzlast — sichtbar in der Struktur. RLNC-Shards sind zufällige \
          Linearkombinationen über GF(256), visuell gleichförmiges Rauschen. \
          Mit einem einzigen Shard lässt sich nichts wiederherstellen — \
          es braucht ≥K unabhängige Shards eines (Kanal, Layer)-Paars.",
         "Chaque case = un shard sur un nœud du cluster. Les shards \
          systématiques (les K premiers par couche) portent un chunk \
          brut — c'est visible dans la structure. Les shards RLNC sont \
          des combinaisons linéaires aléatoires sur GF(256), bruit \
          uniforme. Avec un seul shard, on ne récupère rien — il en \
          faut ≥K indépendants d'un même (canal, couche).",
         "Cada celda = un fragmento en un nodo del clúster. Los fragmentos \
          sistemáticos (los primeros K por capa) llevan un chunk crudo \
          — se ve en la estructura. Los fragmentos RLNC son combinaciones \
          lineales aleatorias sobre GF(256), ruido uniforme. Con un solo \
          fragmento no se recupera nada — hacen falta ≥K independientes \
          de un mismo par (canal, capa)."]),
    ("inspect.layer_prefix",
        ["layer", "слой", "Layer", "couche", "capa"]),
    ("inspect.shards_word",
        ["shards", "шардов", "Shards", "shards", "fragmentos"]),
    ("inspect.systematic_word",
        ["systematic", "systematic", "systematisch", "systématique", "sistemático"]),
    ("inspect.rlnc_word",
        ["RLNC", "RLNC", "RLNC", "RLNC", "RLNC"]),
    ("inspect.shard_of",
        ["shard of object", "шард объекта", "Shard des Objekts",
         "shard de l'objet", "fragmento del objeto"]),
    ("inspect.zoom.shard_idx",
        ["shard_idx", "shard_idx", "shard_idx", "shard_idx", "shard_idx"]),
    ("inspect.zoom.channel",
        ["channel", "канал", "Kanal", "canal", "canal"]),
    ("inspect.zoom.layer",
        ["layer", "слой", "Layer", "couche", "capa"]),
    ("inspect.zoom.physical_node",
        ["physical node", "физическая нода", "physische Node",
         "nœud physique", "nodo físico"]),
    ("inspect.zoom.sym_len",
        ["sym_len", "sym_len", "sym_len", "sym_len", "sym_len"]),
    ("inspect.zoom.shard_hash",
        ["shard_hash", "shard_hash", "shard_hash", "shard_hash", "shard_hash"]),
    ("inspect.zoom.bytes",
        ["bytes", "байт", "Bytes", "octets", "bytes"]),
    ("inspect.zoom.coeffs",
        ["coeffs", "коэффициенты", "Koeffizienten", "coeffs", "coefs"]),
    ("inspect.zoom.coeffs_k_bytes",
        ["(K bytes):", "(K байт):", "(K Bytes):", "(K octets) :", "(K bytes):"]),
    ("inspect.zoom.payload_prefix",
        ["payload", "payload", "Payload", "payload", "payload"]),
    ("inspect.zoom.payload_first_64",
        ["(first 64 bytes of", "(первые 64 байта из",
         "(erste 64 Bytes von", "(64 premiers octets sur",
         "(primeros 64 bytes de"]),
    ("inspect.zoom.as_hex",
        ["as hex):", "в hex):", "als Hex):", "en hex) :", "en hex):"]),
    ("inspect.zoom.shard_unavailable",
        ["shard not retrieved (node unavailable or removed)",
         "шард не получен (нода недоступна или удалена)",
         "Shard nicht abrufbar (Node nicht erreichbar oder entfernt)",
         "shard non récupéré (nœud indisponible ou supprimé)",
         "fragmento no recibido (nodo no disponible o eliminado)"]),
    ("inspect.zoom.back_link",
        ["← all shards of the object",
         "← все шарды объекта",
         "← alle Shards des Objekts",
         "← tous les shards de l'objet",
         "← todos los fragmentos del objeto"]),
    ("inspect.label.image_r",
        ["channel 0 · R (red)", "канал 0 · R (красный)",
         "Kanal 0 · R (Rot)", "canal 0 · R (rouge)",
         "canal 0 · R (rojo)"]),
    ("inspect.label.image_g",
        ["channel 1 · G (green)", "канал 1 · G (зелёный)",
         "Kanal 1 · G (Grün)", "canal 1 · G (vert)",
         "canal 1 · G (verde)"]),
    ("inspect.label.image_b",
        ["channel 2 · B (blue)", "канал 2 · B (синий)",
         "Kanal 2 · B (Blau)", "canal 2 · B (bleu)",
         "canal 2 · B (azul)"]),
    ("inspect.label.audio_l",
        ["channel 0 · L (left)", "канал 0 · L (левый)",
         "Kanal 0 · L (links)", "canal 0 · L (gauche)",
         "canal 0 · L (izquierdo)"]),
    ("inspect.label.audio_r",
        ["channel 1 · R (right)", "канал 1 · R (правый)",
         "Kanal 1 · R (rechts)", "canal 1 · R (droite)",
         "canal 1 · R (derecho)"]),
    ("inspect.label.image_coarse",
        ["L0 · LL · coarse structure",
         "L0 · LL · грубая структура",
         "L0 · LL · grobe Struktur",
         "L0 · LL · structure grossière",
         "L0 · LL · estructura gruesa"]),
    ("inspect.label.image_fine",
        ["HH · finest details", "HH · самые мелкие детали",
         "HH · feinste Details", "HH · détails les plus fins",
         "HH · detalles más finos"]),
    ("inspect.label.audio_bass",
        ["L0 · bass and envelope", "L0 · бас и огибающая",
         "L0 · Bass und Hüllkurve", "L0 · basse et enveloppe",
         "L0 · bajos y envolvente"]),
    ("inspect.label.audio_mid",
        ["mid frequencies", "средние частоты", "Mittenfrequenzen",
         "fréquences moyennes", "frecuencias medias"]),
    ("inspect.label.audio_high",
        ["high frequencies", "высокие частоты", "Hochfrequenzen",
         "fréquences hautes", "frecuencias altas"]),

    // ---- Stage 12.7: per-file unique metrics on /health/<name> ---------
    ("health.metrics_h",
        ["Unique metrics",
         "Уникальные метрики",
         "Einzigartige Kennzahlen",
         "Métriques uniques",
         "Métricas únicas"]),
    ("health.metrics_intro",
        ["These signals fall out of the holofs storage geometry — per-layer shards + content-addressed hashes — and would not exist in a flat object store.",
         "Эти показатели — следствие архитектуры holofs (шарды по слоям + контент-адресация). В обычном объектном хранилище их посчитать нельзя.",
         "Diese Signale ergeben sich aus der holofs-Speichergeometrie (Shards pro Schicht + Content-Adressierung) und existieren in einem flachen Objektspeicher nicht.",
         "Ces indicateurs découlent de la géométrie de stockage holofs (shards par couche + adressage par contenu) et n'existent pas dans un object store classique.",
         "Estos indicadores surgen de la geometría de almacenamiento de holofs (shards por capa + direccionamiento por contenido) y no existen en un object store plano."]),
    ("health.metrics_loading",
        ["computing unique metrics… (one network roundtrip per layer)",
         "считаем уникальные метрики… (по одному сетевому запросу на слой)",
         "Berechne einzigartige Kennzahlen… (eine Netzwerk-Roundtrip pro Schicht)",
         "calcul des métriques uniques… (un aller-retour réseau par couche)",
         "calculando métricas únicas… (un viaje de red por capa)"]),
    ("health.metrics.shards_in_file",
        ["unique / total shards (file)",
         "уникальных / всего шардов (файл)",
         "eindeutige / gesamte Shards (Datei)",
         "shards uniques / total (fichier)",
         "shards únicos / totales (archivo)"]),
    ("health.metrics.file_dedup",
        ["intra-file dedup",
         "дедуп внутри файла",
         "Intra-Datei-Dedup",
         "déduplication interne",
         "deduplicación interna"]),
    ("health.metrics.originality",
        ["originality",
         "оригинальность",
         "Einzigartigkeit",
         "originalité",
         "originalidad"]),
    ("health.metrics.unique_to_file",
        ["shards seen only here",
         "шардов только в этом файле",
         "Shards nur in dieser Datei",
         "shards uniques à ce fichier",
         "shards únicos de este archivo"]),
    ("health.metrics.catalog_dedup_raw",
        ["unique / total (catalog)",
         "уникальных / всего (каталог)",
         "eindeutig / gesamt (Katalog)",
         "uniques / total (catalogue)",
         "únicos / totales (catálogo)"]),
    ("health.metrics.catalog_savings",
        ["catalog-wide dedup savings",
         "экономия от дедупа в каталоге",
         "Katalogweite Dedup-Ersparnis",
         "économie de dédup du catalogue",
         "ahorro de dedup en el catálogo"]),
    ("health.metrics.layer_originality_h",
        ["Originality per layer",
         "Оригинальность по слоям",
         "Einzigartigkeit pro Schicht",
         "Originalité par couche",
         "Originalidad por capa"]),
    ("health.metrics.layer_originality_help",
        ["For each DWT layer: % of its shard hashes that appear in no other catalog entry. High at L0 = unique composition; high at the top = unique detail / texture.",
         "Для каждого слоя DWT: доля хешей, не встречающихся в других файлах. Высоко на L0 = уникальная структура; высоко на верхних = уникальная фактура.",
         "Pro DWT-Schicht: Anteil der Shard-Hashes, die in keinem anderen Katalogeintrag vorkommen. Hoch bei L0 = einzigartige Komposition; hoch oben = einzigartige Details.",
         "Par couche DWT : % de hachages de shards absents des autres fichiers. Élevé en L0 = composition unique ; en haut = détail unique.",
         "Por capa DWT: % de hashes de shards que no aparecen en otros archivos. Alto en L0 = composición única; alto arriba = detalle único."]),
    ("health.metrics.layer_energy_h",
        ["Layer energy distribution",
         "Распределение энергии по слоям",
         "Energieverteilung der Schichten",
         "Distribution d'énergie par couche",
         "Distribución de energía por capa"]),
    ("health.metrics.layer_energy_help",
        ["Share of the squared DWT coefficient energy living in each layer. Coarse-heavy = smooth / structural content; high-layer-heavy = busy / detail-rich content. Computed by actually decoding every layer once.",
         "Доля энергии (сумма квадратов коэффициентов DWT) на каждый слой. Перекос к низким = плавный, «структурный» контент; к верхним = детально-нагруженный. Считается полной разверткой всех слоёв.",
         "Anteil der quadrierten DWT-Koeffizientenenergie pro Schicht. Schwerpunkt unten = glatte / strukturelle Inhalte; oben = detailreiche Inhalte. Per Decodierung aller Schichten berechnet.",
         "Part de l'énergie au carré des coefficients DWT par couche. Concentré en bas = contenu structurel doux ; en haut = contenu détaillé. Calculé via décodage de toutes les couches.",
         "Proporción de la energía cuadrada de coeficientes DWT por capa. Concentrado abajo = contenido suave/estructural; arriba = contenido detallado. Calculado decodificando todas las capas."]),
    ("health.metrics.audio_bands_h",
        ["Audio band split (bass / mid / treble)",
         "Распределение по полосам (бас / средние / высокие)",
         "Audio-Bandverteilung (Bass / Mitte / Höhen)",
         "Répartition par bande audio (basses / médiums / aigus)",
         "Distribución por bandas (graves / medios / agudos)"]),
    ("health.metrics.audio_bands_help",
        ["Layer energies grouped into three equal-thirds bands. Bass = the bottom third of layers (envelope, low frequencies); treble = the top third (transients, sibilance). Useful for auto-tagging speech vs music vs sound effects.",
         "Энергии слоёв сгруппированы в три полосы по третям. Бас — нижняя треть (огибающая, низкие частоты); высокие — верхняя треть (атаки, шипящие). Помогает авто-тегировать речь / музыку / эффекты.",
         "Schichtenergien zu drei Gleichdritteln-Bändern gruppiert. Bass = unteres Drittel (Hüllkurve, tiefe Frequenzen); Höhen = oberes Drittel (Transienten, Zischlaute). Nützlich zur Auto-Klassifikation Sprache/Musik/SFX.",
         "Énergies de couches regroupées en trois bandes égales. Basses = tiers inférieur (enveloppe, basses fréquences) ; aigus = tiers supérieur (transitoires, sifflantes). Utile pour le tagging auto parole/musique/SFX.",
         "Energías de capas agrupadas en tres bandas iguales. Graves = tercio inferior (envolvente, frecuencias bajas); agudos = tercio superior (transitorios, sibilantes). Útil para etiquetado automático voz/música/SFX."]),
    ("health.metrics.neighbours_h",
        ["Cross-file shard reuse",
         "Переиспользование шардов с другими файлами",
         "Schard-Wiederverwendung über Dateien",
         "Réutilisation de shards entre fichiers",
         "Reutilización de shards entre archivos"]),
    ("health.metrics.neighbours_help",
        ["Other catalog entries that share shard hashes with this file. Per-layer breakdown shows where the overlap lives — same coarse layers ⇒ same composition; same fine layers ⇒ same texture / derived asset.",
         "Другие файлы каталога, у которых есть общие шард-хеши с этим. Разбивка по слоям показывает природу совпадения — общие низкие слои = та же композиция; общие высокие = та же фактура / производный ассет.",
         "Andere Katalogeinträge, die Shard-Hashes mit dieser Datei teilen. Pro-Schicht-Verteilung zeigt die Art der Überlappung — gleiche grobe Schichten = gleiche Komposition; gleiche feine Schichten = gleiche Textur / abgeleitetes Asset.",
         "Autres fichiers du catalogue qui partagent des hachages de shards. La répartition par couche montre la nature du chevauchement — couches grossières communes = même composition ; fines communes = même texture / dérivé.",
         "Otros archivos que comparten hashes de shards. El desglose por capa muestra la naturaleza del solape — capas gruesas comunes = misma composición; finas comunes = misma textura / activo derivado."]),
    ("health.metrics.col.name",
        ["file", "файл", "Datei", "fichier", "archivo"]),
    ("health.metrics.col.kind",
        ["kind", "тип", "Art", "type", "tipo"]),
    ("health.metrics.col.shared",
        ["shared shards", "общих шардов", "geteilte Shards",
         "shards partagés", "shards compartidos"]),
    ("health.metrics.col.overlap_pct",
        ["% of this file", "% этого файла", "% dieser Datei",
         "% de ce fichier", "% de este archivo"]),
    ("health.metrics.col.layer_breakdown",
        ["per-layer breakdown",
         "разбивка по слоям",
         "Aufschlüsselung pro Schicht",
         "détail par couche",
         "desglose por capa"]),
    ("health.metrics.empty",
        ["No cross-file reuse and no decodable layers — this file is fully isolated in the catalog.",
         "Нет переиспользования и декодируемых слоёв — файл полностью изолирован в каталоге.",
         "Keine Wiederverwendung und keine dekodierbaren Schichten — diese Datei ist im Katalog vollständig isoliert.",
         "Aucune réutilisation et aucune couche décodable — ce fichier est totalement isolé dans le catalogue.",
         "Sin reutilización ni capas decodificables — este archivo está aislado en el catálogo."]),

    // ---- Stage 12.7: /about marketing page -----------------------------
    ("nav.about",
        ["why holofs", "зачем holofs", "warum holofs",
         "pourquoi holofs", "por qué holofs"]),
    ("about.hero.tag",
        ["object store", "объектное хранилище", "Objektspeicher",
         "object store", "object store"]),
    ("about.hero.h",
        ["A storage layer that knows what it stores.",
         "Хранилище, которое понимает, что в нём лежит.",
         "Ein Speicher, der weiß, was er speichert.",
         "Un stockage qui comprend ce qu'il garde.",
         "Un almacenamiento que entiende lo que guarda."]),
    ("about.hero.sub",
        ["Holofs encodes every image, audio and document into per-layer, content-addressed shards. The same geometry that makes it durable also exposes business signals — originality, brand consistency, dedup, frequency profile — without re-decoding anything.",
         "Holofs кладёт каждое изображение, звук и документ как набор шардов по слоям с адресацией по содержимому. Та же геометрия, что делает данные надёжными, бесплатно даёт бизнес-метрики — оригинальность, бренд-согласованность, дедупликацию, частотный профиль — без перекодирования.",
         "Holofs zerlegt jedes Bild, jede Audiodatei und jedes Dokument in schichtweise, inhaltsadressierte Shards. Dieselbe Geometrie, die die Daten haltbar macht, liefert kostenlos Geschäftskennzahlen — Originalität, Markenkonsistenz, Deduplizierung, Frequenzprofil — ohne erneutes Dekodieren.",
         "Holofs encode chaque image, audio et document en shards par couche, adressés par contenu. La même géométrie qui assure la durabilité offre gratuitement des indicateurs métier — originalité, cohérence de marque, déduplication, profil fréquentiel — sans décoder à nouveau.",
         "Holofs codifica cada imagen, audio y documento en shards por capa direccionados por contenido. La misma geometría que da durabilidad ofrece gratis métricas de negocio — originalidad, consistencia de marca, deduplicación, perfil frecuencial — sin volver a decodificar."]),
    ("about.unique.h",
        ["What only holofs gives you",
         "Что есть только у holofs",
         "Was nur holofs bietet",
         "Ce que seul holofs offre",
         "Qué solo da holofs"]),
    ("about.unique.layer.h",
        ["Per-layer addressable storage",
         "Адресуемое по слоям хранилище",
         "Schichtweise adressierbarer Speicher",
         "Stockage adressable par couche",
         "Almacenamiento direccionable por capa"]),
    ("about.unique.layer.body",
        ["Each file is split into DWT layers (image/audio) or RLNC chunks (binary). You can fetch only the coarse layers for an instant low-quality preview, the high layers for detail diffing, or the whole stack for byte-perfect recovery.",
         "Каждый файл разложен на слои DWT (картинка/звук) или RLNC-куски (бинарь). Можно тянуть только нижние слои — мгновенное превью, верхние — для diff деталей, всё вместе — побитовое восстановление.",
         "Jede Datei wird in DWT-Schichten (Bild/Audio) bzw. RLNC-Chunks (Binär) zerlegt. Sie können nur die groben Schichten für sofortige Vorschau, nur die hohen für Detail-Vergleich oder den ganzen Stapel für bitgenaue Wiederherstellung abrufen.",
         "Chaque fichier est découpé en couches DWT (image/audio) ou en blocs RLNC (binaires). Vous pouvez ne récupérer que les couches grossières pour un aperçu instantané, les couches hautes pour comparer les détails, ou la pile complète pour une restitution octet pour octet.",
         "Cada archivo se divide en capas DWT (imagen/audio) o bloques RLNC (binarios). Puede recuperar solo las capas gruesas para vista previa instantánea, las altas para comparar detalles o la pila completa para recuperación bit a bit."]),
    ("about.unique.dedup.h",
        ["Content-addressed dedup, automatic",
         "Дедуп по содержимому, автоматически",
         "Inhaltsadressierte Deduplizierung, automatisch",
         "Déduplication par contenu, automatique",
         "Deduplicación por contenido, automática"]),
    ("about.unique.dedup.body",
        ["Two files that share a coefficient block share a shard. Storage savings, leak detection, and provenance scoring fall out of the same primitive.",
         "Если у двух файлов совпадает блок коэффициентов — шард общий. Из одной примитивной операции получаем экономию места, детект утечек и оценку оригинальности.",
         "Teilen sich zwei Dateien einen Koeffizientenblock, teilen sie einen Shard. Speicherersparnis, Leck-Erkennung und Provenienz-Bewertung folgen aus derselben Primitive.",
         "Deux fichiers qui partagent un bloc de coefficients partagent un shard. Économies de stockage, détection de fuite et scoring de provenance découlent du même primitif.",
         "Si dos archivos comparten un bloque de coeficientes comparten un shard. Ahorro de almacenamiento, detección de fugas y puntuación de procedencia salen del mismo primitivo."]),
    ("about.unique.rlnc.h",
        ["RLNC erasure coding (k-of-n)",
         "RLNC-кодирование (k-из-n)",
         "RLNC-Erasure-Coding (k-aus-n)",
         "Codage RLNC (k parmi n)",
         "Codificación RLNC (k de n)"]),
    ("about.unique.rlnc.body",
        ["Survive the loss of an entire rack and still decode bit-perfect from any k surviving shards. Repair runs without re-reading the source.",
         "Любая стойка может умереть целиком — пока живы любые k шардов, файл восстанавливается побитово. Репликации без чтения исходника.",
         "Auch der Ausfall eines kompletten Racks ist tolerierbar: solange k Shards leben, wird bitgenau dekodiert. Repair ohne erneutes Lesen der Quelle.",
         "Toute la baie peut tomber : tant que k shards survivent, le fichier se reconstitue octet pour octet. Les réparations n'exigent pas la relecture de la source.",
         "Un rack entero puede caer: mientras vivan k shards, el archivo se reconstruye bit a bit. Las reparaciones no necesitan releer el origen."]),
    ("about.unique.transforms.h",
        ["Operate on shards, not pixels",
         "Операции на шардах, не на пикселях",
         "Operationen auf Shards statt Pixeln",
         "Opérations sur shards, pas pixels",
         "Operaciones en shards, no en píxeles"]),
    ("about.unique.transforms.body",
        ["Wavelet mixing (combine the structure of A with the detail of B), audio band filtering, layer-cut previews — all run on shard buckets directly. No re-encoding, no temporary files.",
         "Wavelet-микширование (структура от A, детали от B), частотные фильтры аудио, превью с обрезом по слоям — всё это работает с бакетами шардов напрямую. Без перекодирования, без временных файлов.",
         "Wavelet-Mixing (Struktur aus A, Detail aus B), Audio-Bandfilter, Layer-Cut-Vorschauen — alle Operationen laufen direkt auf Shard-Buckets. Kein Re-Encoding, keine temporären Dateien.",
         "Mixage par ondelettes (structure de A, détails de B), filtres de bande audio, aperçus tronqués par couche — tout opère directement sur les buckets de shards. Sans ré-encodage, sans fichier temporaire.",
         "Mezcla por wavelets (estructura de A, detalle de B), filtros de banda de audio, vistas previas truncadas por capa — todo opera directamente sobre los buckets de shards. Sin recodificar, sin archivos temporales."]),
    ("about.value.h",
        ["What that buys the business",
         "Что это даёт бизнесу",
         "Was das dem Geschäft bringt",
         "Ce que ça apporte au métier",
         "Qué aporta al negocio"]),
    ("about.value.list",
        ["Lower cost-per-asset via cross-asset dedup. | Compliance-grade originality scoring on every PUT. | Tiered CDN delivery (LQIP from coarse layers, full from all). | Instant similarity / leak detection without ML. | Survive a full zone outage with zero data loss. | Storage you can audit — every byte has a content-hash trail.",
         "Меньше стоимости на ассет за счёт дедупа между файлами. | Проверка оригинальности уровня compliance на каждый PUT. | Многоуровневая CDN-доставка (LQIP с нижних слоёв, полное — со всех). | Мгновенный поиск похожих / детект утечек без ML. | Полный отказ зоны не теряет данных. | Хранилище под аудит — у каждого байта своя хеш-цепочка.",
         "Geringere Kosten pro Asset durch dateiübergreifende Deduplizierung. | Compliance-tauglicher Originalitätsscore bei jedem PUT. | CDN-Auslieferung in Stufen (LQIP aus groben Schichten, voll aus allen). | Sofortige Ähnlichkeits- / Leck-Erkennung ohne ML. | Vollständiger Zonenausfall ohne Datenverlust. | Auditierbarer Speicher — jeder Byte hat eine Hash-Spur.",
         "Coût par actif réduit grâce à la déduplication inter-fichiers. | Score d'originalité de niveau conformité à chaque PUT. | Livraison CDN par paliers (LQIP via couches grossières, complet via toutes). | Détection instantanée de similitudes / fuites sans ML. | Panne complète de zone sans perte de données. | Stockage auditable — chaque octet a une piste de hachage.",
         "Menor coste por activo gracias a la deduplicación entre archivos. | Puntuación de originalidad de nivel cumplimiento en cada PUT. | Entrega CDN por niveles (LQIP desde capas gruesas, completo desde todas). | Detección instantánea de similitudes / fugas sin ML. | Caída total de una zona sin pérdida de datos. | Almacenamiento auditable — cada byte tiene un rastro de hash."]),
    ("about.cases.h",
        ["Real use cases",
         "Реальные кейсы",
         "Reale Anwendungsfälle",
         "Cas d'usage réels",
         "Casos de uso reales"]),
    ("about.cases.brand.h",
        ["Brand-asset compliance",
         "Контроль брендовых ассетов",
         "Markenkonformität",
         "Conformité d'actifs de marque",
         "Cumplimiento de activos de marca"]),
    ("about.cases.brand.body",
        ["For an agency archive: surface every derivative of a master logo (same coarse layers, divergent details) and every leak (any shard reuse outside an approved folder) — no perceptual hashing, no manual tagging.",
         "Архив агентства: подсветить все производные мастер-логотипа (общие низкие слои, разные детали) и любые утечки (шарды появляются вне разрешённой папки) — без перцептивных хешей и ручных тегов.",
         "Agentur-Archiv: jede Ableitung eines Master-Logos (gleiche grobe Schichten, abweichende Details) und jeden Leak (Shards außerhalb der Freigabe-Ordner) aufzeigen — ohne Perceptual Hashing, ohne manuelles Tagging.",
         "Archives d'agence : faire émerger chaque dérivé d'un logo maître (couches grossières communes, détails divergents) et chaque fuite (shards hors dossier autorisé) — sans hachage perceptuel, sans tag manuel.",
         "Archivo de agencia: sacar a la luz cada derivado de un logo maestro (capas gruesas comunes, detalles divergentes) y cada filtración (shards fuera de la carpeta aprobada) — sin hashing perceptual, sin etiquetado manual."]),
    ("about.cases.dedup.h",
        ["Hosting bill — minus 40-70%",
         "Счёт за хостинг — минус 40-70%",
         "Hosting-Rechnung — minus 40-70 %",
         "Facture d'hébergement — moins 40-70 %",
         "Factura de hosting — menos 40-70 %"]),
    ("about.cases.dedup.body",
        ["Media libraries with template-driven content (e-commerce variants, social-media kits, slide-deck exports) collapse hard under shard-level dedup. The catalog's dedup % is visible on this very page.",
         "Медиа-библиотеки на шаблонах (варианты e-commerce, soc-media наборы, экспорты слайдов) проседают сильно под дедупом на уровне шардов. Процент дедупа по каталогу видно на этой же странице.",
         "Medienbibliotheken mit Vorlagen-Inhalten (E-Commerce-Varianten, Social-Media-Kits, Slide-Exports) brechen unter Shard-Dedup massiv ein. Der Katalog-Dedup-Anteil ist genau hier sichtbar.",
         "Les bibliothèques média à contenu basé sur modèles (variantes e-commerce, kits réseaux sociaux, exports de slides) chutent fortement sous la déduplication au niveau shard. Le % de dédup du catalogue est visible sur cette même page.",
         "Las bibliotecas multimedia basadas en plantillas (variantes e-commerce, kits redes sociales, exportaciones de slides) caen fuerte con dedup a nivel de shard. El % de dedup del catálogo es visible en esta misma página."]),
    ("about.cases.cdn.h",
        ["Tiered CDN: 8% trafic, 92% perception",
         "Многоуровневый CDN: 8% трафика, 92% восприятия",
         "Mehrstufiges CDN: 8 % Traffic, 92 % Wahrnehmung",
         "CDN à paliers : 8 % de trafic, 92 % de perception",
         "CDN por niveles: 8 % de tráfico, 92 % de percepción"]),
    ("about.cases.cdn.body",
        ["First paint pulls only the coarse layers (≈8% of the bytes); the browser swaps in detail layers as bandwidth allows. Same file, no separate LQIP / thumbnail pipeline.",
         "Первый кадр тянет только нижние слои (~8% объёма); по мере доступной полосы догружаются детали. Тот же файл, без отдельного LQIP-пайплайна и без миниатюр.",
         "Erstes Frame holt nur die groben Schichten (~8 % der Bytes); der Browser ergänzt Detailschichten je nach Bandbreite. Gleiche Datei, kein separates LQIP- / Thumbnail-Pipeline.",
         "Le premier rendu ne récupère que les couches grossières (~8 % des octets) ; le navigateur ajoute les couches de détail selon la bande passante. Même fichier, pas de pipeline LQIP / vignette séparé.",
         "El primer render solo recupera las capas gruesas (~8 % de los bytes); el navegador añade capas de detalle según ancho de banda. Mismo archivo, sin pipeline LQIP / miniatura aparte."]),
    ("about.cases.plagiarism.h",
        ["Document overlap / plagiarism scan",
         "Поиск пересечений / плагиата в документах",
         "Dokumentüberschneidung / Plagiats-Check",
         "Recoupement / scan plagiat de documents",
         "Solape / escaneo de plagio en documentos"]),
    ("about.cases.plagiarism.body",
        ["Byte-shard hashes on text objects yield instant copy detection across the archive: «this spec copies 38% of contract X». No NLP, no embeddings, no token budget.",
         "Хеши шардов на текстовых объектах дают мгновенный детект копий по архиву: «эта спека на 38% копирует контракт X». Без NLP, без эмбеддингов, без токен-бюджета.",
         "Byte-Shard-Hashes auf Textobjekten liefern sofortige Kopieerkennung im Archiv: „diese Spec übernimmt 38 % von Vertrag X“. Ohne NLP, ohne Embeddings, ohne Token-Budget.",
         "Les hachages de shards octet sur les objets texte donnent une détection instantanée des copies à travers l'archive : « cette spec reprend 38 % du contrat X ». Sans NLP, sans embeddings, sans budget de tokens.",
         "Los hashes de shards byte sobre objetos de texto dan detección instantánea de copias en todo el archivo: «esta spec copia 38 % del contrato X». Sin NLP, sin embeddings, sin presupuesto de tokens."]),
    ("about.cases.audio.h",
        ["Auto-tag speech vs music vs SFX",
         "Авто-тег: речь / музыка / эффекты",
         "Auto-Tagging Sprache / Musik / SFX",
         "Auto-tag parole / musique / SFX",
         "Auto-etiqueta voz / música / SFX"]),
    ("about.cases.audio.body",
        ["Audio energy split by Haar layer maps to bass / mid / treble bands. Speech concentrates in mid; music spreads; SFX spikes high. One scan, no audio classifiers required.",
         "Энергия аудио по слоям Haar = бас / средние / высокие. Речь концентрируется в средних, музыка размазана, эффекты бьют в верхние. Один проход, без классификаторов.",
         "Audio-Energie nach Haar-Schicht ergibt Bass / Mitte / Höhen. Sprache konzentriert sich in der Mitte, Musik streut, SFX-Spitzen oben. Ein Pass, kein Audio-Klassifizierer nötig.",
         "L'énergie audio par couche Haar donne basses / médiums / aigus. La parole se concentre en médium, la musique s'étale, les SFX piquent en aigu. Un seul passage, pas de classifieur audio requis.",
         "La energía audio por capa Haar da graves / medios / agudos. La voz se concentra en medios, la música se reparte, los SFX picos arriba. Una pasada, sin clasificador de audio."]),
    ("about.cases.escrow.h",
        ["Cryptographic escrow without trust",
         "Криптографический эскроу без доверия",
         "Kryptographische Treuhand ohne Vertrauen",
         "Séquestre cryptographique sans confiance",
         "Depósito criptográfico sin confianza"]),
    ("about.cases.escrow.body",
        ["Same RLNC math powers escrow: split any file into n shares such that any k recover it. M&A data rooms, key escrow, last-will distribution — no single-party reliance.",
         "Та же RLNC-математика — эскроу: режем файл на n частей, любые k восстанавливают. M&A data-room, эскроу ключей, цифровое завещание — без доверия одному.",
         "Dieselbe RLNC-Mathematik treibt Treuhand: jede Datei in n Anteile splitten, beliebige k rekonstruieren. M&A-Datenräume, Schlüssel-Treuhand, digitales Testament — keine Abhängigkeit von einer Partei.",
         "Même mathématique RLNC pour le séquestre : découper un fichier en n parts dont k quelconques le reconstituent. Data-rooms M&A, séquestre de clés, testament numérique — sans dépendance à un tiers unique.",
         "Las mismas matemáticas RLNC para el depósito: dividir un archivo en n partes con k cualesquiera recuperándolo. Data-rooms M&A, depósito de claves, testamento digital — sin depender de una parte."]),
    ("about.cta.h",
        ["See it on a real file",
         "Посмотреть на живом файле",
         "An einer realen Datei sehen",
         "Voir sur un fichier réel",
         "Verlo en un archivo real"]),
    ("about.cta.body",
        ["Open the catalog, pick any image, and follow the «health» link on its row. Every metric on this page is computed live against the file you choose.",
         "Открой каталог, выбери любую картинку, перейди по ссылке «health» в её строке. Все метрики, описанные здесь, считаются в живую для выбранного файла.",
         "Katalog öffnen, ein beliebiges Bild wählen und dem „health“-Link in seiner Zeile folgen. Jede Kennzahl wird live für die gewählte Datei berechnet.",
         "Ouvrez le catalogue, choisissez une image et suivez le lien « health » de sa ligne. Chaque indicateur est calculé en direct pour le fichier sélectionné.",
         "Abre el catálogo, elige cualquier imagen y sigue el enlace «health» de su fila. Cada métrica se calcula en vivo para el archivo elegido."]),
    ("about.cta.button",
        ["Open the catalog →", "Открыть каталог →", "Katalog öffnen →",
         "Ouvrir le catalogue →", "Abrir el catálogo →"]),
];

/// Order locales appear in the language picker. First entry is the default.
pub const LOCALE_ORDER: &[&str] = &["en", "ru", "de", "fr", "es"];

/// Display label per locale (native script).
pub const LOCALE_LABEL: &[(&str, &str)] = &[
    ("en", "English"),
    ("ru", "Русский"),
    ("de", "Deutsch"),
    ("fr", "Français"),
    ("es", "Español"),
];

fn locale_idx(code: &str) -> usize {
    match code {
        "en" => 0,
        "ru" => 1,
        "de" => 2,
        "fr" => 3,
        "es" => 4,
        _ => 0,
    }
}

/// Look up `key` in [`TRANSLATIONS`] for `locale`. Falls back to English
/// when the key isn't translated; falls back to the key itself when it
/// isn't in the table at all (so a typo in source produces a visible "X
/// not translated" rather than panicking).
pub fn translate(key: &str, locale: &str) -> &'static str {
    let idx = locale_idx(locale);
    for (k, vals) in TRANSLATIONS {
        if *k == key {
            let v = vals[idx];
            if !v.is_empty() {
                return v;
            }
            return vals[0]; // fallback to English
        }
    }
    // Final fallback: leave the key so it's visible in the UI. This is
    // intentionally not a panic — a missing translation should be a
    // bug-report nudge, not a crash.
    leak_key(key)
}

/// `t!("nav.catalog")` — translate the literal key for the current
/// request's locale. Read via [`current_locale`].
#[macro_export]
macro_rules! t {
    ($key:literal) => {
        $crate::i18n::translate($key, &$crate::i18n::current_locale())
    };
}

/// Locale picker rendered in every page's topbar. Each entry is a link
/// that preserves the current path + query but rewrites (or adds) the
/// `lang` parameter. SSR-friendly — no JS, plain `<a>` tags.
#[component]
pub fn LocaleSwitcher() -> impl IntoView {
    use leptos_router::hooks::use_location;
    let location = use_location();
    let active = move || current_locale();
    view! {
        <span class="locale-switcher">
            {LOCALE_LABEL.iter().enumerate().map(|(i, (code, label))| {
                let code = *code;
                let label = *label;
                let location = location.clone();
                let href = move || {
                    let path = location.pathname.get();
                    let q = location.search.get();
                    rewrite_lang(&path, &q, code)
                };
                let separator = if i == 0 { None } else { Some(" · ") };
                let cls = move || if active() == code { "active" } else { "" };
                view! {
                    {separator.map(|s| s.to_string())}
                    <a class=cls href=href title=label>{code}</a>
                }
            }).collect_view()}
        </span>
    }
}

/// Replace `lang=` in `query_str` with `new_lang`, or append it. Keeps
/// the rest of the query intact so locale switches preserve `?p=…` etc.
/// Pure string transformation; no `url`-crate dependency.
pub fn rewrite_lang(path: &str, query_str: &str, new_lang: &str) -> String {
    let q = query_str.trim_start_matches('?');
    let mut pieces: Vec<String> = if q.is_empty() {
        Vec::new()
    } else {
        q.split('&').map(|p| p.to_string()).collect()
    };
    let mut replaced = false;
    for piece in &mut pieces {
        if let Some(k) = piece.split('=').next() {
            if k == "lang" {
                *piece = format!("lang={new_lang}");
                replaced = true;
            }
        }
    }
    if !replaced {
        pieces.push(format!("lang={new_lang}"));
    }
    let new_q = pieces.join("&");
    if new_q.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{new_q}")
    }
}

// Holds string literals leaked from `translate` when the key is unknown.
// Tiny growing cache so we don't leak unbounded memory on a runaway
// typo loop. In practice the set stabilises at zero in CI.
fn leak_key(key: &str) -> &'static str {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, &'static str>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut g = cache.lock().expect("i18n leak_key mutex");
    if let Some(s) = g.get(key) {
        return s;
    }
    let leaked: &'static str = Box::leak(key.to_string().into_boxed_str());
    g.insert(key.to_string(), leaked);
    leaked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_known_key() {
        assert_eq!(translate("nav.catalog", "ru"), "каталог");
        assert_eq!(translate("nav.catalog", "de"), "Katalog");
        assert_eq!(translate("nav.catalog", "es"), "catálogo");
        assert_eq!(translate("nav.catalog", "en"), "catalog");
    }

    #[test]
    fn translate_unknown_locale_falls_to_english() {
        assert_eq!(translate("nav.catalog", "ja"), "catalog");
    }

    #[test]
    fn translate_unknown_key_returns_key() {
        let s = translate("nav.nonexistent", "ru");
        assert_eq!(s, "nav.nonexistent");
    }

    #[test]
    fn resolve_query_wins_over_cookie_and_header() {
        let l = resolve_locale(Some("de"), Some("ru"), Some("fr,en"));
        assert_eq!(l, "de");
    }

    #[test]
    fn resolve_cookie_wins_over_header() {
        let l = resolve_locale(None, Some("es"), Some("fr,en"));
        assert_eq!(l, "es");
    }

    #[test]
    fn resolve_accept_language_strips_region_and_quality() {
        let l = resolve_locale(None, None, Some("ja-JP,fr-CA;q=0.7,en;q=0.3"));
        assert_eq!(l, "fr");
    }

    #[test]
    fn resolve_unknown_falls_to_english() {
        let l = resolve_locale(Some("xx"), Some("yy"), Some("zz"));
        assert_eq!(l, "en");
    }

    #[test]
    fn translations_table_consistent_widths() {
        // Every row must have exactly 5 translation slots (en, ru, de, fr, es).
        for (k, vals) in TRANSLATIONS {
            assert_eq!(vals.len(), 5, "key {k} has wrong column count");
        }
    }

    #[test]
    fn rewrite_lang_replaces_existing() {
        assert_eq!(rewrite_lang("/", "?lang=ru", "de"), "/?lang=de");
    }

    #[test]
    fn rewrite_lang_appends_when_missing() {
        assert_eq!(rewrite_lang("/help", "?p=foo", "ru"), "/help?p=foo&lang=ru");
    }

    #[test]
    fn rewrite_lang_preserves_path_with_no_query() {
        assert_eq!(rewrite_lang("/catalog", "", "fr"), "/catalog?lang=fr");
    }

    #[test]
    fn rewrite_lang_preserves_other_params() {
        let r = rewrite_lang("/", "?p=photos&lang=en&zoom=2", "es");
        assert!(r.contains("p=photos"));
        assert!(r.contains("lang=es"));
        assert!(r.contains("zoom=2"));
        assert!(!r.contains("lang=en"));
    }
}
