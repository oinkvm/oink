
/** Fixed-step rope simulation keeps the elastic tail independent of frame rate. */
export type Pt = { x: number; y: number };
export type Pose = Pt & { rot: number };

export const FLY_MS = 900;

export const PIVOT: Pt = { x: 18, y: 24 };

const WIND = 0.2;
const BACK: Pose = { x: 4, y: 5, rot: 6 };

const AWAY: Pose = { x: -760, y: -680, rot: -16 };

const easeOut = (t: number) => 1 - (1 - t) ** 3;

export function pose(t: number): Pose {
  if (t <= 0) return { x: 0, y: 0, rot: 0 };
  if (t < WIND) {
    const e = easeOut(t / WIND);
    return { x: BACK.x * e, y: BACK.y * e, rot: BACK.rot * e };
  }
  const e = Math.min(1, (t - WIND) / (1 - WIND)) ** 2.4;
  return { x: BACK.x + (AWAY.x - BACK.x) * e, y: BACK.y + (AWAY.y - BACK.y) * e, rot: BACK.rot + (AWAY.rot - BACK.rot) * e };
}

export function place(p: Pt, ps: Pose): Pt {
  const a = (ps.rot * Math.PI) / 180;
  const dx = p.x - PIVOT.x, dy = p.y - PIVOT.y;
  return { x: PIVOT.x + dx * Math.cos(a) - dy * Math.sin(a) + ps.x, y: PIVOT.y + dx * Math.sin(a) + dy * Math.cos(a) + ps.y };
}

export function tailRest(n: number): Pt[] {
  const dense: Pt[] = [];
  const P = [{ x: 20.5, y: 32 }, { x: 20.5, y: 46 }, { x: 31, y: 54.5 }, { x: 45, y: 54.5 }];
  for (let i = 0; i <= 200; i++) {
    const t = i / 200, u = 1 - t;
    dense.push({
      x: u ** 3 * P[0].x + 3 * u * u * t * P[1].x + 3 * u * t * t * P[2].x + t ** 3 * P[3].x,
      y: u ** 3 * P[0].y + 3 * u * u * t * P[1].y + 3 * u * t * t * P[2].y + t ** 3 * P[3].y,
    });
  }
  const a0 = P[3], a1 = { x: 36.3, y: 41.2 }, r = 9.5;
  const mx = (a0.x + a1.x) / 2, my = (a0.y + a1.y) / 2;
  const h = Math.hypot(a1.x - a0.x, a1.y - a0.y) / 2;
  const off = Math.sqrt(Math.max(0, r * r - h * h));
  const ux = -(a1.y - a0.y) / (2 * h), uy = (a1.x - a0.x) / (2 * h);
  for (const sgn of [1, -1]) {
    const c = { x: mx + ux * off * sgn, y: my + uy * off * sgn };
    const t0 = Math.atan2(a0.y - c.y, a0.x - c.x);
    let t1 = Math.atan2(a1.y - c.y, a1.x - c.x);
    while (t1 > t0) t1 -= 2 * Math.PI;
    if (t0 - t1 <= Math.PI) continue;
    for (let i = 1; i <= 200; i++) {
      const th = t0 + (t1 - t0) * (i / 200);
      dense.push({ x: c.x + r * Math.cos(th), y: c.y + r * Math.sin(th) });
    }
    break;
  }
  const acc = [0];
  for (let i = 1; i < dense.length; i++) acc.push(acc[i - 1] + Math.hypot(dense[i].x - dense[i - 1].x, dense[i].y - dense[i - 1].y));
  const total = acc[acc.length - 1];
  const out: Pt[] = [];
  let j = 1;
  for (let i = 0; i < n; i++) {
    const want = (total * i) / (n - 1);
    while (j < acc.length - 1 && acc[j] < want) j++;
    const f = (want - acc[j - 1]) / (acc[j] - acc[j - 1] || 1);
    out.push({ x: dense[j - 1].x + (dense[j].x - dense[j - 1].x) * f, y: dense[j - 1].y + (dense[j].y - dense[j - 1].y) * f });
  }
  return out;
}

