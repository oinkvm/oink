/** The visible fingerprint is the first 60 public-key bits in Crockford base32. */
export function fingerprint(device: string): string {
  const hex = device.replace(/^ed25519:/, '');
  if (!/^[a-f0-9]{64}$/i.test(hex)) return '—';
  const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
  const prefix = BigInt(`0x${hex.slice(0, 15)}`);
  const text = Array.from({length:12}, (_,i) => alphabet[Number((prefix >> BigInt((11-i)*5)) & 31n)]).join('');
  return `${text.slice(0,4)}-${text.slice(4,8)}-${text.slice(8)}`;
}
