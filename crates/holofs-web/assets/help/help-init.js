// client-side init for the /help docs viewer.
//
// Two libraries are loaded from CDN by the page itself; this shim:
// 1. wires KaTeX auto-render to scan rendered doc bodies for `\(…\)` /
//    `\[…\]` delimiters (which is what our server-side markdown renderer
//    emits in place of `$…$` and `$$…$$`);
// 2. boots Mermaid in `loose` security mode with theme-friendly defaults
//    and renders every `<div class="mermaid">` block.
//
// The script is `defer`'d so it runs after the DOM is parsed. We also
// listen for `DOMContentLoaded` as a safety net.

(function () {
    "use strict";

    function renderKatex() {
        if (typeof renderMathInElement !== "function") return;
        var nodes = document.querySelectorAll(".help-body");
        nodes.forEach(function (n) {
            renderMathInElement(n, {
                delimiters: [
                    { left: "\\(", right: "\\)", display: false },
                    { left: "\\[", right: "\\]", display: true },
                    { left: "$$", right: "$$", display: true }
                ],
                throwOnError: false,
                strict: "ignore"
            });
        });
    }

    function bootMermaid() {
        if (typeof mermaid === "undefined") return;
        mermaid.initialize({
            startOnLoad: false,
            theme: "dark",
            securityLevel: "loose",
            fontFamily: "ui-sans-serif, system-ui, sans-serif"
        });
        mermaid.run({ querySelector: ".mermaid" }).catch(function (e) {
            console.warn("mermaid render failed:", e);
        });
    }

    function init() {
        // KaTeX auto-render is also `defer`'d — give it a tick to settle.
        setTimeout(function () {
            try { renderKatex(); } catch (e) { console.warn("katex:", e); }
            try { bootMermaid(); } catch (e) { console.warn("mermaid:", e); }
        }, 0);
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", init);
    } else {
        init();
    }
})();
