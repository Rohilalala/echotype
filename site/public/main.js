// The night sky, the app's own orb (assets/orb.js, copied from app/src/orb.js) in four places,
// the dictation demo, the line that lights up as you scroll, and the Windows waitlist form.
import { createOrb } from './assets/orb.js';

const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
const $ = s => document.querySelector(s);
const wait = ms => new Promise(r => setTimeout(r, ms));
const hash = i => { const v = Math.sin(i * 12.9898) * 43758.5453; return v - Math.floor(v); };

// ---------- the sky ----------
// Stars drift with the scroll at their own depth and a few twinkle; holding Space draws the ones
// near the hero orb in towards it. Redrawn only while something moves.
const sky = $('#sky'), sg = sky.getContext('2d');
let stars = [], pull = 0, pullTo = 0, skyRaf = 0;
function seedSky() {
  const d = Math.min(devicePixelRatio || 1, 2);
  sky.width = innerWidth * d; sky.height = innerHeight * d;
  sg.setTransform(d, 0, 0, d, 0, 0);
  const n = Math.round((innerWidth * innerHeight) / 4200);
  stars = Array.from({ length: n }, (_, i) => ({
    x: hash(i + 1) * innerWidth, y: hash(i + 7) * innerHeight * 3,
    r: 0.35 + hash(i + 3) ** 3 * 1.3, a: 0.25 + hash(i + 5) * 0.6,
    z: 0.15 + hash(i + 9) * 0.85, tw: hash(i + 11) < 0.12 ? 0.6 + hash(i + 13) * 2 : 0,
  }));
  drawSky(performance.now());
}
function drawSky(now) {
  sg.clearRect(0, 0, innerWidth, innerHeight);
  const orbBox = heroCanvas.getBoundingClientRect(), cx = innerWidth / 2, cy = orbBox.top + orbBox.height / 2;
  const h = innerHeight * 3;
  for (const s of stars) {
    let y = (s.y - scrollY * s.z * 0.35) % h; if (y < 0) y += h;
    if (y > innerHeight + 4) continue;
    let x = s.x;
    if (pull > 0.001) { const k = pull * 0.08 * s.z; x += (cx - x) * k; y += (cy - y) * k; }
    const tw = still || !s.tw ? 1 : 0.55 + 0.45 * Math.sin(now / 900 * s.tw + s.x);
    sg.globalAlpha = Math.min(1, s.a * tw * (1 + pull * 0.6));
    sg.fillStyle = '#f2f4ff';
    sg.beginPath(); sg.arc(x, y, s.r, 0, Math.PI * 2); sg.fill();
  }
  sg.globalAlpha = 1;
}
function skyLoop(now) {
  pull += (pullTo - pull) * 0.06;
  drawSky(now);
  skyRaf = !still && (document.visibilityState === 'visible') ? requestAnimationFrame(skyLoop) : 0;
}
addEventListener('resize', seedSky);
addEventListener('scroll', () => { if (still) drawSky(0); }, { passive: true });
document.addEventListener('visibilitychange', () => { if (!skyRaf && !still && document.visibilityState === 'visible') skyRaf = requestAnimationFrame(skyLoop); });

// ---------- orbs ----------
// Each orb draws on its own canvas and only while that canvas is on screen.
function orbOn(canvas, size, setup) {
  const orb = createOrb(canvas, still);
  const place = () => orb.point(canvas.clientWidth / 2, canvas.clientHeight / 2, size(canvas));
  new ResizeObserver(place).observe(canvas);
  setup?.(orb);
  new IntersectionObserver(([e]) => (e.isIntersecting ? (place(), orb.start()) : orb.stop())).observe(canvas);
  return orb;
}

