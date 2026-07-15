// Progressive-enhancement for mutation forms (mkdir / rmdir / rm /
// mv / upload / restore / versions/delete / batch-delete).
//
// Server still renders every mutation as a plain `<form method="POST"
// action="/api/...">` — no-JS clients keep the historical
// text/plain + 303-redirect flow. When JS is available AND the form
// carries `data-holofs-mutate="1"`, we hijack the submit:
//
//   1. preventDefault → collect FormData → fetch (or XHR for uploads
//      so we can surface `upload.onprogress` %).
//   2. On 2xx / 303: pop a green toast (message from
//      `data-holofs-mutate-ok`, if any) and either navigate to the
//      response's Location header or soft-reload the current URL.
//      No full-page raw-text takeover.
//   3. On 4xx / 5xx: pop a red toast with the response body
//      (env-jargon like `HOLOFS_UPLOAD_MAX_SIZE=…` is scrubbed) and
//      stay on the current page.
//
// A1 · UI-UX-review: the *worst* pain-point in the whole UI was the
// raw text/plain response on a failed mutation — every 4xx / 5xx
// replaced the entire page with a bare error string, no layout, no
// nav, no back button that made sense. This file kills that.
//
// A2 · UI-UX-review: upload progress. Fetch has no upload-progress
// callback, XHR does, so multipart-form-data submits go through XHR
// so we can render % in the toast.

