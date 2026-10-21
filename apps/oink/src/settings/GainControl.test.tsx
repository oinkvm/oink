// @vitest-environment jsdom
import { useState } from 'react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { GainControl } from './GainControl';
beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
afterEach(cleanup);
function Gain({ commit }: { commit: (v: number) => void }) {
  const [value, setValue] = useState(1);
  return <OinkProvider defaultMotion="reduced"><GainControl label="Gain" value={value} onChange={setValue} onCommit={commit}/></OinkProvider>;
}
const setup = () => {
  const commit = vi.fn();
  const result = render(<Gain commit={commit}/>);
  return { commit, slider: result.getByRole('slider'), input: result.getByRole('textbox') as HTMLInputElement, result };
};
it('steps the slider by 0.05 and shows the value in the field with a suffix', () => {
  const { commit, slider, input, result } = setup();
  expect(input.value).toBe('1.00');
  expect(input.closest('.ok-input')?.textContent).toContain('x');
  fireEvent.keyDown(slider, { key: 'ArrowRight' });
  expect(commit).toHaveBeenLastCalledWith(1.05);
  expect(slider.getAttribute('aria-valuenow')).toBe('1.05');
  expect(input.value).toBe('1.05');
  expect(result.container.querySelector('.setp__val')).toBeNull();
});
it('commits typed values at 0.01 precision on Enter and blur and moves the slider', () => {
  const { commit, slider, input } = setup();
  fireEvent.change(input, { target: { value: '1.234' } });
  expect(commit).not.toHaveBeenCalled();
  fireEvent.keyDown(input, { key: 'Enter' });
  expect(commit).toHaveBeenLastCalledWith(1.23);
  expect(input.value).toBe('1.23');
  expect(slider.getAttribute('aria-valuenow')).toBe('1.25');
  fireEvent.change(input, { target: { value: '2.5' } });
  fireEvent.blur(input);
  expect(commit).toHaveBeenLastCalledWith(2.5);
  expect(slider.getAttribute('aria-valuenow')).toBe('2.5');
});
it('clamps typed values to the slider range and ignores empty input', () => {
  const { commit, input } = setup();
  fireEvent.change(input, { target: { value: '9' } });
  fireEvent.blur(input);
  expect(commit).toHaveBeenLastCalledWith(5);
  expect(input.value).toBe('5.00');
  fireEvent.change(input, { target: { value: '0.01' } });
  fireEvent.blur(input);
  expect(commit).toHaveBeenLastCalledWith(0.1);
  expect(input.value).toBe('0.10');
  fireEvent.change(input, { target: { value: '' } });
  fireEvent.blur(input);
  expect(commit).toHaveBeenCalledTimes(2);
  expect(input.value).toBe('0.10');
});
