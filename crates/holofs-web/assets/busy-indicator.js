// Stage 16: global "something is loading" indicator.
//
// Exposes `window.holofsBusy(true|false)` as a counter-based gate
// — every `true` increments, every `false` decrements, and the
// thin top-of-viewport progress bar is visible iff the counter is
// > 0. We auto-instrument `fetch`, `XMLHttpRequest` and HTML
// `<form>` submissions so 95% of the UI gets a free progress
// indicator without per-call wiring. Pages that need an explicit
// per-button state still wrap their submit handler around
// `holofsBusy(true) / (false)` so the button can disable itself
// independent of the global bar.

(function () {
    "use strict";

    var count = 0;
    var ROOT = document.documentElement;

    function flip() {
        if (count > 0) {
            ROOT.classList.add("holofs-busy");
        } else {
            ROOT.classList.remove("holofs-busy");
        }
    }

    window.holofsBusy = function (on) {
        if (on) {
            count++;
        } else {
            count = Math.max(0, count - 1);
        }
        flip();
    };

    // 1. fetch wrapper. Most leptos server-fn calls go through
    //    this; we want every one of them to ping the indicator.
    var origFetch = window.fetch;
    if (typeof origFetch === "function") {
        window.fetch = function () {
            window.holofsBusy(true);
            var done = false;
            var release = function () {
                if (done) return;
                done = true;
                window.holofsBusy(false);
            };
            try {
                return origFetch.apply(this, arguments).then(
                    function (r) { release(); return r; },
                    function (e) { release(); throw e; }
                );
            } catch (e) {
                release();
                throw e;
            }
        };
    }

    // 2. XMLHttpRequest wrapper. Belt-and-suspenders for the few
    //    callers that still use XHR (older form helpers, fetch
    //    fallback shims, etc.). Resolved either on `loadend` (the
    //    universal "I'm done" event) or via abort/error guards.
    var XHR = window.XMLHttpRequest && window.XMLHttpRequest.prototype;
    if (XHR && XHR.send) {
        var origSend = XHR.send;
        XHR.send = function () {
            var that = this;
            window.holofsBusy(true);
            var done = false;
            var release = function () {
                if (done) return;
                done = true;
                window.holofsBusy(false);
            };
            that.addEventListener("loadend", release);
            try {
                return origSend.apply(this, arguments);
            } catch (e) {
                release();
                throw e;
            }
        };
    }

    // 3. Form submissions trigger a full navigation, so there's no
    //    promise to await — instead we light the bar and disable
    //    the submitter button. The browser navigates within a tick
    //    or two and the next page boots clean (the counter resets
    //    because this script runs again from scratch).
    document.addEventListener("submit", function (ev) {
        var form = ev.target;
        if (!form || form.tagName !== "FORM") return;
        // Honour `data-holofs-no-busy` so loud spammable forms
        // (eg search-as-you-type) can opt out.
        if (form.dataset && form.dataset.holofsNoBusy === "1") return;

        window.holofsBusy(true);

        // Disable the submitter so the user can't double-fire.
        // Keep visible label intact so the button doesn't reflow;
        // CSS adds the spinner glyph via `:disabled` styling.
        var btn = ev.submitter ||
            form.querySelector("button[type='submit'], input[type='submit']");
        if (btn && !btn.disabled) {
            btn.disabled = true;
            btn.dataset.holofsWasBusy = "1";
            // 30 s safety net: re-enable if navigation never
            // completed (e.g. the server returned a 4xx and the
            // browser stayed on the current page). Without this
            // a failed submit leaves the button permanently dead.
            setTimeout(function () {
                if (btn.dataset.holofsWasBusy === "1") {
                    btn.disabled = false;
                    delete btn.dataset.holofsWasBusy;
                    window.holofsBusy(false);
                }
            }, 30000);
        }
    }, true);

    // 4. Inject the visual indicator. Plain <div> at body-start;
    //    CSS positions it `fixed` so it never reflows layout.
    function inject() {
        if (document.getElementById("holofs-progress")) return;
        var b = document.body;
        if (!b) return;
        var d = document.createElement("div");
        d.id = "holofs-progress";
        d.setAttribute("aria-hidden", "true");
        b.insertBefore(d, b.firstChild);
    }
    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", inject);
    } else {
        inject();
    }
})();
