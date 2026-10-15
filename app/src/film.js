// The first-run film in the app (macOS): the stars over the desktop collapse into the ball
// (intro.js), the ball becomes the orb and the setup card grows out from behind it (intro-act3.js),
// and the score plays along (intro-score.js). The film stops while setup runs in the real card,
// then home() plays the end: the card shrinks away and the dots fall to the pill's place.
// main.js owns the card and the setup; this owns the canvases and the clock.
import { createIntro } from './intro.js';
import { createAct3, ACT3, MOVE } from './intro-act3.js';
import { createScore } from './intro-score.js';

const fadeIn = (t, a, d = 0.35) => { const k = Math.min(1, Math.max(0, (t - a) / d)); return 1 - (1 - k) ** 3; };
// Setup takes over once the orb has settled in the card's left column.
export const HOLD = ACT3.left + MOVE + 0.35;
/** When the card's content comes in (s from the start). */
export const CONTENT = ACT3.content;

/**
 * card: the setup card (.ob-card); slot: the step's orb slot; shell: the card's backdrop, which the
 * film grows and shrinks; welcome: "Welcome to Echotype"; home(): the pill's ball in page px
 * ({x, y, r}) or null; light: the desktop's appearance.
 */
export function createFilm({ card, slot, shell, welcome, home, light }) {
  const canvas = cls => {
    const c = document.createElement('canvas');
    c.className = `film-canvas ${cls}`;
    c.setAttribute('aria-hidden', 'true');
    document.body.append(c);
    return c;
  };
  const stars = canvas('film-stars'), dots = canvas('film-dots');
  const intro = createIntro(stars);
  // Where Act 3 puts things: the real card's rect and the step's orb slot, read live.
  const layout = () => {
    const c = card.getBoundingClientRect(), o = slot().getBoundingClientRect();
    return { card: { x: c.left, y: c.top, w: c.width, h: c.height, r: 18 }, orb: { x: o.left + o.width / 2, y: o.top + o.height / 2, size: Math.min(o.width, o.height) } };
  };
  const act3 = createAct3(dots, { layout, light, home: (W, H) => home() ?? { x: W / 2, y: H - 34, r: 7.5 } });
  const score = createScore();
  let raf = 0, t0 = 0, from = 0, until = 0, done = null;

  function draw(t) {
    intro.draw(t < ACT3.take ? t : -100);
    const a = act3.draw(t);
    score.motion(a.motion ?? 0);
    const R = a.rect || layout().card;
    Object.assign(shell.style, { left: `${R.x}px`, top: `${R.y}px`, width: `${R.w}px`, height: `${R.h}px`, borderRadius: `${R.r || 18}px`, opacity: a.fill ?? 0 });
    const w = fadeIn(t, ACT3.welcome, 0.5) * (1 - fadeIn(t, ACT3.left, 0.3));
    welcome.style.opacity = w;
    welcome.style.transform = `translateY(${8 * (1 - fadeIn(t, ACT3.welcome, 0.5))}px)`;
  }
  function loop(now) {
    const t = Math.min(until, from + (now - t0) / 1000);
    draw(t);
    if (t < until) raf = requestAnimationFrame(loop);
    else { raf = 0; done?.(); }
  }
  function run(a, b) {
    from = a;
    until = b;
    t0 = performance.now();
    cancelAnimationFrame(raf);
    return new Promise(r => { done = r; raf = requestAnimationFrame(loop); });
  }
  const resize = () => { intro.build?.(); act3.size(); };
  addEventListener('resize', resize);

  return {
    /** Stars to the orb settled in the card: resolves at HOLD. */
    play() {
      score.start(false);
      return run(0, HOLD);
    },
    show(on) { dots.style.opacity = on ? 1 : 0; },
    /** The end: back to the centre, the card shrinks away, the dots fall home. */
    async home() {
      act3.settle(ACT3.finish);
      dots.style.opacity = 1;
      score.home();
      await run(ACT3.finish, ACT3.landed + 0.5);
    },
    destroy() {
      cancelAnimationFrame(raf);
      removeEventListener('resize', resize);
      stars.remove(); // the score rings out by itself
      dots.remove();
    },
  };
}
