// The first-launch film's score, synthesised with Web Audio: no audio files to ship or license.
// Four chimes, on beats the film makes deterministic: the ball forms, it twirls, "Welcome to
// Echotype", and the dots settle home. Under them a pad and a shimmer that follows the dots' actual
// speed (fed every frame through motion()). After the welcome the music fades to almost nothing:
// setup asks the user to speak, so it stays out of the way.
import { ACT3 as A, MOVE, FALL, FALL_LAG } from './intro-act3.js';

const COLLAPSE = 4.0, FORMED = 6.9;
const hz = m => 440 * 2 ** ((m - 69) / 12);
function rng(seed) { return () => ((seed = (seed * 1664525 + 1013904223) >>> 0) / 2 ** 32); }

// A soft, wide hall for the reverb: smooth decaying noise, gently low-passed by averaging.
function hall(ctx, secs = 4.5) {
  const len = Math.floor(ctx.sampleRate * secs), buf = ctx.createBuffer(2, len, ctx.sampleRate), r = rng(7);
  for (let c = 0; c < 2; c++) {
    const d = buf.getChannelData(c);
    let lp = 0;
    for (let i = 0; i < len; i++) { lp += ((r() * 2 - 1) - lp) * 0.35; d[i] = lp * Math.exp((-4.2 * i) / len); }
  }
  return buf;
}

