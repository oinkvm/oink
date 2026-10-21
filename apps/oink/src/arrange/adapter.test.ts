import { describe, it, expect } from 'vitest';
import type { Layout, Placement, Rect, Screen } from '../api/types';
import { layoutPlacements, presentScreens, renderLayout, visibleScreens } from './adapter';
import { cornerPoint, opposite, resizeFromCorner } from './geometry';
import { edgeProjections, sameProjections, tolerance } from './facing';
import { presentCards } from '../devices/project';
import type { DeviceCardsView } from '../api/cards';

const screen = (display: string, bounds: Rect): Screen => ({
    key: { device: 'computer', display },
    device_name: 'Computer',
    os: 'macos',
    display: {
        key: display,
        name: display,
        bounds,
        pixel_width: 2000,
        pixel_height: 1200,
        reference_extent: null,
        rotation_deg: 0,
        is_primary: display === 'a',
        is_builtin: false,
        is_virtual: false,
    },
    origin: { x: 1 + bounds.x * .001, y: 2 + bounds.y * .001 },
    size: { w: bounds.w * .001, h: bounds.h * .001 },
    size_source: 'reported',
    gain: 1,
    hidden: false,
    device_disabled: false,
    online: true,
});
const layout = (screens: Screen[]): Layout => ({
    revision: 1,
    source: 'manual',
    geometry: { screens, bounds: { x: 0, y: 0, w: 4, h: 3 } },
});
const placement: Placement = { device: 'computer', origin: { x: 5, y: 6 }, scale: .002 };

it.each(['disconnected','connecting','address_stale'] as const)('uses the device-list offline group for %s and follows reconnect across history',reason=>{
    const value=layout([screen('a',{x:0,y:0,w:1000,h:600}),screen('b',{x:1000,y:0,w:800,h:600})]);
    const history=renderLayout(value,{'computer:a':'wallpaper.png'},[placement]);
    const cards:DeviceCardsView={schema:1,stream:'canvas',revision:'1',group:null,generatedAtMs:0,exitVolume:null,emergencyDelivery:null,cards:[{
        device:'computer',name:'Computer',isSelf:false,os:'windows',logo:'windows10',section:'offline',
        connection:{kind:'offline',reason,signal:{kind:'offline',rttMs:null,bars:0,colour:'grey'}},
        lastSeenMs:null,status:null,pause:null,
    }]};
    const offline=presentScreens(history,presentCards(cards,[]));
    expect(offline.map(s=>s.offline)).toEqual([true,true]);
    expect(offline[0].image).toBe('wallpaper.png');
    cards.cards[0]={...cards.cards[0],section:'online',connection:{kind:'online',signal:{kind:'unknown',rttMs:null,bars:0,colour:'grey'}}};
    const online=presentScreens(offline,presentCards(cards,[]));
    expect(online.map(s=>s.offline)).toEqual([false,false]);
    expect(online[0].image).toBe('wallpaper.png');
    expect(online.map(s=>[s.x,s.y,s.w,s.h])).toEqual(history.map(s=>[s.x,s.y,s.w,s.h]));
    expect(history.every(s=>!s.offline)).toBe(true);
});

describe('group placement adapter and history projection', () => {
    it('renders one saved device transform over current native geometry', () => {
        const value = layout([
            screen('a', { x: 0, y: 0, w: 1000, h: 600 }),
            screen('b', { x: 1000, y: -200, w: 800, h: 500 }),
        ]);
        const rendered = renderLayout(value, {}, [placement]);
        expect(rendered[0]).toMatchObject({ x: 5 * 4096, y: 6 * 4096, w: 2 * 4096, h: 1.2 * 4096 });
        expect(rendered[1]).toMatchObject({ x: 7 * 4096, y: 5.6 * 4096, w: 1.6 * 4096, h: 1 * 4096 });
        expect(layoutPlacements(value, rendered)).toEqual([placement]);
    });

    it('undoing a device transform never restores an obsolete OS arrangement', () => {
        const changed = layout([
            screen('a', { x: 0, y: 0, w: 1000, h: 600 }),
            screen('b', { x: -800, y: -500, w: 800, h: 500 }),
            screen('c', { x: 1000, y: 0, w: 500, h: 500 }),
        ]);
        const rendered = renderLayout(changed, {}, [placement]);
        expect(rendered[1]).toMatchObject({ x: 3.4 * 4096, y: 5 * 4096, w: 1.6 * 4096, h: 1 * 4096 });
        expect(rendered[2]).toMatchObject({ x: 7 * 4096, y: 6 * 4096, w: 1 * 4096, h: 1 * 4096 });
        expect(rendered.every(s => s.groupSize === 3)).toBe(true);
    });
});

