//! Server-side markdown → HTML renderer used by [`super::get_doc`].
//!
//! Built on `pulldown-cmark`. Two non-default behaviours:
//! 1. ``` ```mermaid``` fenced blocks emit `<div class="mermaid">…</div>`
//!    with their literal source so the client-side Mermaid loader can pick
//!    them up. The block body is HTML-escaped so it cannot smuggle `<script>`
//!    via raw markdown.
//! 2. `$inline$` and `$$display$$` math (`ENABLE_MATH`) emit `\(…\)` and
//!    `\[…\]` wrappers — the canonical KaTeX auto-render delimiters.
//!
//! Doc files live under `docs/<locale>/<slug>.md`. English is the
//! fallback locale and may also live directly at `docs/<slug>.md` for
//! backwards-compat with the pre-Stage-10 layout.

use std::path::PathBuf;

use pulldown_cmark::{html, CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};

/// Resolve `<locale>/<slug>.md` to a real path on disk. Tries the
/// localized variant first, then the English fallback at `docs/<slug>.md`,
/// then `docs/en/<slug>.md`. Returns the locale that was actually served.
pub fn read_doc(locale: &str, slug: &str) -> std::io::Result<(String, String)> {
    let candidates: [(String, PathBuf); 3] = [
        (
            locale.to_string(),
            PathBuf::from("docs").join(locale).join(format!("{slug}.md")),
        ),
        (
            "en".to_string(),
            PathBuf::from("docs").join("en").join(format!("{slug}.md")),
        ),
        (
            "en".to_string(),
            PathBuf::from("docs").join(format!("{slug}.md")),
        ),
    ];
    for (loc, path) in candidates {
        if let Ok(s) = std::fs::read_to_string(&path) {
            return Ok((loc, s));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("doc not found: {locale}/{slug}"),
    ))
}

/// Read just the H1 of a localized doc, for the sidebar. Returns `None`
/// when the file doesn't exist or has no `# heading`.
pub fn read_h1(locale: &str, slug: &str) -> Option<String> {
    let (_, md) = read_doc(locale, slug).ok()?;
    extract_h1(&md)
}

/// Find the first `# heading` line in `md`. Falls back through ATX (`# `)
/// then Setext (`====`) styles.
pub fn extract_h1(md: &str) -> Option<String> {
    for line in md.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Markdown → HTML with the Mermaid and KaTeX transforms above.
pub fn render_markdown(md: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_MATH);
    options.insert(Options::ENABLE_GFM);
    // Smart punctuation off — we want exact source rendering for code-heavy docs.

    let parser = Parser::new_ext(md, options);

    let mut out_events: Vec<Event<'_>> = Vec::new();
    let mut in_mermaid = false;
    let mut mermaid_src = String::new();

    for ev in parser {
        match ev {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(ref lang)))
                if lang.as_ref() == "mermaid" =>
            {
                in_mermaid = true;
                mermaid_src.clear();
            }
            Event::End(TagEnd::CodeBlock) if in_mermaid => {
                in_mermaid = false;
                // textContent of the div carries the literal mermaid
                // source; we HTML-escape to keep the DOM parser from
                // interpreting `-->`/`<br/>` etc.
                let html_block = format!(
                    "<div class=\"mermaid\">{}</div>",
                    html_escape(&mermaid_src)
                );
                out_events.push(Event::Html(CowStr::from(html_block)));
            }
            Event::Text(t) if in_mermaid => {
                mermaid_src.push_str(&t);
            }
            Event::InlineMath(expr) => {
                let html_block = format!(
                    r#"<span class="math math-inline">\({}\)</span>"#,
                    html_escape(&expr)
                );
                out_events.push(Event::Html(CowStr::from(html_block)));
            }
            Event::DisplayMath(expr) => {
                let html_block = format!(
                    r#"<div class="math math-display">\[{}\]</div>"#,
                    html_escape(&expr)
                );
                out_events.push(Event::Html(CowStr::from(html_block)));
            }
            // Stage 11.14: rewrite relative `*.md` links so they land
            // on the in-app help viewer instead of trying to GET a
            // catalog object. Source docs link to siblings via
            // `[theory.md](./theory.md)` for human readability; in the
            // rendered HTML we point those at `/help/<slug>`.
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let new_dest = rewrite_md_link(&dest_url);
                out_events.push(Event::Start(Tag::Link {
                    link_type,
                    dest_url: CowStr::from(new_dest),
                    title,
                    id,
                }));
            }
            other => out_events.push(other),
        }
    }

    let mut out = String::with_capacity(md.len() * 2);
    html::push_html(&mut out, out_events.into_iter());
    out
}

