// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { ProfileList } from './LayoutProfiles';
import type { LayoutProfilesState, ProfileSummary } from '../api/layoutProfiles';
import { setLanguage } from '../i18n';
const profile = {id:'work',name:{kind:'custom' as const,value:'Office'},active:true,exact_match:true,content_revision:'1',last_used_ms:'0'};
const state = {profiles:[profile],readiness:'ready'} as LayoutProfilesState;
beforeEach(() => setLanguage('en'));
afterEach(cleanup);
function setup(onHotkey: (p: ProfileSummary, keys: string) => Promise<string|null>) {
  render(<ProfileList state={state} platform="mac" hotkeys={{work:'Control+Shift+F9'}} onHotkey={onHotkey} onRename={async()=>null} onDelete={async()=>{}}/>);
  return screen.getByRole('button', {name:'Shortcut for Switch to “Office”'});
}
it('keeps the recorded value on Escape and clears with Backspace', async () => {
  const save=vi.fn(async()=>null), recorder=setup(save);
  fireEvent.click(recorder); fireEvent.keyDown(recorder,{key:'Escape',code:'Escape'});
  expect(save).not.toHaveBeenCalled();
  fireEvent.click(recorder); fireEvent.keyDown(recorder,{key:'Backspace',code:'Backspace'});
  await waitFor(()=>expect(save).toHaveBeenCalledWith(profile,''));
});
it('shows an asynchronous backend conflict under the layout name', async () => {
  const save=vi.fn(async()=> 'Conflicts with “Open the Oink window”'), recorder=setup(save);
  fireEvent.click(recorder); fireEvent.keyDown(recorder,{key:'A',code:'KeyA',ctrlKey:true,altKey:true});
  await waitFor(()=>expect(screen.getByText('Conflicts with “Open the Oink window”')).toBeTruthy());
  expect(recorder.getAttribute('aria-invalid')).toBe('true');
});
