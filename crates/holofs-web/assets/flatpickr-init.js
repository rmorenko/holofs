// flatpickr init for the catalog filter bar.
//
// The server already renders `<input type="date" name="from" lang="…">`
// which works on its own — flatpickr is progressive enhancement: when
// the lib + the requested locale bundle have loaded, we replace the
// native picker with a cross-browser one whose UI matches the page
// language. If the CDN is unreachable the native input stays usable.
//
// We pin `dateFormat: "Y-m-d"` so the form submission still hits the
// server's YYYY-MM-DD parser, regardless of how the locale would
// otherwise prefer to display dates.

(function () {
    "use strict";

    function ready(fn) {
        if (document.readyState === "loading") {
            document.addEventListener("DOMContentLoaded", fn);
        } else {
            fn();
        }
    }

    function pickLocale(input) {
        if (typeof flatpickr === "undefined") return null;
        var lang = input.getAttribute("lang") || document.documentElement.lang || "en";
        lang = lang.toLowerCase();
        if (lang === "en") return "default";
        // flatpickr.l10ns is the locale registry. Each l10n/<code>.js
        // bundle registers itself under flatpickr.l10ns[code] when it
        // loads. Fall back to default if the requested locale never
        // arrived (e.g. CDN partial failure).
        if (flatpickr.l10ns && flatpickr.l10ns[lang]) {
            return flatpickr.l10ns[lang];
        }
        return "default";
    }

    function init() {
        if (typeof flatpickr === "undefined") return;
        var inputs = document.querySelectorAll('input[type="date"]');
        inputs.forEach(function (el) {
            // Skip if a previous run already wrapped this input — the
            // hydration pass may re-trigger init on the same elements.
            if (el._flatpickr) return;
            try {
                flatpickr(el, {
                    locale: pickLocale(el),
                    dateFormat: "Y-m-d",
                    allowInput: true
                });
            } catch (e) {
                // Quiet failure: native input continues to work.
                if (console && console.warn) {
                    console.warn("flatpickr init failed:", e);
                }
            }
        });
    }

    ready(init);
    // Leptos hydration may insert nodes after DOMContentLoaded. Re-run
    // once on `load` to catch them; idempotent thanks to the
    // `el._flatpickr` guard above.
    window.addEventListener("load", init);
})();
