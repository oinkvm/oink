import base from './vite.config.ts';

export default {
  ...base,
  test: { ...base.test, include: ['src/**/*.slow.test.{ts,tsx}'], exclude: [] },
};
