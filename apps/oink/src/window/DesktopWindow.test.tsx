// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { shell } from '../api/client';
import { DesktopWindow } from './DesktopWindow';

vi.mock('../api/transport',()=>({inTauri:true}));
beforeAll(()=>{
  vi.stubGlobal('ResizeObserver',class {observe(){} disconnect(){}});
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});
});
afterEach(()=>{cleanup();vi.restoreAllMocks();});
describe('native macOS titlebar',()=>{
  it('applies the actual native centre to the macOS toolbar',async()=>{
    vi.spyOn(shell,'titlebarCenter').mockResolvedValue(50);
    vi.spyOn(HTMLElement.prototype,'getBoundingClientRect').mockReturnValue({top:24,height:44} as DOMRect);
    const noop=()=>{};
    const {container}=render(<OinkProvider><DesktopWindow platform="mac" state={{maximized:false,minimized:false,closed:false}} active title="Oink" onMinimize={noop} onToggleMaximize={noop} onClose={noop} onActivate={noop} navOpen onToggleNav={noop} nav={()=>null}>Canvas</DesktopWindow></OinkProvider>);
    await waitFor(()=>expect(container.querySelector<HTMLElement>('.win__titlebar')!.style.getPropertyValue('--native-controls-offset')).toBe('4px'));
  });
  it('reserves the native control space on the same toolbar row as the sidebar toggle',()=>{
    const noop=()=>{};
    const {container}=render(<OinkProvider><DesktopWindow platform="mac" state={{maximized:false,minimized:false,closed:false}} active title="Oink" onMinimize={noop} onToggleMaximize={noop} onClose={noop} onActivate={noop} navOpen onToggleNav={noop} nav={()=>null}>Canvas</DesktopWindow></OinkProvider>);
    const lights=container.querySelector<HTMLElement>('.mac-lights')!;
    const toggle=screen.getByRole('button',{expanded:true});
    expect(lights.style.visibility).toBe('hidden');
    expect(lights.style.pointerEvents).toBe('none');
    expect(lights.nextElementSibling).toBe(toggle);
    expect(lights.parentElement?.className).toBe('win__titlebar');
    expect(lights.parentElement?.hasAttribute('data-tauri-drag-region')).toBe(true);
  });
});

describe('Windows titlebar isolation',()=>{
  it('keeps Windows controls and never requests macOS frame alignment',()=>{
    const query=vi.spyOn(shell,'titlebarCenter').mockResolvedValue(50);
    const noop=()=>{};
    const {container}=render(<OinkProvider><DesktopWindow platform="win" state={{maximized:false,minimized:false,closed:false}} active title="Oink" onMinimize={noop} onToggleMaximize={noop} onClose={noop} onActivate={noop} navOpen onToggleNav={noop} nav={()=>null}>Canvas</DesktopWindow></OinkProvider>);
    expect(container.querySelector('.mac-lights')).toBeNull();
    expect(container.querySelectorAll('.win-cap')).toHaveLength(3);
    expect(container.querySelector<HTMLElement>('.win__titlebar')!.style.getPropertyValue('--native-controls-offset')).toBe('');
    expect(query).not.toHaveBeenCalled();
  });
});
