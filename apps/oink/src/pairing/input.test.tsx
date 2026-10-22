// @vitest-environment jsdom
import { useState } from 'react';
import { cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { PinInput } from 'oink-ui';
import { readFileSync } from 'node:fs';
const pinCss = readFileSync('../../node_modules/oink-ui/dist/styles/pin-input.css', 'utf8');
const pairingCss = readFileSync('src/pairing/pairing.css', 'utf8');
afterEach(cleanup);
function Input({ complete }: { complete: (code: string) => void }) {
  const [value, setValue] = useState('');
  return <PinInput className="pair__pin" autoFocus value={value} onChange={setValue} onComplete={complete} />;
}
describe('pairing code entry', () => {
  it('hands focus to one stable next cell and submits all six digits once', () => {
    const complete = vi.fn();
    const { container } = render(<Input complete={complete} />);
    const cells = [...container.querySelectorAll('input')];
    for (let i = 0; i < cells.length; i++) {
      fireEvent.change(cells[i], { target: { value: String(i + 1) } });
      expect(document.activeElement).toBe(cells[Math.min(i + 1, 5)]);
      expect([...container.querySelectorAll('input')]).toEqual(cells);
    }
    expect(complete).toHaveBeenCalledExactlyOnceWith('123456');
  });
  it('hides the text caret in every code box; the focus ring marks the active box', () => {
    const { container } = render(<><style>{pinCss + pairingCss}</style><Input complete={() => {}} /></>);
    for (const cell of container.querySelectorAll('input')) expect(['transparent', 'rgba(0, 0, 0, 0)']).toContain(getComputedStyle(cell).caretColor);
  });
  it('keeps paste and backspace moving through the boxes', () => {
    const complete = vi.fn();
    const { container } = render(<Input complete={complete} />);
    const cells = [...container.querySelectorAll('input')];
    fireEvent.change(cells[0], { target: { value: '7' } });
    fireEvent.keyDown(cells[1], { key: 'Backspace' });
    expect(document.activeElement).toBe(cells[0]);
    fireEvent.paste(cells[0], { clipboardData: { getData: () => '654321' } });
    expect(cells.map(c => c.value).join('')).toBe('654321');
    expect(complete).toHaveBeenCalledExactlyOnceWith('654321');
  });
});