export const STEP_MS = 4;
const ITERATIONS = 6;

let DAMPING = 0.99;

let BEND = 0.22;

let STRETCH = 0.5;

let SHAPE = 0.02;
export const tune = (o: { damping?: number; bend?: number; stretch?: number; shape?: number }) => {
  if (o.damping != null) DAMPING = o.damping;
  if (o.bend != null) BEND = o.bend;
  if (o.stretch != null) STRETCH = o.stretch;
  if (o.shape != null) SHAPE = o.shape;
};

export function createRope(rest: Pt[], poseAt: (ms: number) => Pose = (ms) => pose(ms / FLY_MS)) {
  const n = rest.length;
  const dist = (a: Pt, b: Pt) => Math.hypot(b.x - a.x, b.y - a.y);
  const links: Array<[number, number, number, number]> = [];
  for (let i = 1; i < n - 1; i++) links.push([i, i + 1, dist(rest[i], rest[i + 1]), 0]);
  for (let i = 0; i < n - 2; i++) links.push([i, i + 2, dist(rest[i], rest[i + 2]), 1]);
  for (let i = 0; i < n - 3; i++) links.push([i, i + 3, dist(rest[i], rest[i + 3]), 1]);
  let cur = rest.map((p) => ({ ...p }));
  let prev = rest.map((p) => ({ ...p }));
  let t = 0;
  const step = () => {
    t += STEP_MS;
    const ps = poseAt(t);
    const goal = rest.map((p) => place(p, ps));
    const next = cur.map((p, i) => {
      if (i < 2) return goal[i];
      const k = SHAPE * (1 - i / n);
      const x = p.x + (p.x - prev[i].x) * DAMPING;
      const y = p.y + (p.y - prev[i].y) * DAMPING;
      return { x: x + (goal[i].x - x) * k, y: y + (goal[i].y - y) * k };
    });
    for (let it = 0; it < ITERATIONS; it++) {
      for (const [i, j, len, kind] of links) {
        const a = next[i], b = next[j];
        const wa = i < 2 ? 0 : 1, wb = 1;
        const dx = b.x - a.x, dy = b.y - a.y;
        const d = Math.hypot(dx, dy) || 1e-6;
        const c = (((d - len) / d) * (kind ? BEND : STRETCH)) / (wa + wb);
        a.x += dx * c * wa; a.y += dy * c * wa;
        b.x -= dx * c * wb; b.y -= dy * c * wb;
      }
    }
    prev = cur;
    cur = next;
  };
  return {

    advance(ms: number) { while (t + STEP_MS <= Math.min(ms, FLY_MS + STEP_MS)) step(); },
    get points() { return cur; },
  };
}

/**
 * Treat the simulated nodes as a cubic B-spline control polygon. Interpolating
 * every node with Catmull-Rom concentrated bends into short elbows when elastic
 * links stretched unevenly. These Bezier spans share first and second
 * derivatives, distributing each bend without changing the rope simulation.
 */
export function smooth(ps: Pt[]) {
  if (!ps.length) return '';
  const f = (v: number) => v.toFixed(4);
  let d = `M${f(ps[0].x)} ${f(ps[0].y)}`;
  // Repeated endpoints keep the attachment and rounded tip in their exact
  // simulated positions; the interior stays inside the control polygon.
  const padded = [ps[0], ps[0], ...ps, ps[ps.length - 1], ps[ps.length - 1]];
  for (let i = 0; i < padded.length - 3; i++) {
    const [, p1, p2, p3] = padded.slice(i, i + 4);
    const c1 = { x: (2 * p1.x + p2.x) / 3, y: (2 * p1.y + p2.y) / 3 };
    const c2 = { x: (p1.x + 2 * p2.x) / 3, y: (p1.y + 2 * p2.y) / 3 };
    const end = { x: (p1.x + 4 * p2.x + p3.x) / 6, y: (p1.y + 4 * p2.y + p3.y) / 6 };
    d += `C${f(c1.x)} ${f(c1.y)} ${f(c2.x)} ${f(c2.y)} ${f(end.x)} ${f(end.y)}`;
  }
  return d;
}
