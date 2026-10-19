import parseSpdx from 'spdx-expression-parse';

// Select OR branches independently; every AND branch must be satisfied.
export function selectLicense(expression, allowed) {
  const walk = node => {
    if (node.conjunction === 'or') {
      try { return walk(node.left); } catch { return walk(node.right); }
    }
    if (node.conjunction === 'and') return `(${walk(node.left)} AND ${walk(node.right)})`;
    const id = node.license + (node.plus ? '+' : '') + (node.exception ? ` WITH ${node.exception}` : '');
    if (!allowed.has(id)) throw Error(`License branch is not allowed: ${id}`);
    return id;
  };
  return walk(parseSpdx(expression));
}