/// Schedules the score into `dest` with film time 0 at context time `t0`: the `intro` (stars to the
/// welcome) and/or the `home` cadence. Returns motion(speed, when): the dots' mean speed (px/s) at
/// context time `when`.
export function scoreInto(ctx, dest, t0, { intro = true, home = true } = {}) {
  const at = s => t0 + s;
  const master = ctx.createGain(); master.gain.value = 1.0;
  const comp = ctx.createDynamicsCompressor(); comp.threshold.value = -8; comp.ratio.value = 3;   // catches overs only, so chimes stay above the pad
  master.connect(comp).connect(dest);
  // The home chime rings on like a bell; the whole score eases to silence by the film's end.
  master.gain.setValueAtTime(1, at(A.end - 1.2));
  master.gain.setTargetAtTime(0, at(A.end - 1.2), 0.3);
  master.gain.setValueAtTime(0, at(A.end));
  const verb = ctx.createConvolver(); verb.buffer = hall(ctx);
  verb.connect(master);

  function out(pan, send) {
    const p = ctx.createStereoPanner(); p.pan.value = pan;
    const s = ctx.createGain(); s.gain.value = send;
    p.connect(master); p.connect(s).connect(verb);
    return p;
  }

  // A chime: a sine with a faint octave, soft attack, long ring. Major pentatonic only.
  function chime(m, s, peak = 0.05, pan = 0, ring = 2.8) {
    const o = out(pan, 0.7);
    for (const [mult, amp] of [[1, 1], [2, 0.12]]) {
      const osc = ctx.createOscillator(); osc.frequency.value = hz(m) * mult;
      const g = ctx.createGain();
      g.gain.setValueAtTime(0, at(s));
      g.gain.linearRampToValueAtTime(peak * amp, at(s + 0.012));
      g.gain.setTargetAtTime(0, at(s + 0.012), ring / 5);
      osc.connect(g).connect(o); osc.start(at(s)); osc.stop(at(s + ring + 0.2));
    }
  }
  // A held, swelling tone for chords.
  function swell(m, s, dur, peak, attack = 0.6, pan = 0) {
    const osc = ctx.createOscillator(); osc.frequency.value = hz(m);
    const g = ctx.createGain(), o = out(pan, 0.6);
    g.gain.setValueAtTime(0, at(s));
    g.gain.linearRampToValueAtTime(peak, at(s + attack));
    g.gain.setTargetAtTime(0, at(s + attack), (dur - attack) / 4);
    osc.connect(g).connect(o); osc.start(at(s)); osc.stop(at(s + dur + 0.5));
  }

  let shimmer = null;
  if (intro) {
    // The bed: A major add9 in sines, slightly detuned pairs for warmth, under the whole film.
    const pad = ctx.createGain(); pad.gain.value = 0;
    const lp = ctx.createBiquadFilter(); lp.type = 'lowpass'; lp.frequency.value = 700; lp.Q.value = 0.3;
    lp.connect(pad).connect(out(0, 0.5));
    for (const m of [45, 52, 59, 61, 64]) for (const d of [-4, 4]) {
      const o = ctx.createOscillator(); o.frequency.value = hz(m); o.detune.value = d;
      const g = ctx.createGain(); g.gain.value = m === 45 ? 0.5 : 0.3;
      o.connect(g).connect(lp); o.start(at(0)); o.stop(at(A.end));
    }
    const pg = pad.gain;
    pg.setValueAtTime(0, at(0));
    pg.linearRampToValueAtTime(0.05, at(3.5));
    pg.linearRampToValueAtTime(0.08, at(FORMED));
    pg.linearRampToValueAtTime(0.06, at(A.welcome));
    pg.linearRampToValueAtTime(0.004, at(A.left + MOVE));   // nearly shut for setup
    pg.setValueAtTime(0.004, at(A.end - 1));
    pg.linearRampToValueAtTime(0, at(A.end - 0.2));
    lp.frequency.setValueAtTime(500, at(0));
    lp.frequency.linearRampToValueAtTime(1500, at(FORMED));
    lp.frequency.linearRampToValueAtTime(900, at(A.form));

    // The shimmer: high sines whose level follows the dots' speed.
    shimmer = ctx.createGain(); shimmer.gain.value = 0;
    shimmer.connect(out(0, 0.9));
    for (const [m, d] of [[76, -6], [81, 5], [83, -3], [85, 7]]) {
      const o = ctx.createOscillator(); o.frequency.value = hz(m); o.detune.value = d;
      const g = ctx.createGain(); g.gain.value = 0.25;
      const trem = ctx.createOscillator(); trem.frequency.value = 3 + d / 4;
      const tg = ctx.createGain(); tg.gain.value = 0.12; trem.connect(tg).connect(g.gain);
      o.connect(g).connect(shimmer); o.start(at(0)); trem.start(at(0)); o.stop(at(A.end)); trem.stop(at(A.end));
    }
    // The stars' collapse has no dots of Act 3 to follow, so its shimmer is drawn by hand.
    shimmer.gain.setValueAtTime(0, at(COLLAPSE));
    shimmer.gain.linearRampToValueAtTime(0.05, at(FORMED - 0.3));
    shimmer.gain.linearRampToValueAtTime(0, at(FORMED + 0.4));
    shimmer.gain.setTargetAtTime(0, at(A.welcome), 0.6);   // and silent for setup

    // The ball forms: one chime.
    swell(45, FORMED - 0.1, 3.5, 0.05, 0.15);
    chime(81, FORMED, 0.1);

    // The twirl: a rolled arpeggio, panned with the turn.
    [69, 71, 73, 76, 78, 81].forEach((m, i) => chime(m + 12, A.twirl + i * 0.13, 0.04, -0.6 + i * 0.24, 2));

    // The window grows and says welcome: the last chime of the film proper.
    for (const m of [57, 64, 69, 73, 76]) swell(m, A.card, 4.5, 0.018, 0.8);
    chime(85, A.welcome, 0.09); chime(73, A.welcome, 0.04);
  }

  // Home: a cadence rolling down to the tonic (E, C#, A) as the last dots settle into the ball,
  // with a soft A major chord blooming under the final note.
  if (home) {
    const h = A.fall + FALL + FALL_LAG;
    chime(88, h - 0.3, 0.05, 0.15); chime(85, h - 0.15, 0.06, -0.1); chime(81, h, 0.09, 0, 3.5);
    for (const m of [45, 57, 61, 64, 69]) swell(m, h - 0.05, 3.2, m === 45 ? 0.04 : 0.022, 0.25);
  }

  return {
    motion(v, when) {
      // The orb idles at ~25 px/s; ~300 px/s is a full swell.
      if (shimmer && when >= at(A.take) && when < at(A.welcome)) shimmer.gain.setTargetAtTime(0.06 * Math.min(1, Math.max(0, v - 30) / 270), when, 0.08);
    },
  };
}

/// Live playback for the film: start() at film time 0 (with the home cadence unless the film
/// pauses for setup), home() when the film resumes at ACT3.finish, motion(speed) every frame, stop().
export function createScore() {
  let ctx, bus, score;
  const play = (t0, parts) => {
    ctx ??= new AudioContext();
    ctx.resume();
    if (bus) { const b = bus; b.gain.setTargetAtTime(0, ctx.currentTime, 0.08); setTimeout(() => b.disconnect(), 600); }
    bus = ctx.createGain(); bus.connect(ctx.destination);
    score = scoreInto(ctx, bus, ctx.currentTime + 0.03 - t0, parts);
  };
  return {
    start(home = true) { play(0, { home }); },
    home() { play(A.finish, { intro: false }); },
    motion(v) { if (score) score.motion(v, ctx.currentTime); },
    stop() {
      if (!bus) return;
      const b = bus; bus = score = null;
      b.gain.setTargetAtTime(0, ctx.currentTime, 0.08);
      setTimeout(() => b.disconnect(), 600);
    },
  };
}
