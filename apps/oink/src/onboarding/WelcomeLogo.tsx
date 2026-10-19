import { useEffect, useRef } from 'react';
import type { CSSProperties, RefObject } from 'react';
import { speed } from 'oink-ui';
import { FLY_MS, PIVOT, createRope, pose, smooth, tailRest } from './flight';

const TAIL_POINTS = 26;

const POINTER = 'M9 22.5V39L33.5 29.5Q23.5 15.5 9 6Z';
const TAIL = 'M20.5 32C20.5 46 31 54.5 45 54.5A9.5 9.5 0 1 0 36.3 41.2';

const STROKES: Array<{ d: string; dot?: boolean }> = [
  { d: 'M32 5.5a26.5 26.5 0 1 0 0 53a26.5 26.5 0 1 0 0 -53' },
  { d: 'M79.5 25.5V58.5' },
  { d: 'M79.5 7h0', dot: true },
  { d: 'M102.5 25.5V58.5' },
  { d: 'M102.5 58.5V38.5a13 13 0 0 1 26 0V58.5' },
  { d: 'M151.5 5.5V58.5' },
  { d: 'M172 25.5L151.5 44' },
  { d: 'M159.5 37.5L174.5 58.5' },
];

// Animate the pointer rigidly and the attached tail as a soft rope.
function useFly(leaving: boolean, flyer: RefObject<SVGGElement | null>, ptr: RefObject<SVGGElement | null>, tail: RefObject<SVGPathElement | null>) {
  useEffect(() => {
    if (!leaving) return;
    const s = speed();
    const f = flyer.current, g = ptr.current, t = tail.current;
    if (!f || !g || !t || s <= 0) return;
    const rope = createRope(tailRest(TAIL_POINTS));
    t.style.strokeDasharray = 'none';
    t.removeAttribute('pathLength');
    const start = performance.now();
    let raf = 0;
    const frame = (now: number) => {
      const ms = Math.min(FLY_MS, (now - start) / s);
      rope.advance(ms);
      const ps = pose(ms / FLY_MS);
      g.setAttribute('transform', `translate(${ps.x.toFixed(2)} ${ps.y.toFixed(2)}) rotate(${ps.rot.toFixed(2)} ${PIVOT.x} ${PIVOT.y})`);
      t.setAttribute('d', smooth(rope.points));
      const tt = ms / FLY_MS;
      f.style.opacity = String(tt < 0.8 ? 1 : 1 - (tt - 0.8) / 0.2);
      if (ms < FLY_MS) raf = requestAnimationFrame(frame);
    };
    raf = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(raf);
  }, [leaving]);
}

export function WelcomeLogo({ leaving }: { leaving: boolean }) {
  const flyer = useRef<SVGGElement>(null);
  const ptr = useRef<SVGGElement>(null);
  const tail = useRef<SVGPathElement>(null);
  useFly(leaving, flyer, ptr, tail);
  return (
    <div className="ob-logo" data-leaving={leaving ? '' : undefined} role="img" aria-label="Oink">
      <svg className="ob-logo__mark" viewBox="2 0 60 68" aria-hidden>
        <g transform="translate(0 1.5)" ref={flyer}>
          <path className="ob-logo__tail brand-tail" d={TAIL} pathLength={1} ref={tail} />
          <g className="ob-logo__ptr" ref={ptr}>
            <path className="ob-logo__ptr-path" d={POINTER} pathLength={1} />
          </g>
        </g>
      </svg>
      <svg className="ob-logo__word" viewBox="-6 -6 193 77" aria-hidden>
        {STROKES.map((s, i) => (
          <path
            key={i} d={s.d} pathLength={1}
            className={s.dot ? 'ob-logo__dot' : 'ob-logo__stroke'}
            style={{ '--i': i } as CSSProperties}
          />
        ))}
      </svg>
    </div>
  );
}
