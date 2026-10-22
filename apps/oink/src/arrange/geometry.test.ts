import { describe, expect, it } from 'vitest';
import { bounds, fitArrangement, cornerClearance, cornerPoint, opposite, clampMove, resizeFromCorner, snap, snapResize } from './geometry';
import type { Corner, SnapEdges } from './geometry';
describe('designer arrangement geometry',()=>{
  it('makes touching corners square and preserves independent outer corners',()=>{
    expect(cornerClearance({x:1512,y:0,w:800,h:982},[{x:0,y:0,w:1512,h:982}])).toEqual({nw:0,ne:null,sw:0,se:null});
  });
  it('stops keyboard movement at a neighbor and keeps the opposite resize corner fixed',()=>{
    expect(clampMove({x:0,y:0,w:800,h:500},[{x:1000,y:0,w:800,h:500}],'x',300)).toBe(200);
    const r=resizeFromCorner({x:0,y:0,w:800,h:500},'se',{x:1600,y:1000});
    expect(r.x).toBe(0);expect(r.y).toBe(0);expect(r.w/r.h).toBeCloseTo(1.6);
  });
});

describe('proportional resize snapping',()=>{
  const origin={x:0,y:0,w:800,h:500};
  it.each(['nw','ne','sw','se'] as Corner[])('snaps the %s moving edge while keeping the opposite corner and aspect ratio',corner=>{
    const east=corner[1]==='e',south=corner[0]==='s';
    const anchor=cornerPoint(origin,opposite[corner]);
    const live={x:east?anchor.x:anchor.x-995,y:south?anchor.y:anchor.y-621.875,w:995,h:621.875};
    const line=anchor.x+(east?1000:-1000);
    const neighbor={x:east?line:line-800,y:live.y+100,w:800,h:300};
    const r=snapResize(live,origin,corner,[neighbor],10);
    expect(cornerPoint(r,corner).x).toBe(line);
    expect(cornerPoint(r,opposite[corner])).toEqual(anchor);
    expect(r.w/r.h).toBeCloseTo(origin.w/origin.h);
  });
  it('aligns a resized bottom with a side neighbour for equal height',()=>{
    const live={...origin,w:952,h:595};
    const neighbor={x:960,y:0,w:900,h:600};
    expect(snapResize(live,origin,'se',[neighbor],10)).toEqual({...origin,w:960,h:600});
  });
  it('aligns a resized right with a neighbour above for equal width',()=>{
    const live={...origin,w:995,h:621.875};
    expect(snapResize(live,origin,'se',[{x:0,y:-500,w:1000,h:500}],10)).toEqual({...origin,w:1000,h:625});
  });
  it('chooses the closest proportional size when horizontal and vertical lines compete',()=>{
    const live={...origin,w:995,h:621.875};
    const neighbours=[{x:1000,y:100,w:500,h:500},{x:100,y:624,w:500,h:500}];
    const r=snapResize(live,origin,'se',neighbours,10);
    expect(r.h).toBe(624);
    expect(r.w).toBe(998.4);
  });
  it('uses a canvas-pixel threshold and ignores distant spans or sizes below the minimum',()=>{
    const live={...origin,w:995,h:621.875};
    const adjacent={x:1000,y:100,w:500,h:500};
    expect(snapResize(live,origin,'se',[adjacent],4)).toEqual(live);
    expect(snapResize(live,origin,'se',[adjacent],5).w).toBe(1000);
    expect(snapResize(live,origin,'se',[{...adjacent,y:1000}],10)).toEqual(live);
    const small={...origin,w:640,h:400};
    expect(snapResize(small,origin,'se',[{x:635,y:50,w:500,h:200}],10)).toEqual(small);
    expect(snapResize(small,origin,'se',[{x:635,y:50,w:500,h:200}],10,600).w).toBe(635);
  });
});

