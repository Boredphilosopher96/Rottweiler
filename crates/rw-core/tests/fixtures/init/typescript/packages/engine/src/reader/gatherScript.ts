/**
 * Runs inside the page (via executeJavaScript) to collect block candidates for
 * the reader scorer. Returns { title, byline, blocks: ReaderBlock[] }. Kept as a
 * self-contained string so it has no engine dependencies and can be injected as-is.
 */
export const READER_GATHER_SCRIPT = `(() => {
  const SEL = 'p,article,section,main,blockquote,pre,h1,h2,h3,h4,li,figure,td';
  const depthOf = (el) => { let d = 0; let n = el; while (n && n.parentElement) { d++; n = n.parentElement; } return d; };
  const nodes = Array.prototype.slice.call(document.querySelectorAll(SEL));
  const blocks = nodes.map((el) => {
    const text = (el.innerText || el.textContent || '').trim();
    const links = Array.prototype.slice.call(el.querySelectorAll('a'));
    const linkText = links.map((a) => a.innerText || a.textContent || '').join(' ');
    return { tag: el.tagName.toLowerCase(), text, linkText, depth: depthOf(el), role: el.getAttribute('role') || undefined };
  }).filter((b) => b.text.length > 0);
  const meta = document.querySelector('meta[name="author"]');
  const bylineEl = document.querySelector('[rel="author"], .byline, .author, [itemprop="author"]');
  const byline = (meta && meta.content) || (bylineEl && bylineEl.innerText) || null;
  return { title: document.title || '', byline: byline || null, blocks: blocks };
})()`