// Hero: a single star bursts into the listening orb. Hold Space and it listens for real-ish:
// the level follows a speaking rhythm, then it thinks, lands on a check and goes back.
const heroCanvas = $('#heroOrb');
const hero = orbOn(heroCanvas, c => Math.min(c.clientWidth * 0.7, c.clientHeight * 0.78, 340));
if (!still) setTimeout(() => hero.set('orb'), 650); else hero.set('orb');
const speaking = t => 0.25 + 0.55 * Math.abs(Math.sin(t / 130)) * Math.abs(Math.sin(t / 410 + 1));
let held = false, heldAt = 0;
function heroIdle(now) {
  if (held) hero.level(speaking(now - heldAt));
  else if (hero.kind === 'orb') hero.level(0.05 + 0.05 * Math.sin(now / 700));
  requestAnimationFrame(heroIdle);
}
requestAnimationFrame(heroIdle);
const hint = $('#tryHint'), hintText = hint.innerHTML;
const typing = e => e.target.closest('input, textarea, select, button, a, summary');
addEventListener('keydown', e => {
  if (e.code !== 'Space' || e.repeat || typing(e)) return;
  if (heroCanvas.getBoundingClientRect().bottom < 0) return;
  e.preventDefault();
  held = true; heldAt = performance.now(); pullTo = 1;
  hero.set('orb');
  hint.classList.add('live'); hint.textContent = "Listening. Let go when you're done.";
});
addEventListener('keyup', async e => {
  if (e.code !== 'Space' || !held) return;
  held = false; pullTo = 0;
  hint.textContent = 'Cleaning up';
  hero.set('think'); await wait(900);
  hero.set('check'); hint.textContent = 'Written.'; await wait(1300);
  hero.set('orb'); hint.classList.remove('live'); hint.innerHTML = hintText;
});

// Languages: the globe shape, turning behind the greetings.
orbOn($('#langOrb'), c => c.clientWidth * 0.9, o => { o.style({ listen: 'searching' }); o.set('orb'); });
// Privacy: a calm orb, breathing.
orbOn($('#privacyOrb'), c => c.clientWidth * 0.62, o => { o.style({ listen: 'breathing' }); o.set('orb'); });
const pillOrb = orbOn($('#pillOrb'), () => 30);

// ---------- dictation demo ----------
// Each scene: what was said (segments, the ones the cleanup removes marked with why), then what
// lands in the app. Words appear at a speaking pace, the cuts are struck and labelled, then the
// clean text settles in.
const scenes = [
  {
    title: 'New Message', head: '<p><b>To</b> Arjun</p><p><b>Subject</b> Saturday</p>',
    said: [['um', 'Filler'], ['so', 'Filler'], ['before saturday I need you to grab'], ['14 footballs, no actually', 'Correction'], ['12 footballs, 2 cricket bats and 3 tennis rackets']],
    clean: 'Before Saturday, I need you to grab:\n- 12 footballs\n- 2 cricket bats\n- 3 tennis rackets',
  },
  {
    title: '# design-review', head: '<p><b>Message</b> #design-review</p>',
    said: [['hey'], ['uh', 'Filler'], ['can we push the review to thursday'], ['like', 'Filler'], ["the files aren't ready yet"]],
    clean: "Hey, can we push the review to Thursday? The files aren't ready yet.",
  },
  {
    title: 'Launch checklist', head: '<p><b>Note</b> Launch checklist</p>',
    said: [['okay so', 'Filler'], ['first update the website second email the beta list third tell the team']],
    clean: '1. Update the website\n2. Email the beta list\n3. Tell the team',
  },
];
const text = $('#demoText'), pill = $('#pill'), pillLabel = $('#pillLabel'), tabs = [...document.querySelectorAll('.tabs button')];
let sceneAt = 0, run = 0, demoVisible = false;
function showScene(i) {
  tabs.forEach((t, k) => t.setAttribute('aria-selected', String(k === i)));
  $('#winTitle').textContent = scenes[i].title;
  $('#winHead').innerHTML = scenes[i].head;
}
async function play(i) {
  const me = ++run, s = scenes[i];
  const alive = () => me === run;
  showScene(i);
  if (still) { text.textContent = s.clean; return; }
  text.classList.remove('settled'); text.innerHTML = '';
  const cuts = [];
  for (const [seg, why] of s.said) {
    const span = document.createElement('span');
    span.className = 'raw';
    if (why) { cuts.push([span, why]); }
    for (const w of seg.split(' ')) {
      const ws = document.createElement('span'); ws.className = 'w'; ws.textContent = w + ' ';
      span.append(ws);
    }
    text.append(span);
  }
  pill.classList.add('live'); pillLabel.textContent = 'Listening'; pillOrb.set('orb');
  for (const ws of text.querySelectorAll('.w')) {
    if (!alive()) return;
    ws.classList.add('on');
    pillOrb.level(0.35 + 0.5 * Math.random());
    await wait(115 + Math.random() * 90);
  }
  pillOrb.level(0);
  pill.classList.remove('live'); pillLabel.textContent = 'Cleaning up'; pillOrb.set('think');
  await wait(500); if (!alive()) return;
  for (const [span, why] of cuts) {
    span.classList.add('cut');
    const tag = document.createElement('span'); tag.className = 'tag'; tag.textContent = why;
    span.after(tag);
    await wait(260); if (!alive()) return;
  }
  await wait(900); if (!alive()) return;
  text.textContent = s.clean; text.classList.add('settled');
  pillLabel.textContent = 'Done'; pillOrb.set('check');
  await wait(2800); if (!alive()) return;
  pillOrb.set('orb'); pillLabel.textContent = 'Listening';
  if (demoVisible) { sceneAt = (i + 1) % scenes.length; play(sceneAt); }
}
tabs.forEach((t, k) => t.addEventListener('click', () => { sceneAt = k; play(k); }));
new IntersectionObserver(([e]) => {
  demoVisible = e.isIntersecting;
  if (demoVisible) play(sceneAt); else run++;
}, { threshold: 0.45 }).observe($('.window'));

