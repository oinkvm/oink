import type { DeviceCardsView, DeviceCardSignals } from './cards';

/** RTT never changes the global agent snapshot. Subscribers are per device. */
export function createCardSignalsStore() {
  let stream: string | null = null, revision = '', sampledAt = -1;
  let values = new Map<string, number | null>();
  const listeners = new Map<string, Set<() => void>>();
  const replace = (next: Map<string, number | null>) => {
    const previous = values; values = next;
    listeners.forEach((callbacks, device) => {
      if (previous.get(device) !== next.get(device)) callbacks.forEach(callback => callback());
    });
  };
  return {
    get: (device: string) => values.get(device),
    subscribe(device: string, callback: () => void) {
      let callbacks = listeners.get(device);
      if (!callbacks) listeners.set(device, callbacks = new Set());
      callbacks.add(callback);
      return () => { callbacks.delete(callback); if (!callbacks.size) listeners.delete(device); };
    },
    seed(cards: DeviceCardsView) {
      if (stream === cards.stream && revision === cards.revision) return;
      stream = cards.stream; revision = cards.revision; sampledAt = -1;
      replace(new Map(cards.cards.map(card => [card.device, card.connection.kind === 'online' ? card.connection.signal.rttMs : null])));
    },
    receive(update: DeviceCardSignals) {
      if (update.stream !== stream || update.revision !== revision || update.sampledAtMs < sampledAt) return;
      sampledAt = update.sampledAtMs;
      // The owner sends all online rows. Missing rows must not keep an old RTT.
      replace(new Map(update.signals.map(signal => [signal.device, signal.rttMs])));
    },
    reset() { stream = null; revision = ''; sampledAt = -1; replace(new Map()); },
  };
}
