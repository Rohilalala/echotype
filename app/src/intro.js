// The first-launch film, Acts 1-2: deep space over whatever is on screen, then gravity pulls the
// stars into the dotted pill. One WebGL2 draw per frame: every star is an instanced quad whose whole
// path (drift, push-in, fall, settle) is computed on the GPU from the time uniform, drawn as a short
// streak along its motion so fast stars blur like a long exposure. No background: the canvas is
// transparent and every star carries a faint dark rim so it still reads over white windows.

const TAU = Math.PI * 2;

// Seeded random, so every launch (and the offline render) is the same sky.
function rng(seed) {
  let a = seed >>> 0; // mulberry32
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// Black-body colour for a star temperature (K), approximately (Tanner Helland's fit), 0..1.
function kelvin(t) {
  t /= 100;
  const r = t <= 66 ? 255 : 329.7 * (t - 60) ** -0.1332;
  const g = t <= 66 ? 99.47 * Math.log(t) - 161.12 : 288.12 * (t - 60) ** -0.0755;
  const b = t >= 66 ? 255 : t <= 19 ? 0 : 138.52 * Math.log(t - 10) - 305.04;
  return [r, g, b].map(v => Math.min(255, Math.max(0, v)) / 255);
}

// Dot positions of the ball: a Fibonacci sphere of radius r, tilted towards the viewer and seen
// straight on. Each dot is [x, y, depth 0 (back) .. 1 (front)], so the back half reads dimmer.
export function ballDots(n, r, tilt = 0.42, spin = 0.6) {
  const out = [], golden = Math.PI * (3 - Math.sqrt(5));
  for (let i = 0; i < n; i++) {
    const y = 1 - (2 * (i + 0.5)) / n, rad = Math.sqrt(1 - y * y), a = i * golden + spin;
    const x = Math.cos(a) * rad, z = Math.sin(a) * rad;
    const y2 = y * Math.cos(tilt) - z * Math.sin(tilt), z2 = y * Math.sin(tilt) + z * Math.cos(tilt);
    out.push([x * r, -y2 * r, (z2 + 1) / 2]);
  }
  return out;
}

const VERT = `#version 300 es
precision highp float;
layout(location=0) in vec2 corner;      // quad corner, -1..1
layout(location=1) in vec3 star;        // x, y (px from centre), depth 0.15..1 (1 = far)
layout(location=2) in vec4 look;        // brightness 0..1, twinkle phase, spike (0/1), ball depth 0..1 (-1 = absorbed)
layout(location=3) in vec3 tint;        // black-body colour
layout(location=4) in vec4 fall;        // target x, y (px), delay, swirl
uniform vec2 view;       // canvas px
uniform float t;         // seconds since the film began
uniform float dpr;
uniform vec4 timeline;   // collapse start, collapse span, formation time, pill glow
uniform vec2 pillAt;     // pill centre (px from screen centre)
out vec2 uv;
out float seg;           // streak length in radii
out vec3 col;
out float light;
out float rim;
out float spike;
out float near;

vec2 drift(vec3 s, float tt) {
  // Slow push-in (near stars spread faster) and a sideways drift scaled by nearness.
  float push = 1.0 + 0.035 * tt / s.z;
  return s.xy * push + vec2(-6.0, 2.5) * tt / s.z;
}

vec2 path(float tt) {
  vec2 p = drift(star, tt);
  float e = clamp((tt - timeline.x - fall.z) / timeline.y, 0.0, 1.0);
  if (e <= 0.0) return p;
  vec2 to = fall.xy + pillAt;
  vec2 from = drift(star, timeline.x + fall.z);
  // Falling: slow to start, fast at the end (gravity), curving around the mass as it goes.
  float k = e * e * (3.0 - 2.0 * e);
  k = mix(k, e * e * e, 0.55);
  vec2 v = from - to;
  float a = fall.w * (1.0 - k) * k * 3.0;
  mat2 rot = mat2(cos(a), -sin(a), sin(a), cos(a));
  vec2 q = to + rot * v * (1.0 - k);
  // Landing settle for the pill's own dots: a tiny under-damped bounce after arrival.
  float since = tt - (timeline.x + fall.z + timeline.y);
  if (look.w >= 0.0 && since > 0.0) q += normalize(v + 1e-4) * 3.0 * exp(-since * 9.0) * sin(since * 30.0);
  return q;
}

void main() {
  float b = look.x;
  float arrived = clamp((t - (timeline.x + fall.z + timeline.y)) * 6.0, 0.0, 1.0);
  bool pill = look.w >= 0.0;
  // Twinkle on bright stars only, and only a little (real seeing, not a cartoon).
  float tw = 1.0 + 0.12 * b * sin(t * (2.3 + 3.0 * fract(look.y * 7.1)) + look.y * 6.2831);
  float bright = b * tw;
  float r = (0.55 + 2.6 * pow(b, 1.4)) * mix(1.0, 0.7, 1.0 - star.z);  // radius, css px
  vec3 c = tint;
  float fade = 1.0;
  if (pill) {
    // Arriving dots cool to the orb's white and an even size; the back of the ball is dimmer.
    c = mix(c, vec3(0.93, 0.95, 1.0), arrived);
    r = mix(r, mix(1.0, 1.7, look.w), arrived);
    bright = mix(bright, mix(0.28, 1.0, look.w), arrived) * (1.0 + timeline.w);
  } else {
    // Absorbed stars flare as they arrive, then are gone into the mass.
    fade = 1.0 - arrived;
    bright *= 1.0 + 2.5 * arrived * (1.0 - arrived) * 4.0;
  }
  // The fade-in of the opening: faint stars first, bright ones a little later.
  float born = clamp((t - 0.25 - (1.0 - b) * 0.9) / 0.9, 0.0, 1.0);
  fade *= born * born;
  vec2 p1 = path(t), p0 = path(t - 1.0 / 14.0);    // a long-exposure streak: ~70 ms of travel
  vec2 d = p1 - p0;
  float len = length(d);
  vec2 dir = len > 1e-3 ? d / len : vec2(1.0, 0.0);
  vec2 nrm = vec2(-dir.y, dir.x);
  float halo = r * (look.z > 0.5 ? 7.0 : 4.6);    // room for the glow, the shadow (and the spikes)
  vec2 mid = (p0 + p1) * 0.5;
  vec2 pos = mid + dir * corner.x * (len * 0.5 + halo) + nrm * corner.y * halo;
  uv = vec2(corner.x * (len * 0.5 + halo), corner.y * halo) / r;
  seg = len * 0.5 / r;
  col = c;
  light = bright * fade;
  rim = fade * (pill ? 1.0 - 0.6 * arrived : 1.0);
  near = 1.0 - star.z;  // 0 far .. ~0.85 near: how high above the screen this star floats
  spike = look.z * (1.0 - smoothstep(0.0, 0.6, (t - timeline.x) / timeline.y)) * fade;
  gl_Position = vec4((pos * dpr) / (view * 0.5) * vec2(1.0, -1.0), 0.0, 1.0);
}`;

const FRAG = `#version 300 es
precision highp float;
in vec2 uv;
in float seg;
in vec3 col;
in float light;
in float rim;
in float spike;
in float near;
uniform float rimK;
out vec4 o;
void main() {
  // Distance to the streak (a segment along x), in star radii.
  vec2 q = vec2(max(abs(uv.x) - seg, 0.0), uv.y);
  float d2 = dot(q, q);
  float core = exp(-d2 * 2.2);                 // the star itself (a gaussian PSF)
  float glow = exp(-d2 * 0.22) * 0.18;         // scattered light around it
  float sp = spike * (exp(-abs(uv.y) * 7.0) * exp(-abs(uv.x) * 0.55) + exp(-abs(uv.x) * 7.0) * exp(-abs(uv.y) * 0.55)) * 0.35;
  // A streak is a comet, not a stick: brightest at its head (+x), fading back along its tail.
  float along = seg > 0.05 ? clamp((uv.x + seg) / (2.0 * seg), 0.0, 1.0) : 1.0;
  float taper = mix(0.08, 1.0, along * along);
  float lit = (core * taper + glow * mix(0.3, 1.0, along) + sp) * light / (1.0 + seg * 0.18);
  // The star floats above the screen: a soft shadow falls down and to the right of it, further
  // and larger the nearer it is. It is what keeps a star readable over a white window, too.
  vec2 off = vec2(0.45, 0.75) * (0.6 + 2.2 * near);
  vec2 qs = vec2(max(abs(uv.x - off.x) - seg, 0.0), uv.y - off.y);
  float soft = 0.55 / (1.0 + near * 1.6);
  float shadow = exp(-dot(qs, qs) * soft) * 0.34 * rim * min(light * 4.0, 1.0) * (0.55 + 0.6 * near);
  vec3 c = col * lit;
  float a = clamp(lit + shadow * (1.0 - lit), 0.0, 1.0);
  o = vec4(c, a);   // premultiplied
}`;

/// The lights going down: how dark the veil over the desktop is at time t (0..1 of its strength).
/// It comes in with the stars, holds through the collapse and lifts once the pill has formed.
export function veilAt(t, formed = 6.9) {
  const ease = x => x * x * (3 - 2 * x);
  const inn = ease(Math.min(1, Math.max(0, t / 1.4)));
  const out = ease(Math.min(1, Math.max(0, (t - formed - 0.3) / 1.3)));
  return inn * (1 - out);
}

export function createIntro(canvas, opts = {}) {
  const gl = canvas.getContext('webgl2', { premultipliedAlpha: true, alpha: true, antialias: false });
  if (!gl) return null;
  const sh = (type, src) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    return s;
  };
  const prog = gl.createProgram();
  gl.attachShader(prog, sh(gl.VERTEX_SHADER, VERT));
  gl.attachShader(prog, sh(gl.FRAGMENT_SHADER, FRAG));
  gl.linkProgram(prog);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog));
  const U = n => gl.getUniformLocation(prog, n);
  const u = { view: U('view'), t: U('t'), dpr: U('dpr'), timeline: U('timeline'), pillAt: U('pillAt'), rimK: U('rimK') };

  const dpr = opts.dpr || Math.min(2, devicePixelRatio || 1);
  let W = 0, H = 0, count = 0;
  const vao = gl.createVertexArray();
  const quad = gl.createBuffer(), inst = gl.createBuffer();

  // Timeline (s): stars fade in over ~1.2 s; the collapse runs from 4.0 for 2.2 s (+ delays).
  const T = { collapse: 4.0, span: 1.7, formed: 6.9 };
  const ball = { n: opts.ballDots || 520, r: opts.ballR || 46 };

  function build() {
    W = canvas.clientWidth;
    H = canvas.clientHeight;
    canvas.width = Math.round(W * dpr);
    canvas.height = Math.round(H * dpr);
    const R = rng(7), N = opts.stars || 9000;
    const dots = ballDots(ball.n, ball.r);
    const data = new Float32Array(N * 14);
    // The Milky Way: a tilted band across the screen where stars crowd, broken by dust lanes.
    const band = { a: -0.38, w: Math.min(W, H) * 0.16 };
    const lane = (x, y) => Math.sin(x * 0.011 + Math.sin(y * 0.02) * 1.7) * Math.sin(x * 0.004 - y * 0.013 + 1.3);
    const order = []; // which stars become pill dots: brighter, nearer ones, spread over the sky
    for (let i = 0; i < N; i++) {
      let x, y;
      // ~a third are dust: tiny, faint, crowding the band, so it glows with depth instead of dots.
      const dust = R() < 0.34;
      if (R() < (dust ? 0.85 : 0.38)) {
        // in the band, gaussian across it, thinned where the dust is
        do {
          const along = (R() - 0.5) * Math.hypot(W, H) * 1.1;
          const across = (R() + R() + R() - 1.5) * band.w;
          x = Math.cos(band.a) * along - Math.sin(band.a) * across;
          y = Math.sin(band.a) * along + Math.cos(band.a) * across;
        } while (lane(x, y) > 0.35 && R() < 0.8);
      } else {
        x = (R() - 0.5) * W * 1.15;
        y = (R() - 0.5) * H * 1.15;
      }
      const z = 0.15 + 0.85 * R() ** 0.6;
      // Most stars are faint; a handful are bright (a magnitude-like power law).
      const b = dust ? 0.03 + 0.07 * R() : Math.min(1, 0.06 + 0.94 * R() ** 7);
      // Temperatures weighted towards sun-like and cooler; a few hot blue-white ones.
      const k = R();
      const temp = k < 0.08 ? 2900 + R() * 900 : k < 0.62 ? 4300 + R() * 2200 : k < 0.92 ? 6500 + R() * 3500 : 10000 + R() * 18000;
      const [cr, cg, cb] = kelvin(temp);
      const spike = b > 0.82 ? 1 : 0;
      data.set([x, y, z, b, R(), spike, -1, cr, cg, cb, 0, 0, 0, 0], i * 14);
      order.push([i, b * 0.6 + (1 - z) * 0.4 + R() * 0.25]);
    }
    order.sort((p, q) => q[1] - p[1]);
    const pillCentre = [0, 0]; // the ball forms at the centre of the screen
    // Assign each pill dot the nearest unused candidate star, so falls cross as little as possible.
    const pool = order.slice(0, Math.min(N, dots.length * 3)).map(o => o[0]);
    const used = new Set();
    dots.forEach(([dx, dy, dz]) => {
      let best = -1, bd = Infinity;
      for (const i of pool) {
        if (used.has(i)) continue;
        const sx = data[i * 14], sy = data[i * 14 + 1];
        const dd = Math.atan2(sy - pillCentre[1], sx) - Math.atan2(dy, dx);
        const cost = Math.abs(Math.atan2(Math.sin(dd), Math.cos(dd)));
        if (cost < bd) { bd = cost; best = i; }
      }
      used.add(best);
      data[best * 14 + 6] = dz;
      data[best * 14 + 10] = dx;
      data[best * 14 + 11] = dy;
    });
    for (let i = 0; i < N; i++) {
      const o = i * 14, x = data[o], y = data[o + 1];
      const dist = Math.hypot(x, y - pillCentre[1]) / Math.hypot(W, H);
      if (data[o + 6] < 0) { const a = R() * TAU, rr = Math.sqrt(R()) * ball.r * 0.8; data[o + 10] = Math.cos(a) * rr; data[o + 11] = Math.sin(a) * rr; }
      // Near the mass falls first; the far sky follows, with a little spread so it streams.
      data[o + 12] = dist * 1.5 + R() * 0.3 + (data[o + 6] >= 0 ? 0.35 : 0);
      data[o + 13] = (R() < 0.5 ? -1 : 1) * (0.5 + R() * 0.9);
    }
    count = N;
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, quad);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.bindBuffer(gl.ARRAY_BUFFER, inst);
    gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
    const attr = (loc, size, off) => {
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, size, gl.FLOAT, false, 56, off * 4);
      gl.vertexAttribDivisor(loc, 1);
    };
    attr(1, 3, 0);
    attr(2, 4, 3);
    attr(3, 3, 7);
    attr(4, 4, 10);
    state.pillCentre = pillCentre;
  }

  const state = { pillCentre: [0, 0] };

  function draw(t) {
    gl.viewport(0, 0, canvas.width, canvas.height);
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA); // premultiplied "over"
    gl.useProgram(prog);
    gl.uniform2f(u.view, canvas.width, canvas.height);
    gl.uniform1f(u.t, t);
    gl.uniform1f(u.dpr, dpr);
    // The formation glow: swells as the last stars land, then settles.
    const g = Math.max(0, 1 - Math.abs(t - T.formed) / 0.6);
    gl.uniform4f(u.timeline, T.collapse, T.span, T.formed, 0.9 * g * g);
    gl.uniform1f(u.rimK, opts.rim ?? 0);
    gl.uniform2f(u.pillAt, state.pillCentre[0], state.pillCentre[1]);
    gl.bindVertexArray(vao);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, count);
  }

  build();
  // Where the ball sits (css px from the top-left) and its radius, for the dark backing disc the
  // page draws under it: the same dark ball the pill shows, so it reads on any screen.
  const ballAt = () => ({ x: W / 2 + state.pillCentre[0], y: H / 2 + state.pillCentre[1], r: ball.r });
  return { draw, build, ballAt, timeline: T };
}
