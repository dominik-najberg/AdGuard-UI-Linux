// ==UserScript==
// @name         AdGuard UI install links
// @namespace    https://github.com/dominik-najberg/AdGuard-UI-Linux
// @version      1.0.0
// @description  Sends clicks on userscript install links to AdGuard UI, which asks before adding them to AdGuard.
// @homepage     https://github.com/dominik-najberg/AdGuard-UI-Linux#installing-userscripts-from-the-browser
// @downloadURL  https://raw.githubusercontent.com/dominik-najberg/AdGuard-UI-Linux/main/data/userscripts/adguard-ui-install-links.user.js
// @updateURL    https://raw.githubusercontent.com/dominik-najberg/AdGuard-UI-Linux/main/data/userscripts/adguard-ui-install-links.user.js
// @match        https://greasyfork.org/*
// @match        https://sleazyfork.org/*
// @match        https://openuserjs.org/*
// @match        https://github.com/*
// @match        https://gist.github.com/*
// @grant        none
// @run-at       document-start
// ==/UserScript==

// AdGuard CLI injects userscripts into HTML pages only, and a .user.js URL is
// served as JavaScript — so the click has to be caught here, on the page that
// links to the script, before the browser navigates to it. The link is handed
// to AdGuard UI, which shows the URL in full and installs nothing without a
// yes. A modified or middle click is left alone, so opening the script in a
// tab to read it first still works.
(function () {
    'use strict';

    window.addEventListener('click', function (event) {
        if (event.button !== 0 || event.ctrlKey || event.shiftKey || event.altKey || event.metaKey) {
            return;
        }
        const link = event.target instanceof Element ? event.target.closest('a[href]') : null;
        if (!link) {
            return;
        }
        let url;
        try {
            url = new URL(link.href, document.baseURI);
        } catch (_) {
            return;
        }
        if (!/^https?:$/.test(url.protocol) || !url.pathname.endsWith('.user.js')) {
            return;
        }
        // Before the site's own handler, which on Greasy Fork opens a page
        // about installing a userscript manager.
        event.preventDefault();
        event.stopImmediatePropagation();
        window.location.href = 'adguard-ui://install-userscript?url=' + encodeURIComponent(url.href);
    }, true);
})();
