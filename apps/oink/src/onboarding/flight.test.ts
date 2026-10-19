import { describe, expect, it } from 'vitest';
import { createRope, FLY_MS, smooth, tailRest, type Pt } from './flight';

type Cubic = [Pt, Pt, Pt, Pt];
function cubics(d: string): Cubic[] {
  const values = d.match(/-?\d+(?:\.\d+)?/g)!.map(Number);
  let start = { x: values[0], y: values[1] };
  const spans: Cubic[] = [];
  for (let i = 2; i < values.length; i += 6) {
    const end = { x: values[i + 4], y: values[i + 5] };
    spans.push([start, { x: values[i], y: values[i + 1] }, { x: values[i + 2], y: values[i + 3] }, end]);
    start = end;
  }
  return spans;
}
function sample(d: string): Pt[] {
  return cubics(d).flatMap(p => Array.from({ length: 33 }, (_, i) => {
    const t = i / 32, u = 1 - t;
    return {
      x: u ** 3 * p[0].x + 3 * u * u * t * p[1].x + 3 * u * t * t * p[2].x + t ** 3 * p[3].x,
      y: u ** 3 * p[0].y + 3 * u * u * t * p[1].y + 3 * u * t * t * p[2].y + t ** 3 * p[3].y,
    };
  }));
}
function maxTurn(d: string): number {
  const points = sample(d), spaced = [points[0]];
  let left = 0, previous = points[0];
  for (const p of points.slice(1)) {
    const length = Math.hypot(p.x - previous.x, p.y - previous.y);
    for (let at = 0.5 - left; at <= length; at += 0.5) {
      const f = at / length;
      spaced.push({ x: previous.x + (p.x - previous.x) * f, y: previous.y + (p.y - previous.y) * f });
    }
    left = (left + length) % 0.5;
    previous = p;
  }
  let peak = 0;
  for (let i = 1; i < spaced.length - 1; i++) {
    const a = { x: spaced[i].x - spaced[i - 1].x, y: spaced[i].y - spaced[i - 1].y };
    const b = { x: spaced[i + 1].x - spaced[i].x, y: spaced[i + 1].y - spaced[i].y };
    peak = Math.max(peak, Math.acos(Math.max(-1, Math.min(1, (a.x * b.x + a.y * b.y) / Math.hypot(a.x, a.y) / Math.hypot(b.x, b.y)))) * 180 / Math.PI);
  }
  return peak;
}
function expectContinuous(d: string) {
  const spans = cubics(d);
  for (let i = 1; i < spans.length; i++) {
    const a = spans[i - 1], b = spans[i];
    for (const axis of ['x', 'y'] as const) {
      expect(a[3][axis]).toBe(b[0][axis]);
      // Match tangents and curvature, including after SVG decimal rounding.
      expect(Math.abs((a[3][axis] - a[2][axis]) - (b[1][axis] - b[0][axis]))).toBeLessThan(0.0003);
      expect(Math.abs((a[3][axis] - 2 * a[2][axis] + a[1][axis]) - (b[2][axis] - 2 * b[1][axis] + b[0][axis]))).toBeLessThan(0.0005);
    }
  }
}

describe('elastic tail path', () => {
  it('joins sparse, uneven control points with continuous tangents and curvature', () => {
    const points = [{ x: 0, y: 0 }, { x: 0.1, y: 1 }, { x: 18, y: 8 }, { x: 19, y: 8.1 }, { x: 23, y: 20 }];
    const original = structuredClone(points), d = smooth(points), spans = cubics(d);
    expectContinuous(d);
    expect(spans[0][0]).toEqual(points[0]);
    expect(spans.at(-1)![3]).toEqual(points.at(-1));
    expect(points).toEqual(original);
    expect(d).not.toMatch(/[LQ]/);
  });

  it.each([[4], [16], [80], [16, 17, 113, 1, 240, 33]])('has no turn above 10 degrees per half logo unit with frame intervals %j', (...intervals: number[]) => {
    const rope = createRope(tailRest(26));
    let t = 0, frame = 0;
    while (t <= FLY_MS) {
      rope.advance(t);
      const d = smooth(rope.points);
      expect(maxTurn(d)).toBeLessThan(10);
      expectContinuous(d);
      t += intervals[frame++ % intervals.length];
    }
  });

  it('preserves the resting curl within a fraction of the 5.5-unit stroke', () => {
    const reference = tailRest(400);
    const deviation = sample(smooth(tailRest(26))).map(p => Math.min(...reference.map(q => Math.hypot(p.x - q.x, p.y - q.y))));
    expect(Math.max(...deviation)).toBeLessThan(0.25);
  });

  it('handles stationary coincident points and empty input', () => {
    expect(smooth([])).toBe('');
    expect(smooth([{ x: 1, y: 2 }, { x: 1, y: 2 }])).not.toMatch(/NaN|Infinity/);
  });
});