it('keeps resized native geometry and facing intervals through the frozen placement adapter', () => {
    const native = [{ w: 3840, h: 2160 }, { w: 1728, h: 1117 }, { w: 1728, h: 1117 }];
    const starts = [{ x: 493, y: 0, w: 741 }, { x: 0, y: 416.8125, w: 604 }, { x: 604, y: 472.8125, w: 604 }];
    const value = layout(native.map((r, i) => ({ ...screen(String(i), { x: 0, y: 0, ...r }), key: { device: String(i), display: String(i) } })));
    const shown = renderLayout(value, {}, starts.map((r, i) => ({ device: String(i), origin: { x: r.x / 4096, y: r.y / 4096 }, scale: r.w / native[i].w / 4096 })));
    for (const corner of ['nw', 'ne', 'sw', 'se'] as const) {
        const point = cornerPoint(shown[1], corner);
        const edited = shown.map((r, i) => i === 1 ? { ...r, ...resizeFromCorner(r, corner, { x: point.x + 73.3, y: point.y + 49.1 }, 1) } : r);
        const expanded = renderLayout(value, {}, layoutPlacements(value, edited));
        const eps = tolerance(edited);
        expect(sameProjections(edgeProjections(edited), edgeProjections(expanded), eps)).toBe(true);
        for (const key of ['x', 'y', 'w', 'h'] as const)
            expect(expanded[1][key]).toBeCloseTo(edited[1][key], 10);
        const expected = cornerPoint(shown[1], opposite[corner]);
        const actual = cornerPoint(expanded[1], opposite[corner]);
        expect(actual.x).toBeCloseTo(expected.x, 10);
        expect(actual.y).toBeCloseTo(expected.y, 10);
    }
});

it.each(['disconnected', 'connecting', 'address_stale'] as const)(
    'keeps hidden screens and disabled devices out of the %s canvas through history and reconnect', reason => {
        const value = layout([
            screen('a', {x:0,y:0,w:1000,h:600}),
            {...screen('b', {x:1000,y:0,w:800,h:600}), hidden:true, online:false},
            {...screen('c', {x:1800,y:0,w:800,h:600}), key:{device:'disabled',display:'c'}, device_disabled:true, online:false},
        ]);
        const cards:DeviceCardsView={schema:1,stream:'visibility',revision:'1',group:null,generatedAtMs:0,
            exitVolume:null,emergencyDelivery:null,cards:[{
                device:'computer',name:'Computer',isSelf:false,os:'windows',logo:'windows10',section:'offline',
                connection:{kind:'offline',reason,signal:{kind:'offline',rttMs:null,bars:0,colour:'grey'}},
                lastSeenMs:100,status:null,pause:null,
            }]};
        const devices=presentCards(cards,[]);
        const history=presentScreens(renderLayout(value,{},[placement]),devices);
        expect(visibleScreens(history,value,devices).map(s=>s.id)).toEqual(['computer:a']);
        expect(history).toHaveLength(3); // Hidden geometry remains available to placement history.
        cards.cards[0]={...cards.cards[0],section:'online',connection:{kind:'online',signal:{kind:'unknown',rttMs:null,bars:0,colour:'grey'}}};
        const reconnected=presentCards(cards,[]);
        expect(visibleScreens(presentScreens(history,reconnected),value,reconnected).map(s=>s.id)).toEqual(['computer:a']);
    },
);

it('hides every paused computer screen and restores exact stored geometry after resume', () => {
    const value = layout([
        screen('a', {x:0,y:0,w:1000,h:600}),
        screen('b', {x:1000,y:-200,w:800,h:500}),
    ]);
    const shown = renderLayout(value, {}, [placement]);
    const devices = [{id:'computer',screenSharing:true,screen:'shared' as const,paused:true}];
    expect(visibleScreens(shown,value,devices)).toEqual([]);
    expect(value.geometry.screens.every(s=>!s.hidden && !s.device_disabled)).toBe(true);
    const resumed = visibleScreens(shown,value,[{...devices[0],paused:false}]);
    expect(resumed).toEqual(shown);
    expect(layoutPlacements(value,resumed)).toEqual([placement]);
});
