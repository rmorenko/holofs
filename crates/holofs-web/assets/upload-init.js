// Progressive enhancement for every upload form on the page.
//
// Pre-unification the styled focus-view `UploadForm` (`.upload-form`
// block) got drag-drop + filename preview here, but the tree-inline
// / tree-root uploads were bare `<input type="file">` — no
// drag-drop, no filename echo. UI-UX review flagged the split. This
// asset now targets ANY `<form>` on the page that contains an
// `input[type="file"][name="file"]`, matching the mutation endpoint
// whitelist mutation-forms.js already uses.
//
// Without JS: every form still works — the native picker opens on
// button click, the submit button posts the form.
//
// With JS:
//   · dragging a file onto the surrounding <form> sets it as the
//     input's selection (highlight via `.drag-active`);
//   · the chosen filename + human-readable size mirror into a sibling
//     `.file-name` span — pre-existing on `.upload-form`, injected
//     for the tree variants right after the input.
//
// Coexists with mutation-forms.js: this file is about *input
// preparation*; mutation-forms.js catches the submit and does the
// fetch/XHR round-trip + toast.

(function () {
    "use strict";

    function humanSize(size) {
        if (size >= 1024 * 1024) return (size / (1024 * 1024)).toFixed(1) + ' MB';
        if (size >= 1024) return (size / 1024).toFixed(1) + ' KB';
        return size + ' B';
    }

    function ensureFileName(form, input) {
        // Prefer the pre-rendered slot when the styled `.upload-form`
        // laid one out with the right typography. For every other form
        // (tree inline / tree root / any future caller), inject a tiny
        // inline span right after the input.
        var existing = form.querySelector('.file-name');
        if (existing) return existing;
        var span = document.createElement('span');
        span.className = 'file-name file-name-inline mut';
        input.parentNode.insertBefore(span, input.nextSibling);
        return span;
    }

    function setupForm(form) {
        var input = form.querySelector('input[type="file"][name="file"]');
        if (!input) return;
        if (input.dataset.holofsUploadInit === '1') return;
        input.dataset.holofsUploadInit = '1';

        var nameLabel = ensureFileName(form, input);
        var originalText = nameLabel.textContent || '';

        function updateName() {
            if (input.files && input.files.length > 0) {
                var f = input.files[0];
                nameLabel.textContent = f.name + ' · ' + humanSize(f.size);
                nameLabel.classList.remove('mut');
            } else {
                nameLabel.textContent = originalText;
                nameLabel.classList.add('mut');
            }
        }
        input.addEventListener('change', updateName);

        // ---- drag and drop -------------------------------------------
        // Prevent the browser's default "open the file in a new tab"
        // when the user misses the target. Wired to the <form> itself
        // so any bounding box (styled or bare) becomes a drop zone.
        ['dragenter', 'dragover', 'dragleave', 'drop'].forEach(function (ev) {
            form.addEventListener(ev, function (e) {
                e.preventDefault();
                e.stopPropagation();
            });
        });
        form.addEventListener('dragenter', function () {
            form.classList.add('drag-active');
        });
        form.addEventListener('dragover', function () {
            form.classList.add('drag-active');
        });
        form.addEventListener('dragleave', function (e) {
            // dragleave fires on every child; only clear on true form-exit.
            if (e.target === form) form.classList.remove('drag-active');
        });
        form.addEventListener('drop', function (e) {
            form.classList.remove('drag-active');
            if (!e.dataTransfer || !e.dataTransfer.files || e.dataTransfer.files.length === 0) return;
            var dt = new DataTransfer();
            dt.items.add(e.dataTransfer.files[0]);
            input.files = dt.files;
            updateName();
        });
    }

    function init() {
        document.querySelectorAll('form').forEach(setupForm);
    }

    // Re-scan on hydrate: Leptos may insert additional upload forms
    // (LazyDirNode etc.) after DOMContentLoaded, so wire a
    // MutationObserver in addition to the one-time init pass.
    function watchNewForms() {
        if (typeof MutationObserver !== 'function') return;
        var obs = new MutationObserver(function (records) {
            for (var i = 0; i < records.length; i++) {
                var added = records[i].addedNodes;
                for (var j = 0; j < added.length; j++) {
                    var node = added[j];
                    if (node.nodeType !== 1) continue;
                    if (node.tagName === 'FORM') setupForm(node);
                    if (node.querySelectorAll) {
                        node.querySelectorAll('form').forEach(setupForm);
                    }
                }
            }
        });
        obs.observe(document.body, { childList: true, subtree: true });
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function () {
            init();
            watchNewForms();
        });
    } else {
        init();
        watchNewForms();
    }
})();
