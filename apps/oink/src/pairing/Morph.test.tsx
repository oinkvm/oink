// @vitest-environment jsdom
import { useState } from 'react';
import { act, cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Morph } from './Morph';

beforeEach(() => {
    vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
    vi.stubGlobal('getComputedStyle', () => ({ height: '100px', getPropertyValue: () => '1' }));
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); delete (HTMLElement.prototype as { animate?: unknown }).animate; });

function Form() {
    const [value, setValue] = useState('');
    return <input aria-label="Address" value={value} onChange={e => setValue(e.target.value)} />;
}

describe('pairing view morph', () => {
    it('keeps the populated manual view mounted until the code is ready', () => {
        const { rerender, getByRole, queryByText } = render(<Morph viewKey="manual"><Form /></Morph>);
        const input = getByRole('textbox');
        fireEvent.change(input, { target: { value: '192.0.2.1' } });
        rerender(<Morph viewKey="pair" pending><span>Connecting</span></Morph>);
        expect(getByRole('textbox')).toBe(input);
        expect((input as HTMLInputElement).value).toBe('192.0.2.1');
        expect(input.closest('[inert]')).not.toBeNull();
        expect(queryByText('Connecting')).toBeNull();
        rerender(<Morph viewKey="pair"><span>123456</span></Morph>);
        expect(queryByText('123456')).not.toBeNull();
        expect(document.querySelector('input')).toBeNull();
    });

    it('retains an inert outgoing view until the size and fades finish, then removes it', () => {
        const animations: Array<{ cancel: ReturnType<typeof vi.fn>; onfinish?: () => void }> = [];
        Object.defineProperty(HTMLElement.prototype, 'animate', { configurable: true, value: vi.fn(() => {
            const animation = { cancel: vi.fn() };
            animations.push(animation);
            return animation as unknown as Animation;
        }) });
        document.documentElement.style.setProperty('--ok-ms', '1');
        const { rerender, container } = render(<Morph viewKey="old"><Form /></Morph>);
        const input = container.querySelector('input');
        rerender(<Morph viewKey="new"><span>Code</span></Morph>);
        expect(container.querySelector('.morph__out input')).toBe(input);
        expect(container.querySelector('.morph__out')?.hasAttribute('inert')).toBe(true);
        expect(animations).toHaveLength(3);
        act(() => animations[0].onfinish?.());
        expect(container.querySelector('.morph__out')).toBeNull();
        expect(container.textContent).toBe('Code');
        expect(animations.every(a => a.cancel.mock.calls.length === 1)).toBe(true);
        // A completed morph must not overwrite the next start height during cleanup.
        rerender(<Morph viewKey="failed"><span>Failed</span></Morph>);
        expect(animations).toHaveLength(6);
        document.documentElement.style.removeProperty('--ok-ms');
    });
});