(function () {
    "use strict";

    // --- Toast area ------------------------------------------------------

    var TOAST_ID = "holofs-toast-area";
    var OK_MS = 3000;   // green toast auto-dismiss
    var ERR_MS = 8000;  // red toast auto-dismiss

    function toastArea() {
        var el = document.getElementById(TOAST_ID);
        if (el) return el;
        el = document.createElement("div");
        el.id = TOAST_ID;
        el.setAttribute("aria-live", "polite");
        document.body.appendChild(el);
        return el;
    }

    function showToast(kind, message, opts) {
        opts = opts || {};
        var area = toastArea();
        var t = document.createElement("div");
        t.className = "holofs-toast holofs-toast-" + kind;
        // role/aria-live per WAI-ARIA APG toast pattern.
        t.setAttribute("role", kind === "err" ? "alert" : "status");
        t.setAttribute("aria-live", kind === "err" ? "assertive" : "polite");

        var msg = document.createElement("div");
        msg.className = "holofs-toast-msg";
        msg.textContent = message;
        t.appendChild(msg);

        if (opts.progress) {
            var bar = document.createElement("div");
            bar.className = "holofs-toast-progress";
            var fill = document.createElement("div");
            fill.className = "holofs-toast-progress-fill";
            bar.appendChild(fill);
            t.appendChild(bar);
            t.__progressFill = fill;
        }

        var close = document.createElement("button");
        close.className = "holofs-toast-close";
        close.type = "button";
        close.setAttribute("aria-label", "dismiss");
        close.textContent = "×";
        close.addEventListener("click", function () { dismiss(t); });
        t.appendChild(close);

        area.appendChild(t);
        if (!opts.sticky) {
            t.__timer = setTimeout(function () { dismiss(t); },
                kind === "err" ? ERR_MS : OK_MS);
        }
        return t;
    }

    function dismiss(t) {
        if (!t || !t.parentNode) return;
        if (t.__timer) { clearTimeout(t.__timer); t.__timer = null; }
        t.classList.add("holofs-toast-out");
        setTimeout(function () {
            if (t.parentNode) t.parentNode.removeChild(t);
        }, 200);
    }

    function updateProgress(t, pct, label) {
        if (!t || !t.__progressFill) return;
        var p = Math.max(0, Math.min(100, pct));
        t.__progressFill.style.width = p.toFixed(1) + "%";
        if (label) {
            var msg = t.querySelector(".holofs-toast-msg");
            if (msg) msg.textContent = label;
        }
    }

    // --- Error-body scrubbing --------------------------------------------

    // Strip env-variable jargon (`HOLOFS_UPLOAD_MAX_SIZE=1073741824`,
    // etc.) from server error bodies. The 413 response used to leak
    // the exact env name; end-users don't care.
    function scrubBody(text) {
        if (!text) return "";
        text = String(text).trim();
        // "(HOLOFS_FOO)" or "(HOLOFS_FOO=…)" → drop parenthetical.
        text = text.replace(/\s*\(HOLOFS_[A-Z0-9_]+(?:=[^)]*)?\)\s*/g, "");
        // Bare `HOLOFS_FOO=…` tokens.
        text = text.replace(/HOLOFS_[A-Z0-9_]+=\S+/g, "").trim();
        // Trailing punctuation from the scrub.
        text = text.replace(/[\s;,]+$/, "");
        // Cap at 240 chars so a runaway trace doesn't overflow the
        // toast; toast is visually capped anyway but keep the DOM tidy.
        if (text.length > 240) text = text.slice(0, 240) + "…";
        return text || "request failed";
    }

    // --- Navigation on success -------------------------------------------

    function afterSuccess(form, resp) {
        // Priority: explicit `data-holofs-mutate-after` attribute →
        // resp.url (fetch followed the 303 → this is the redirect
        // target) → current URL reload.
        //
        // We use `redirect: "follow"` on the fetch (not manual) so
        // the browser resolves 303 for us and hands back the target
        // URL via `resp.url`. Manual redirect gives us an
        // `opaqueredirect` response whose Location header is not
        // readable by the CORS-safety rules, defeating the whole
        // point of the intercept.
        var after = form && form.dataset && form.dataset.holofsMutateAfter;
        if (after) {
            window.location.assign(after);
            return;
        }
        if (resp && resp.url && resp.url !== window.location.href) {
            window.location.assign(resp.url);
            return;
        }
        window.location.reload();
    }

    // --- Form-level intercept --------------------------------------------

    // Whitelist of known-safe mutation endpoints. Auto-detect keeps
    // the leptos view files untouched — otherwise every form would
    // need a `data-holofs-mutate="1"` annotation. Explicit opt-out:
    // `data-holofs-mutate-optout="1"` on the <form>.
    var MUTATION_ACTIONS = [
        "/api/mkdir",   // JSON (POST /api/mkdir/<path>) + form (POST /api/mkdir)
        "/api/rmdir",   // JSON (DELETE /api/rmdir/<path>) + form (POST /api/rmdir)
        "/api/rm",
        "/api/mv",
        "/api/upload",
        "/api/restore",
        "/api/versions/delete",
        "/api/batch/delete",
    ];

    function actionMatches(action) {
        if (!action) return false;
        // Trim scheme+host so absolute + relative URLs both match.
        var path = action;
        try { path = new URL(action, window.location.href).pathname; }
        catch (_) { /* fall through — action is already a path */ }
        for (var i = 0; i < MUTATION_ACTIONS.length; i++) {
            var prefix = MUTATION_ACTIONS[i];
            if (path === prefix || path.indexOf(prefix + "/") === 0) return true;
        }
        return false;
    }

    function shouldIntercept(form) {
        if (!form || form.tagName !== "FORM") return false;
        if (!form.dataset) return false;
        // Explicit opt-in wins.
        if (form.dataset.holofsMutate === "1") return true;
        // Explicit opt-out.
        if (form.dataset.holofsMutateOptout === "1") return false;
        // Otherwise, decide by action URL. GET forms (filter/search)
        // stay full-page — no reason to fetch-hijack a navigation.
        var method = (form.method || "GET").toUpperCase();
        if (method === "GET" || method === "DIALOG") return false;
        return actionMatches(form.getAttribute("action") || form.action || "");
    }

    function isMultipart(form) {
        var enc = (form.getAttribute("enctype") || "").toLowerCase();
        return enc === "multipart/form-data";
    }

    function submitViaFetch(form, okMsg) {
        var fd = new FormData(form);
        return fetch(form.action || window.location.pathname, {
            method: (form.method || "POST").toUpperCase(),
            body: fd,
            // `follow` — let the browser resolve 303 so we can
            // navigate to the final URL via `resp.url`. See
            // `afterSuccess` comment for why manual doesn't work
            // (opaqueredirect hides Location).
            redirect: "follow",
            credentials: "same-origin",
        }).then(function (resp) {
            if (resp.ok) {
                if (okMsg) showToast("ok", okMsg);
                afterSuccess(form, resp);
                return;
            }
            return resp.text().then(function (body) {
                showToast("err", scrubBody(body) || ("HTTP " + resp.status));
            });
        }).catch(function (e) {
            showToast("err", "network error: " + (e && e.message ? e.message : e));
        });
    }

    function submitViaXhr(form, okMsg) {
        // XHR path exists so we can surface upload.onprogress %.
        var fd = new FormData(form);
        var xhr = new XMLHttpRequest();
        xhr.open((form.method || "POST").toUpperCase(),
                 form.action || window.location.pathname, true);
        xhr.withCredentials = true;
        // Progress toast: sticky (auto-dismiss would race a slow
        // upload). Replaced with a final ok/err toast once the request
        // resolves.
        var progressToast = showToast("ok",
            "uploading… 0%", { progress: true, sticky: true });

        if (xhr.upload) {
            xhr.upload.addEventListener("progress", function (ev) {
                if (!ev.lengthComputable) return;
                var pct = ev.loaded * 100 / ev.total;
                var mib = (ev.total / (1024 * 1024)).toFixed(1);
                updateProgress(progressToast, pct,
                    "uploading… " + pct.toFixed(0) + "% of " + mib + " MiB");
            });
        }
        xhr.addEventListener("load", function () {
            dismiss(progressToast);
            var ok = xhr.status >= 200 && xhr.status < 400;
            if (ok) {
                if (okMsg) showToast("ok", okMsg);
                // XHR follows 3xx by default; `responseURL` is the
                // final target. Same shape as fetch's `resp.url`.
                var after = form.dataset && form.dataset.holofsMutateAfter;
                if (after) {
                    window.location.assign(after);
                } else if (xhr.responseURL &&
                           xhr.responseURL !== window.location.href) {
                    window.location.assign(xhr.responseURL);
                } else {
                    window.location.reload();
                }
            } else {
                showToast("err",
                    scrubBody(xhr.responseText) || ("HTTP " + xhr.status));
            }
        });
        xhr.addEventListener("error", function () {
            dismiss(progressToast);
            showToast("err", "upload failed (network error)");
        });
        xhr.addEventListener("abort", function () {
            dismiss(progressToast);
            showToast("err", "upload aborted");
        });
        xhr.send(fd);
    }

    // Bubbling submit listener so it fires for late-added forms too.
    // `capture: false` lets other handlers (busy-indicator) run first.
    document.addEventListener("submit", function (ev) {
        var form = ev.target;
        if (!shouldIntercept(form)) return;
        ev.preventDefault();
        var okMsg = form.dataset.holofsMutateOk || "";

        // Disable submitter to prevent double-fire; re-enable on
        // failure so the user can retry.
        var btn = ev.submitter ||
            form.querySelector("button[type='submit'], input[type='submit']");
        if (btn) btn.disabled = true;

        var reenable = function () { if (btn) btn.disabled = false; };

        if (isMultipart(form)) {
            submitViaXhr(form, okMsg);
            // Re-enable button after a fixed window so a stuck upload
            // still lets the user cancel-and-retry via the browser.
            setTimeout(reenable, 60000);
        } else {
            submitViaFetch(form, okMsg).then(reenable, reenable);
        }
    }, false);

    // --- Dropdown click-outside close ------------------------------------
    //
    // Any <details class="js-dropdown"> auto-closes when the user
    // clicks outside its subtree. Native <details> keeps itself open
    // until the summary is clicked again — fine for content blocks,
    // awkward for popover menus (A6: ObjectCard "⋯" secondary
    // actions). Also Escape closes the most recent open dropdown.
    document.addEventListener("click", function (ev) {
        var openDetails = document.querySelectorAll("details.js-dropdown[open]");
        openDetails.forEach(function (d) {
            if (!d.contains(ev.target)) d.open = false;
        });
    });
    document.addEventListener("keydown", function (ev) {
        if (ev.key !== "Escape") return;
        var openDetails = document.querySelectorAll("details.js-dropdown[open]");
        if (openDetails.length === 0) return;
        // Close the last one (deepest / most recently opened).
        openDetails[openDetails.length - 1].open = false;
    });
})();