describe('canvas fit',()=>{
  it.each([24,48])('fits and centres negative-origin screens at %i px side/bottom margins',padding=>{
    const fit=bounds([{x:-2000,y:-900,w:1600,h:900},{x:-400,y:-600,w:2400,h:1500}]);
    const viewport={w:900,h:600};
    const {zoom,ox,oy}=fitArrangement(fit,viewport,padding);
    const left=ox+fit.x*zoom, top=oy+fit.y*zoom;
    const right=left+fit.w*zoom,bottom=top+fit.h*zoom;
    expect(left).toBeGreaterThanOrEqual(padding-1e-8);
    expect(right).toBeLessThanOrEqual(viewport.w-padding+1e-8);
    expect(top).toBeGreaterThanOrEqual(56-1e-8);
    expect(bottom).toBeLessThanOrEqual(viewport.h-padding+1e-8);
    expect(left-padding).toBeCloseTo(viewport.w-padding-right);
    expect(top-56).toBeCloseTo(viewport.h-padding-bottom);
    expect(Math.min(left-padding,top-56)).toBeCloseTo(0);
  });
  it('enlarges small arrangements without a zoom ceiling',()=>{
    expect(fitArrangement({x:0,y:0,w:100,h:100},{w:500,h:500},48)).toEqual({zoom:3.96,ox:52,oy:56});
  });
  it('fits by height with an invariant 56 px top inset and recomputes after resize',()=>{
    const fit={x:100,y:200,w:1000,h:2000};
    for(const viewport of [{w:1200,h:700},{w:820,h:560}]) {
      const {zoom,oy}=fitArrangement(fit,viewport,24);
      expect(zoom).toBeCloseTo((viewport.h-80)/2000);
      expect(oy+fit.y*zoom).toBeCloseTo(56);
      expect(oy+(fit.y+fit.h)*zoom).toBeCloseTo(viewport.h-24);
    }
  });
  it('handles an unmeasured or empty canvas without invalid numbers',()=>{
    expect(fitArrangement({x:0,y:0,w:0,h:0},{w:0,h:0},48)).toEqual({zoom:0,ox:0,oy:56});
  });
});

it('retains a fractional native aspect ratio and the fixed seam through all resize corners', () => {
  const origin = { x: 604.25, y: 472.8125, w: 604, h: 604 * 1117 / 1728 };
  for (const corner of ['nw', 'ne', 'sw', 'se'] as Corner[]) {
    const point = cornerPoint(origin, corner);
    const r = resizeFromCorner(origin, corner, { x: point.x + 73.3, y: point.y + 49.1 }, 1);
    expect(r.w / r.h).toBeCloseTo(1728 / 1117, 12);
    expect(cornerPoint(r, opposite[corner])).toEqual(cornerPoint(origin, opposite[corner]));
  }
});

describe('snap candidate roundoff', () => {
  it('keeps the same matched edge when overlapping equal-height screens are reprojected', () => {
    const neighbour = { x: 1000, y: 0, w: 800, h: 500 };
    for (const y of [0, 1e-14, -1e-14, 1e-13, -1e-13]) {
      const edges: SnapEdges = {};
      const result = snap({ x: 600, y, w: 800, h: 500 }, [neighbour], 10, edges);
      expect(result.y).toBeCloseTo(0, 12);
      expect(edges).toEqual({ y: '0:2' });
    }
  });

  it('keeps the first proportional resize candidate when two sizes differ only by roundoff', () => {
    const origin = { x: 0, y: 0, w: 800, h: 500 };
    const neighbour = { x: 1000, y: 625.0000000000001, w: 500, h: 500 };
    for (const w of [1000, 1000.0000000000002, 999.9999999999998]) {
      const edges: SnapEdges = {};
      const result = snapResize({ ...origin, w, h: w * .625 }, origin, 'se', [neighbour], 10, 640, edges);
      expect(result.w).toBeCloseTo(1000, 12);
      expect(edges).toEqual({ x: '0:0' });
    }
  });
});
