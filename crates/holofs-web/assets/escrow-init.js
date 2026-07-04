// progressive enhancement for the two escrow forms.
//
// Without JS: clicking the styled <label> opens the native file
// picker, pressing submit posts the form. Works.
//
// With JS: each `.escrow-section` becomes a drop zone for its own
// file input. The chosen filename (or "N files chosen" for the
// multi-file recover form) shows in `.file-name` so the user has
// feedback before submit. A `drag-active` class toggles a hover state
// while a file is hovering over the section.
//
// The "N files chosen" string is pulled from a data-* attribute the
// server sets via the t! macro — we never hardcode English here.

(function () {
    "use strict";

    function fmtSize(n) {
        if (n >= 1024 * 1024) return (n / (1024 * 1024)).toFixed(1) + ' MB';
        if (n >= 1024) return (n / 1024).toFixed(1) + ' KB';
        return n + ' B';
    }

    function setupSection(section) {
        var input = section.querySelector('input[type="file"]');
        var nameLabel = section.querySelector('.file-name');
        if (!input) return;

        var originalText = nameLabel ? nameLabel.textContent : '';
        var multiTemplate = nameLabel
            ? (nameLabel.getAttribute('data-multi-template') || '{n} files')
            : '{n} files';
        var isMulti = !!input.hasAttribute('multiple');

        function updateName() {
            if (!nameLabel) return;
            var files = input.files;
            if (!files || files.length === 0) {
                nameLabel.textContent = originalText;
                nameLabel.classList.add('mut');
                return;
            }
            if (isMulti && files.length > 1) {
                nameLabel.textContent =
                    multiTemplate.replace('{n}', String(files.length));
            } else {
                var f = files[0];
                nameLabel.textContent = f.name + ' · ' + fmtSize(f.size);
            }
            nameLabel.classList.remove('mut');
        }
        input.addEventListener('change', updateName);

        // Drop zone: the whole .escrow-section reacts to drag events
        // and on drop hands the FileList over to the hidden input.
        ['dragenter', 'dragover', 'dragleave', 'drop'].forEach(function (ev) {
            section.addEventListener(ev, function (e) {
                e.preventDefault();
                e.stopPropagation();
            });
        });
        section.addEventListener('dragenter', function () {
            section.classList.add('drag-active');
        });
        section.addEventListener('dragover', function () {
            section.classList.add('drag-active');
        });
        section.addEventListener('dragleave', function (e) {
            if (e.target === section) {
                section.classList.remove('drag-active');
            }
        });
        section.addEventListener('drop', function (e) {
            section.classList.remove('drag-active');
            if (!e.dataTransfer || !e.dataTransfer.files
                || e.dataTransfer.files.length === 0) return;
            var dt = new DataTransfer();
            if (isMulti) {
                for (var i = 0; i < e.dataTransfer.files.length; i++) {
                    dt.items.add(e.dataTransfer.files[i]);
                }
            } else {
                dt.items.add(e.dataTransfer.files[0]);
            }
            input.files = dt.files;
            updateName();
        });
    }

    function init() {
        document.querySelectorAll('.escrow-section').forEach(setupSection);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init);
    } else {
        init();
    }
})();
