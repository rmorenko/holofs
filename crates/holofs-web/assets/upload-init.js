// progressive enhancement for the catalog upload form.
//
// Without JS: clicking the styled <label> opens the native file picker,
// pressing "upload" submits the form. Works fine.
//
// With JS: dragging a file onto the dashed `.upload-form` box (or
// anywhere within) sets it as the input's selection; the chosen
// filename is mirrored into `.file-name` so the user has feedback
// before clicking submit. A `drag-active` class toggles a hover state
// while a file is hovering over the drop target.

(function () {
    "use strict";

    function setupForm(formBox) {
        var input = formBox.querySelector('input[type="file"]');
        var nameLabel = formBox.querySelector('.file-name');
        if (!input) return;

        // ---- filename preview ----------------------------------------
        var originalText = nameLabel ? nameLabel.textContent : '';
        function updateName() {
            if (!nameLabel) return;
            if (input.files && input.files.length > 0) {
                var f = input.files[0];
                var size = f.size;
                var size_s;
                if (size >= 1024 * 1024) size_s = (size / (1024*1024)).toFixed(1) + ' MB';
                else if (size >= 1024) size_s = (size / 1024).toFixed(1) + ' KB';
                else size_s = size + ' B';
                nameLabel.textContent = f.name + ' · ' + size_s;
                nameLabel.classList.remove('mut');
            } else {
                nameLabel.textContent = originalText;
                nameLabel.classList.add('mut');
            }
        }
        input.addEventListener('change', updateName);

        // ---- drag and drop -------------------------------------------
        // Prevent the browser's default "open the file in a new tab"
        // when the user accidentally misses the drop zone.
        ['dragenter','dragover','dragleave','drop'].forEach(function (ev) {
            formBox.addEventListener(ev, function (e) {
                e.preventDefault();
                e.stopPropagation();
            });
        });
        formBox.addEventListener('dragenter', function () {
            formBox.classList.add('drag-active');
        });
        formBox.addEventListener('dragover', function () {
            formBox.classList.add('drag-active');
        });
        formBox.addEventListener('dragleave', function (e) {
            // The dragleave fires for every child element traversed.
            // Only clear the highlight when leaving the actual box.
            if (e.target === formBox) {
                formBox.classList.remove('drag-active');
            }
        });
        formBox.addEventListener('drop', function (e) {
            formBox.classList.remove('drag-active');
            if (!e.dataTransfer || !e.dataTransfer.files || e.dataTransfer.files.length === 0) return;
            // FileList is read-only; we use DataTransfer to build a fresh one.
            var dt = new DataTransfer();
            dt.items.add(e.dataTransfer.files[0]);
            input.files = dt.files;
            updateName();
        });
    }

    function init() {
        document.querySelectorAll('.upload-form').forEach(setupForm);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init);
    } else {
        init();
    }
})();
