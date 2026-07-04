// sticky horizontal scrollbar for the catalog tree.
//
// The tree lives in a `.tree-scroll` viewport with bounded
// height; its native horizontal scrollbar is hidden via CSS. A
// fixed-position `.tree-hscroll-proxy` pinned to the viewport
// bottom is what the user actually drags — this script syncs
// `scrollLeft` between the two and tracks DOM changes so the
// proxy's spacer width stays equal to the tree's `scrollWidth`.
//
// Tree-scroll and proxy mount via streaming Suspense, so they
// might not be in the DOM when this script first runs. We poll
// for a few seconds and re-init on `load` as a safety net.

(function () {
    "use strict";

    function pair(tree, proxy) {
        if (tree.dataset.hscrollPaired === "1") return;
        var inner = proxy.querySelector(".tree-hscroll-proxy-inner");
        if (!inner) return;
        tree.dataset.hscrollPaired = "1";

        var syncing = false;
        tree.addEventListener("scroll", function () {
            if (syncing) return;
            syncing = true;
            proxy.scrollLeft = tree.scrollLeft;
            syncing = false;
        }, { passive: true });
        proxy.addEventListener("scroll", function () {
            if (syncing) return;
            syncing = true;
            tree.scrollLeft = proxy.scrollLeft;
            syncing = false;
        }, { passive: true });

        function refresh() {
            // Align the proxy with the tree-scroll's bounding box so
            // its drag ratio matches 1:1 — without this the proxy
            // spans the full window width, the scroll ratio gets
            // scaled, and the user can't reach the right edge of the
            // tree content.
            var rect = tree.getBoundingClientRect();
            proxy.style.left = rect.left + "px";
            proxy.style.width = rect.width + "px";
            inner.style.width = tree.scrollWidth + "px";
            proxy.style.display =
                tree.scrollWidth > tree.clientWidth + 1 ? "block" : "none";
        }
        refresh();
        // Window resizes / viewport changes shift the tree-scroll's
        // left edge — re-align the proxy then too.
        window.addEventListener("resize", refresh, { passive: true });

        if (typeof ResizeObserver !== "undefined") {
            new ResizeObserver(refresh).observe(tree);
        }
        if (typeof MutationObserver !== "undefined") {
            new MutationObserver(refresh).observe(tree, {
                childList: true,
                subtree: true,
                attributes: true,
                attributeFilter: ["open", "style"],
            });
        }
        // Keep refreshing while the lazy tree mounts deeper levels.
        var ticks = 0;
        var ticker = setInterval(function () {
            refresh();
            if (++ticks > 50) clearInterval(ticker);
        }, 200);
    }

    function tryInit() {
        var trees = document.querySelectorAll(".tree-scroll");
        if (trees.length === 0) return false;
        var proxy = document.querySelector(".tree-hscroll-proxy");
        if (!proxy) return false;
        trees.forEach(function (t) { pair(t, proxy); });
        return true;
    }

    // Streaming Suspense may not have injected `.tree-scroll` yet
    // when this defer script fires. Poll for up to ~5s, then stop.
    function startPolling() {
        if (tryInit()) return;
        var attempts = 0;
        var t = setInterval(function () {
            if (tryInit() || ++attempts > 50) clearInterval(t);
        }, 100);
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", startPolling);
    } else {
        startPolling();
    }
    window.addEventListener("load", tryInit);
})();
