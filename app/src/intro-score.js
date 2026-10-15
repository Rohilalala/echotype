// The first-launch film's score, synthesised with Web Audio: no audio files to ship or license,
// and every cue lands on the film's own timeline (intro.js for the stars, ACT3 for the rest).
import { ACT3 as A } from './intro-act3.js';

const COLLAPSE = 4.0, FORMED = 6.9, END = 19.5;
const hz = m => 440 * 2 ** ((m - 69) / 12);

// A seeded random, so the stars twinkle the same way every time.
function rng(seed) { return () => ((seed = (seed * 1664525 + 1013904223) >>> 0) / 2 ** 32); }

// A long, dark hall: decaying stereo noise for the convolver.
function hall(ctx, secs = 5.5, decay = 2.8) {
  const len = Math.floor(ctx.sampleRate * secs), buf = ctx.createBuffer(2, len, ctx.sampleRate);
  const r = rng(7);
  for (let c = 0; c < 2; c++) {
    const d = buf.getChannelData(c);
    for (let i = 0; i < len; i++) d[i] = (r() * 2 - 1) * (1 - i / len) ** decay;
  }
  return buf;
}

/// Schedules the whole score into `dest`, with film time 0 at context time `t0`.
export function scoreInto(ctx, dest, t0) {
  const comp = ctx.createDynamicsCompressor();
  comp.threshold.value = -14; comp.ratio.value = 3;
  comp.connect(dest);
  const verb = ctx.createConvolver(); verb.buffer = hall(ctx);
  const wet = ctx.createGain(); wet.gain.value = 0.7;
  verb.connect(wet).connect(comp);
  const noiseBuf = ctx.createBuffer(1, ctx.sampleRate * 2, ctx.sampleRate);
  { const d = noiseBuf.getChannelData(0), r = rng(3); for (let i = 0; i < d.length; i++) d[i] = r() * 2 - 1; }

  const at = s => t0 + s;
  // A voice's tail: gain → pan → dry + reverb send.
  function voice(start, pan = 0, send = 0.5, panTo = pan, dur = 0) {
    const g = ctx.createGain(); g.gain.value = 0;
    const p = ctx.createStereoPanner(); p.pan.setValueAtTime(pan, at(start));
    if (panTo !== pan) p.pan.linearRampToValueAtTime(panTo, at(start + dur));
    const s = ctx.createGain(); s.gain.value = send;
    g.connect(p); p.connect(comp); p.connect(s).connect(verb);
    return g;
  }
  function env(g, start, attack, dur, peak, release = dur - attack) {
    g.gain.setValueAtTime(0, at(start));
    g.gain.linearRampToValueAtTime(peak, at(start + attack));
    g.gain.exponentialRampToValueAtTime(0.0001, at(start + attack + release));
  }
  function tone(m, start, dur, { type = 'sine', peak = 0.05, attack = 0.01, pan = 0, send = 0.6, glide, detune = 0 } = {}) {
    const o = ctx.createOscillator(); o.type = type; o.detune.value = detune;
    o.frequency.setValueAtTime(hz(m), at(start));
    if (glide !== undefined) o.frequency.exponentialRampToValueAtTime(hz(glide), at(start + dur));
    const g = voice(start, pan, send); env(g, start, attack, dur, peak);
    o.connect(g); o.start(at(start)); o.stop(at(start + dur + 0.05));
  }
  // A bell: a sine with a quieter inharmonic partial.
  function bell(m, start, peak = 0.04, pan = 0, dur = 3.5) {
    tone(m, start, dur, { peak, pan, send: 0.9 });
    tone(m + 19.02, start, dur * 0.4, { peak: peak * 0.35, pan, send: 0.9 });
  }
  // Filtered noise swept between two frequencies: wind, whooshes, risers.
  function air(start, dur, { from = 400, to = 2000, q = 1.2, peak = 0.08, attack, pan = 0, panTo = pan, send = 0.5 } = {}) {
    const n = ctx.createBufferSource(); n.buffer = noiseBuf; n.loop = true;
    const f = ctx.createBiquadFilter(); f.type = 'bandpass'; f.Q.value = q;
    f.frequency.setValueAtTime(from, at(start)); f.frequency.exponentialRampToValueAtTime(to, at(start + dur));
    const g = voice(start, pan, send, panTo, dur); env(g, start, attack ?? dur * 0.6, dur, peak);
    n.connect(f).connect(g); n.start(at(start)); n.stop(at(start + dur + 0.05));
  }
  function thump(start, peak = 0.22) { tone(40, start, 0.45, { peak, attack: 0.005, send: 0.25, glide: 28 }); }

  // The bed: an A Lydian pad under the whole film, breathing through a slow filter.
  const padOut = ctx.createGain(); padOut.gain.value = 0;
  const lp = ctx.createBiquadFilter(); lp.type = 'lowpass'; lp.frequency.value = 900; lp.Q.value = 0.5;
  const lfo = ctx.createOscillator(); lfo.frequency.value = 0.07;
  const lfoAmt = ctx.createGain(); lfoAmt.gain.value = 350; lfo.connect(lfoAmt).connect(lp.frequency);
  const padSend = ctx.createGain(); padSend.gain.value = 0.8;
  lp.connect(padOut); padOut.connect(comp); padOut.connect(padSend).connect(verb);
  const pad = [];
  for (const [m, d] of [[33, 0], [45, -6], [52, 5], [56, -4], [59, 7], [63, -8]]) {
    const o = ctx.createOscillator(); o.type = m < 40 ? 'sine' : 'triangle';
    o.frequency.value = hz(m); o.detune.value = d;
    o.connect(lp); o.start(at(0)); o.stop(at(END)); pad.push(o);
  }
  lfo.start(at(0)); lfo.stop(at(END));
  const pg = padOut.gain;
  pg.setValueAtTime(0, at(0));
  pg.linearRampToValueAtTime(0.06, at(3.2));        // in with the stars
  pg.linearRampToValueAtTime(0.1, at(FORMED));      // swells through the collapse
  pg.linearRampToValueAtTime(0.055, at(A.form));
  pg.linearRampToValueAtTime(0.085, at(A.welcome + 0.6));
  pg.linearRampToValueAtTime(0.05, at(A.finish));
  pg.linearRampToValueAtTime(0.06, at(A.landed));
  pg.linearRampToValueAtTime(0, at(END - 0.1));
  lp.frequency.setValueAtTime(700, at(0));
  lp.frequency.linearRampToValueAtTime(2600, at(FORMED));
  lp.frequency.linearRampToValueAtTime(1100, at(A.form));
  lp.frequency.linearRampToValueAtTime(1800, at(A.welcome + 0.6));

  // Act 1, the stars: cosmic wind and far-off twinkles.
  air(0.2, COLLAPSE + 0.6, { from: 180, to: 420, q: 0.7, peak: 0.035, attack: 2.5, send: 0.9 });
  const r = rng(11), high = [81, 83, 85, 88, 90, 93, 95];
  for (let i = 0; i < 16; i++) bell(high[Math.floor(r() * high.length)], 0.6 + r() * 3.3, 0.012 + r() * 0.012, r() * 1.6 - 0.8, 2.5);

  // Act 2, the collapse: a riser that pulls everything in, then a bloom as the ball forms.
  air(COLLAPSE, FORMED - COLLAPSE, { from: 220, to: 5200, q: 2.2, peak: 0.07, attack: FORMED - COLLAPSE - 0.05, release: 0.15, send: 0.6 });
  tone(57, COLLAPSE + 0.4, FORMED - COLLAPSE - 0.4, { peak: 0.018, attack: 2.2, glide: 69, send: 0.7 });
  for (const m of [57, 61, 64, 68, 71]) tone(m, FORMED, 4.2, { type: 'triangle', peak: 0.03, attack: 0.02, send: 0.85 });
  bell(81, FORMED, 0.05); bell(88, FORMED + 0.06, 0.03, 0.3);

  // Act 3: the hop, the landing, the twirl.
  air(A.hop - 0.3, 0.55, { from: 500, to: 2400, peak: 0.05, attack: 0.4, pan: -0.15, panTo: 0.15 });
  thump(A.land);
  [69, 73, 76, 80, 81, 85].forEach((m, i) => bell(m, A.twirl + i * 0.11, 0.022, -0.7 + i * 0.28, 1.6));

  // The orb forms and the window grows from behind it.
  bell(76, A.form, 0.03); bell(83, A.form + 0.05, 0.02, 0.2);
  air(A.card, A.carded - A.card, { from: 300, to: 1400, q: 0.8, peak: 0.04, send: 0.8 });

  // Welcome to Echotype.
  for (const m of [45, 57, 64, 69, 71, 73, 76]) tone(m, A.welcome, 4.5, { type: 'triangle', peak: m < 50 ? 0.04 : 0.022, attack: 0.25, send: 0.85 });
  bell(88, A.welcome + 0.1, 0.035);

  // The dots stream to the left, setup appears, they stream back.
  air(A.left - 0.05, 0.9, { from: 900, to: 300, peak: 0.055, attack: 0.25, pan: 0, panTo: -0.75 });
  bell(93, A.content, 0.014, -0.2, 1.2);
  air(A.finish - 0.05, 0.85, { from: 300, to: 1100, peak: 0.05, attack: 0.4, pan: -0.75, panTo: 0 });
  tone(76, A.shrink, A.shrunk - A.shrink + 0.3, { peak: 0.02, attack: 0.15, glide: 64, send: 0.8 });

  // The fall: a cascade like rain on glass, then home.
  const fall = A.landed - A.fall, drops = [93, 90, 88, 85, 83, 81, 78, 76, 73, 71, 69, 66];
  drops.forEach((m, i) => bell(m, A.fall + fall * (i / drops.length) ** 0.7, 0.016, (r() - 0.5) * 0.6, 1.4));
  thump(A.landed, 0.16);
  for (const m of [45, 52, 57, 64]) tone(m, A.landed, END - A.landed - 0.2, { peak: 0.02, attack: 0.3, send: 0.85 });
  bell(81, A.landed + 0.02, 0.03);
}

/// Live playback for the film: start() at film time 0; stop() fades out.
export function createScore() {
  let ctx, out;
  return {
    start() {
      ctx ??= new AudioContext();
      ctx.resume();
      this.stop();
      out = ctx.createGain(); out.connect(ctx.destination);
      scoreInto(ctx, out, ctx.currentTime + 0.03);
    },
    stop() {
      if (!out) return;
      const o = out; out = null;
      o.gain.setTargetAtTime(0, ctx.currentTime, 0.08);
      setTimeout(() => o.disconnect(), 600);
    },
  };
}
