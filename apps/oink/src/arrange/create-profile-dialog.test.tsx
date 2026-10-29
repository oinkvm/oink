// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, expect, it } from 'vitest';
import { useState } from 'react';
import { OinkProvider } from 'oink-ui';
import { setLanguage } from '../i18n';
import type { LayoutProfilesState } from '../api/layoutProfiles';
import { CreateProfileDialog } from './LayoutProfiles';

beforeAll(()=>Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})}));
afterEach(()=>{cleanup();setLanguage('en');});

const profile = (id: string, value: string) => ({ id, name: { kind: 'custom', value }, content_revision: '1', last_used_ms: '0', exact_match: false, active: false });

it('does not flag the newly created name as a duplicate while the dialog closes', async () => {
  setLanguage('en');
  let resolve!: (v: string | null) => void;
  function Harness() {
    const [state, setState] = useState({ readiness: 'ready', create_enabled: true, profiles: [profile('a', 'Home')] } as unknown as LayoutProfilesState);
    const [open, setOpen] = useState(true);
    return <OinkProvider defaultMotion="reduced"><CreateProfileDialog open={open} state={state} container={null} onClose={() => setOpen(false)}
      onSubmit={() => new Promise(r => { setState(s => ({ ...s, profiles: [...s.profiles, profile('b', 'Office')] }) as LayoutProfilesState); resolve = r; })}/></OinkProvider>;
  }
  render(<Harness/>);
  fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Office' } });
  fireEvent.click(screen.getByRole('button', { name: 'Create' }));
  expect(screen.queryByText('A layout with this name already exists')).toBeNull();
  await act(async () => { resolve(null); });
  expect(screen.queryByText('A layout with this name already exists')).toBeNull();
});
