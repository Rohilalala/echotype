// Small DOM helpers shared by the settings window (main.js) and the meeting window.

// h('button', { className, onclick }, ...children): null/false children and attributes are skipped.
export function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k in el && k !== 'list') el[k] = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  el.append(...kids.flat().filter(k => k != null && k !== false));
  return el;
}

// Copies `text`; `btn` says Copied for a moment.
export async function copyText(text, btn) {
  try { await navigator.clipboard.writeText(text); }
  catch {
    const t = h('textarea', { value: text });
    document.body.append(t); t.select(); document.execCommand('copy'); t.remove();
  }
  if (!btn) return;
  const label = btn.textContent;
  btn.textContent = 'Copied';
  setTimeout(() => (btn.textContent = label), 1200);
}
