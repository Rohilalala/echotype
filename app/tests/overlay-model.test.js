import { test } from 'node:test';
import assert from 'node:assert/strict';
import { step, angle, clockText, face, cardReason } from '../src/overlay-model.js';

// Runs the needle towards `target` from `from` at `fps` frames a second; returns each frame's position.
function run(from, target, ms, still, fps = 60) {
  let n = { x: from, v: 0 };
  const xs = [];
  for (let t = 0; t < ms; t += 1000 / fps) xs.push((n = step(n, target, 1 / fps, still)).x);
  return xs;
}
const msUntil = (xs, ok, fps = 60) => Math.round((xs.findIndex(ok) + 1) * 1000 / fps); // frame i ends at (i + 1) / fps

// 120 Hz, 60 Hz, 30 Hz and the 50 ms cap overlay.js puts on a stalled frame: the same ballistics.
for (const fps of [120, 60, 30, 20]) {
  test(`fast attack, at most 3% overshoot (${fps} fps)`, () => {
    const xs = run(0, 1, 500, false, fps);
    const t90 = msUntil(xs, x => x >= 0.9, fps);
    assert.ok(t90 >= 8 && t90 <= 60, `90% after ${t90} ms`);
    assert.ok(Math.max(...xs) <= 1.03, `peak ${Math.max(...xs)}`);
    assert.ok(Math.abs(xs.at(-1) - 1) < 0.005, 'settles on the level');
  });

  test(`slow release, never below zero (${fps} fps)`, () => {
    const xs = run(1, 0, 1000, false, fps);
    const t10 = msUntil(xs, x => x <= 0.1, fps);
    assert.ok(t10 >= 200 && t10 <= 400, `back to 10% after ${t10} ms`);
    assert.ok(xs.every(x => x >= 0));
  });
}

test('mid-scale and turning around mid-flight', () => {
  assert.deepEqual(step({ x: 0.5, v: 0 }, 0.5, 0), { x: 0.5, v: 0 }, 'no time, no movement');
  const xs = run(0, 0.5, 400);
  assert.ok(Math.max(...xs) <= 0.5 * 1.03 && Math.abs(xs.at(-1) - 0.5) < 0.005, 'settles on a mid level');
  // Rising fast towards 1, the voice stops: the needle turns and falls without going below zero.
  let n = { x: 0, v: 0 };
  for (let i = 0; i < 2; i++) n = step(n, 1, 1 / 60);
  const fall = [];
  for (let i = 0; i < 60; i++) fall.push((n = step(n, 0, 1 / 60)).x);
  assert.ok(Math.max(...fall) < 1 && fall.every(x => x >= 0) && fall.at(-1) < 0.1);
});

test('reduced motion still moves, with the same timing and no overshoot', () => {
  const up = run(0, 1, 500, true), down = run(1, 0, 1000, true);
  const t90 = msUntil(up, x => x >= 0.9), t10 = msUntil(down, x => x <= 0.1);
  assert.ok(t90 >= 16 && t90 <= 50, `90% after ${t90} ms`);
  assert.ok(t10 >= 200 && t10 <= 400, `10% after ${t10} ms`);
  assert.ok(Math.max(...up) <= 1, 'no overshoot');
});

test('the needle spans the dial and stops just past full scale', () => {
  assert.equal(angle(0), -48);
  assert.equal(angle(1), 48);
  assert.ok(angle(2) < 51);
});

test('clock text', () => {
  for (const [ms, text] of [[0, '0:00'], [59999, '0:59'], [3599000, '59:59'], [3600000, '1:00:00'], [5025000, '1:23:45'], [-5000, '0:00']]) {
    assert.equal(clockText(ms), text);
  }
});

test('a dictation state outranks the meeting, which outranks the idle hint', () => {
  const meeting = 1_700_000_000_000;
  assert.deepEqual(face('recording', { active: true, meeting }), { active: true, meeting: false, expand: false });
  assert.deepEqual(face('done', { active: true, meeting, expand: true }), { active: true, meeting: false, expand: false });
  assert.deepEqual(face('done', { active: false, meeting }), { active: false, meeting: true, expand: false }, 'the notice ended: back to the meeting');
  assert.deepEqual(face('idle', { active: false, meeting, expand: true }), { active: false, meeting: true, expand: false }, 'no hint over a meeting');
  assert.deepEqual(face('idle', { active: false, meeting: null, expand: true }), { active: false, meeting: false, expand: true });
  assert.deepEqual(face('idle', { active: false, meeting: null }), { active: false, meeting: false, expand: false });
  assert.deepEqual(face('transcribing', null), { active: true, meeting: false, expand: false }, 'browser preview: the state decides');
});

test('the card says why in one short line', () => {
  assert.equal(cardReason({ warning: 'Nowhere to paste' }), 'Nowhere to paste');
  assert.equal(cardReason({}), 'Nowhere to paste');
  assert.equal(cardReason({ warning: 'Accessibility is off, so Echotype copied instead of pasting', action: 'accessibility' }), 'Accessibility is off, copied instead');
  assert.equal(cardReason({ warning: 'Secure input is on. The text is on your clipboard' }), 'Paste was blocked, copied instead');
});
