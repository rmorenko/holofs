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
