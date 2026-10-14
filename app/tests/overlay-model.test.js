import { test } from 'node:test';
import assert from 'node:assert/strict';
import { step, angle } from '../src/needle.js';

// Runs the needle at 60 fps towards `target` from `from`; returns each frame's position.
function run(from, target, ms, still) {
  let n = { x: from, v: 0 };
  const xs = [];
  for (let t = 0; t < ms; t += 1000 / 60) xs.push((n = step(n, target, 1 / 60, still)).x);
  return xs;
}
const msUntil = (xs, ok) => Math.round(xs.findIndex(ok) * 1000 / 60);

test('fast attack, at most 3% overshoot', () => {
  const xs = run(0, 1, 500);
  const t90 = msUntil(xs, x => x >= 0.9);
  assert.ok(t90 >= 16 && t90 <= 50, `90% after ${t90} ms`);
  assert.ok(Math.max(...xs) <= 1.03, `peak ${Math.max(...xs)}`);
  assert.ok(Math.abs(xs.at(-1) - 1) < 0.005, 'settles on the level');
});

test('slow release, never below zero', () => {
  const xs = run(1, 0, 1000);
  const t10 = msUntil(xs, x => x <= 0.1);
  assert.ok(t10 >= 200 && t10 <= 400, `back to 10% after ${t10} ms`);
  assert.ok(xs.every(x => x >= 0));
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
