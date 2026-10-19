import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

function styles(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? styles(path) : entry.name.endsWith('.css') ? [path] : [];
  });
}

describe('WebKit corner registration', () => {
  it('registers every shape override, including round and inherited shapes', () => {
    let count = 0;
    for (const path of styles(fileURLToPath(new URL('.', import.meta.url)))) {
      const css = readFileSync(path, 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
      for (const block of css.matchAll(/\{([^{}]*)\}/g)) {
        const declarations = block[1].split(';').map(value => value.trim()).filter(Boolean);
        declarations.forEach((declaration, index) => {
          const shape = /^corner-shape\s*:\s*(.+)$/.exec(declaration);
          if (!shape) return;
          count++;
          // WebKit drops corner-shape. Without its non-inherited marker an
          // app override can retain the library Button's round fallback shape.
          expect(declarations[index + 1], `${path}: ${declaration}`).toBe(`--ok-shape: ${shape[1].trim()}`);
        });
      }
    }
    expect(count).toBeGreaterThan(30);
  });
});