// ---------- the line that lights up ----------
// Words light from the first to the last as the paragraph passes the middle of the screen.
const statement = $('#statement');
(function split(node) {
  for (const n of [...node.childNodes]) {
    if (n.nodeType === Node.TEXT_NODE) {
      const frag = document.createDocumentFragment();
      for (const part of n.textContent.split(/(\s+)/)) {
        if (!part) continue;
        if (/^\s+$/.test(part)) { frag.append(part); continue; }
        const w = document.createElement('span'); w.className = 'w'; w.textContent = part; frag.append(w);
      }
      n.replaceWith(frag);
    } else if (n.nodeName === 'KBD') {
      n.classList.add('w');
    }
  }
})(statement);
const words = [...statement.querySelectorAll('.w')];
function light() {
  const r = statement.getBoundingClientRect();
  const p = still ? 1 : Math.min(1, Math.max(0, (innerHeight * 0.82 - r.top) / (r.height + innerHeight * 0.35)));
  const n = Math.round(p * words.length);
  words.forEach((w, i) => w.classList.toggle('lit', i < n));
}
addEventListener('scroll', light, { passive: true });

// ---------- tiles ----------
// A faint constellation behind each drawing, the same every visit.
document.querySelectorAll('.art').forEach((art, k) => {
  // A short chain across the top of the drawing, clear of its text.
  const pts = Array.from({ length: 5 }, (_, i) => [8 + hash(k * 31 + i) * 84, 2 + hash(k * 17 + i + 3) * 14]).sort((a, b) => a[0] - b[0]);
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 100 100'); svg.setAttribute('preserveAspectRatio', 'none');
  svg.innerHTML = pts.slice(1).map((p, i) => `<line x1="${pts[i][0]}" y1="${pts[i][1]}" x2="${p[0]}" y2="${p[1]}" vector-effect="non-scaling-stroke"/>`).join('')
    + pts.map(([x, y], i) => `<circle cx="${x}" cy="${y}" r="${i % 3 ? 0.5 : 0.8}" opacity="${0.35 + (i % 3) * 0.2}"/>`).join('');
  art.prepend(svg);
});
const hellos = ['Hello', 'Hola', 'नमस्ते', 'Bonjour', 'こんにちは', 'Olá', 'Ciao', 'مرحبا', 'Hallo', '你好'];
const hello = $('#hello');
let hi = 0;
if (!still) setInterval(() => {
  hello.style.opacity = 0;
  setTimeout(() => { hi = (hi + 1) % hellos.length; hello.textContent = hellos[hi]; hello.style.opacity = 1; }, 400);
}, 1800);

// ---------- Windows waitlist ----------
const form = $('#waitlist'), note = $('#wlNote');
form.addEventListener('submit', async e => {
  e.preventDefault();
  const email = form.email.value.trim();
  note.className = 'form-note';
  if (!/^[^@\s]+@[^@\s.]+(\.[^@\s.]+)+$/.test(email)) { note.textContent = 'Enter a valid email address.'; note.classList.add('err'); form.email.focus(); return; }
  const btn = form.querySelector('button');
  btn.disabled = true; btn.textContent = 'Joining';
  try {
    const r = await fetch('/api/waitlist', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ email, website: form.website.value }) });
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || 'Something went wrong. Try again.');
    form.replaceWith(Object.assign(document.createElement('p'), { className: 'form-note ok', textContent: "You're on the list." }));
    note.classList.add('ok'); note.textContent = "We'll email you once, when Echotype for Windows is ready.";
  } catch (err) {
    note.textContent = err.message; note.classList.add('err');
    btn.disabled = false; btn.textContent = 'Join waitlist';
  }
});

seedSky();
light();
if (!still) skyRaf = requestAnimationFrame(skyLoop);
