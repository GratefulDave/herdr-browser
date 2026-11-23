(query, step) => {
    const key = Symbol.for('herdr-browser.find-state');
    const allName = 'herdr-browser-find';
    const activeName = 'herdr-browser-find-active';
    const previous = window[key];
    if (!query) {
        if (globalThis.CSS && CSS.highlights) {
            CSS.highlights.delete(allName);
            CSS.highlights.delete(activeName);
        }
        if (previous && previous.sheet) {
            document.adoptedStyleSheets = document.adoptedStyleSheets.filter(sheet => sheet !== previous.sheet);
        }
        delete window[key];
        return { current: 0, total: 0 };
    }
    if (!globalThis.CSS || !CSS.highlights || !globalThis.Highlight || !globalThis.CSSStyleSheet || !('adoptedStyleSheets' in document)) {
        throw new Error('Find highlighting is unavailable in this page');
    }
    let sheet = previous && previous.sheet;
    if (!sheet) {
        sheet = new CSSStyleSheet();
        sheet.replaceSync('::highlight(herdr-browser-find) { background-color: #ffe066; color: #111; } ::highlight(herdr-browser-find-active) { background-color: #ff9632; color: #111; }');
        document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
    }
    const ranges = [];
    const pattern = new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'giu');
    if (document.body) {
        const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
        for (let node = walker.nextNode(); node; node = walker.nextNode()) {
            const parent = node.parentElement;
            if (!parent || parent.closest('script, style, noscript, template, [hidden], [inert]')) continue;
            let visible = true;
            for (let element = parent; element; element = element.parentElement) {
                const style = getComputedStyle(element);
                if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse' || Number(style.opacity) === 0 || style.contentVisibility === 'hidden') {
                    visible = false;
                    break;
                }
            }
            if (!visible) continue;
            pattern.lastIndex = 0;
            let match;
            while ((match = pattern.exec(node.data)) !== null) {
                const range = document.createRange();
                range.setStart(node, match.index);
                range.setEnd(node, match.index + match[0].length);
                if (range.getClientRects().length) ranges.push(range);
            }
        }
    }
    const total = ranges.length;
    let index = 0;
    if (total && step && previous && previous.query === query && previous.total) {
        index = ((previous.index + step) % total + total) % total;
    }
    const all = new Highlight();
    for (const range of ranges) all.add(range);
    const active = new Highlight();
    if (total) active.add(ranges[index]);
    active.priority = 1;
    CSS.highlights.set(allName, all);
    CSS.highlights.set(activeName, active);
    window[key] = { query, index, total, sheet };
    if (total) {
        const range = ranges[index];
        range.startContainer.parentElement.scrollIntoView({ block: 'center', inline: 'nearest' });
        const rect = range.getBoundingClientRect();
        if (rect.top < 0 || rect.bottom > innerHeight) {
            window.scrollBy({ top: rect.top - innerHeight / 2, behavior: 'instant' });
        }
    }
    return { current: total ? index + 1 : 0, total };
}