/// Rewrite a markdown link target so relative `.md` paths point at the
/// in-app `/help/<slug>` route. Absolute URLs (http, https, mailto) and
/// non-`.md` paths pass through unchanged. Anchor fragments survive —
/// `./theory.md#section` becomes `/help/theory#section`.
fn rewrite_md_link(dest: &str) -> String {
    // Leave external links alone.
    let lowered = dest.to_ascii_lowercase();
    if lowered.starts_with("http://")
        || lowered.starts_with("https://")
        || lowered.starts_with("mailto:")
        || lowered.starts_with("//")
    {
        return dest.to_string();
    }
    // Split off the anchor (#...) so we can put it back after rewriting
    // the path part.
    let (path_part, anchor) = match dest.split_once('#') {
        Some((p, a)) => (p, Some(a)),
        None => (dest, None),
    };
    // Only rewrite if the path actually ends in `.md`.
    let stripped = path_part.trim_start_matches("./").trim_start_matches("../");
    // Drop any directory prefix — the doc viewer only knows flat slugs.
    let leaf = stripped.rsplit('/').next().unwrap_or(stripped);
    if let Some(slug) = leaf.strip_suffix(".md") {
        let mut out = format!("/help/{slug}");
        if let Some(a) = anchor {
            out.push('#');
            out.push_str(a);
        }
        return out;
    }
    dest.to_string()
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_markdown() {
        let html = render_markdown("# title\n\nhello **world**\n");
        assert!(html.contains("<h1>title</h1>"));
        assert!(html.contains("<strong>world</strong>"));
    }

    #[test]
    fn mermaid_block_becomes_mermaid_div() {
        let md = "```mermaid\nflowchart LR\n  A --> B\n```\n";
        let html = render_markdown(md);
        assert!(html.contains(r#"<div class="mermaid">"#));
        // `-->` must be HTML-escaped inside the div body.
        assert!(html.contains("--&gt;"));
        // No <pre><code class="language-mermaid"> wrapper survived.
        assert!(!html.contains("language-mermaid"));
    }

    #[test]
    fn inline_math_uses_katex_paren_delimiters() {
        let html = render_markdown("see $x^2 + y^2$\n");
        assert!(html.contains(r#"<span class="math math-inline">\(x^2 + y^2\)</span>"#));
    }

    #[test]
    fn display_math_uses_katex_bracket_delimiters() {
        let html = render_markdown("$$\nE = mc^2\n$$\n");
        assert!(html.contains(r#"<div class="math math-display">\["#));
        assert!(html.contains(r"E = mc^2"));
        assert!(html.contains(r"\]"));
    }

    #[test]
    fn extract_h1_handles_atx() {
        assert_eq!(extract_h1("# Title\n\nbody"), Some("Title".to_string()));
        assert_eq!(extract_h1("no heading"), None);
    }

    #[test]
    fn rewrite_md_link_relative_sibling() {
        assert_eq!(rewrite_md_link("./theory.md"), "/help/theory");
        assert_eq!(rewrite_md_link("../api.md"), "/help/api");
        assert_eq!(rewrite_md_link("operations.md"), "/help/operations");
        assert_eq!(
            rewrite_md_link("./test-scenarios.md#1-bring-up-the-cluster"),
            "/help/test-scenarios#1-bring-up-the-cluster"
        );
    }

    #[test]
    fn rewrite_md_link_external_pass_through() {
        assert_eq!(
            rewrite_md_link("https://example.com/file.md"),
            "https://example.com/file.md"
        );
        assert_eq!(rewrite_md_link("mailto:hi@example.com"), "mailto:hi@example.com");
        assert_eq!(rewrite_md_link("./image.png"), "./image.png");
    }

    #[test]
    fn md_link_in_doc_gets_rewritten() {
        let html = render_markdown("see [theory](./theory.md) for math");
        assert!(html.contains(r#"href="/help/theory""#));
        assert!(!html.contains("./theory.md"));
    }
}
