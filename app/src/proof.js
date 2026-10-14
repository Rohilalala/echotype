// History as galley proofs: which words of the raw transcript cleanup took out, by a word-level
// longest common subsequence against the pasted text. `node app/src/proof.js` runs the checks below.
const norm = w => w.toLowerCase().replace(/[^\p{L}\p{N}']/gu, '');

// [{ w, cut }] for each word of `raw`, in order.
export function proof(raw, clean) {
  const a = raw.split(/\s+/).filter(Boolean), A = a.map(norm), B = clean.split(/\s+/).map(norm).filter(Boolean);
  const none = a.map(w => ({ w, cut: false }));
  const n = A.length, m = B.length;
  if (!n || !m || n * m > 250000) return none; // ponytail: O(n·m) table; past ~500×500 words nothing is struck
  const L = Array.from({ length: n + 1 }, () => new Uint16Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) L[i][j] = A[i] && A[i] === B[j] ? L[i + 1][j + 1] + 1 : Math.max(L[i + 1][j], L[i][j + 1]);
  const kept = new Array(n).fill(false);
  for (let i = 0, j = 0; i < n && j < m;) {
    if (A[i] && A[i] === B[j]) { kept[i] = true; i++; j++; } else if (L[i + 1][j] >= L[i][j + 1]) i++; else j++;
  }
  // A translation or a rewrite keeps few of the pasted words: the texts differ, nothing was "removed".
  if (L[0][0] < m / 2) return none;
  return a.map((w, i) => ({ w, cut: !kept[i] && !!A[i] }));
}

if (typeof window === 'undefined') {
  const { strict: assert } = await import('node:assert');
  const cuts = (r, c) => proof(r, c).filter(x => x.cut).map(x => x.w).join(' ');
  assert.equal(cuts('um so the the build is green now, uh, can you cut it', 'The build is green now. Can you cut it?'), 'um so the uh,');
  assert.equal(cuts('groceries eggs milk and bread', 'Groceries:\n- Eggs\n- Milk\n- Bread'), 'and');
  assert.equal(cuts('kal ki meeting mein we decided ki launch ko push karenge', "In yesterday's meeting we decided to push the launch to Thursday."), '', 'a translation strikes nothing');
  assert.equal(cuts('same words here', 'Same words here.'), '');
  assert.equal(cuts('', 'anything'), '');
  console.log('proof.js: ok');
}
