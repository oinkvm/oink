import type { ArrangementSnapshot } from '../api/layoutProfiles';
import type { Placement, ScreenKey } from '../api/types';

type Send = (gesture: number, placements: Placement[], affected: ScreenKey[], ended: boolean, arrangement?: ArrangementSnapshot) => Promise<unknown>;
let serial = Date.now() * 1000;

/** One ordered IPC request in flight; intermediate animation frames are replaced.
 * Heartbeats keep a stationary held gesture alive; closing never writes layout. */
export class LayoutProjectionPreview {
  private gesture = 0;
  private releaseTimer: ReturnType<typeof setTimeout> | undefined;
  get lingering() { return this.releaseTimer !== undefined; }
  private latest: Placement[] | null = null;
  private arrangement: ArrangementSnapshot | undefined;
  private pending: { gesture: number; placements: Placement[]; ended: boolean; arrangement?: ArrangementSnapshot } | null = null;
  private busy = false;
  private timer: ReturnType<typeof setInterval> | undefined;
  constructor(private send: Send) {}
  update(placements: Placement[], arrangement?: ArrangementSnapshot) {
    clearTimeout(this.releaseTimer); this.releaseTimer = undefined;
    this.arrangement = arrangement;
    if (!this.gesture) {
      this.gesture = ++serial;
      this.timer = setInterval(() => {
        if (this.latest) this.enqueue(this.latest, false);
      }, 500);
    }
    this.latest = placements;
    this.enqueue(placements, false);
  }
  linger() {
    clearTimeout(this.releaseTimer);
    this.releaseTimer = setTimeout(() => { this.releaseTimer = undefined; this.end(); }, 1200);
  }
  end() {
    if (this.lingering) return;
    if (!this.gesture) return;
    clearInterval(this.timer);
    this.enqueue([], true);
    this.gesture = 0;
    this.latest = null;
  }
  close() { clearTimeout(this.releaseTimer); this.releaseTimer = undefined; this.end(); }
  private enqueue(placements: Placement[], ended: boolean) {
    this.pending = { gesture: this.gesture, placements, ended, ...(!ended && this.arrangement ? { arrangement: this.arrangement } : {}) };
    void this.flush();
  }
  private async flush() {
    if (this.busy) return;
    this.busy = true;
    try {
      while (this.pending) {
        const frame = this.pending;
        this.pending = null;
        // Empty affected means the complete group layout on every real screen.
        await (frame.arrangement
          ? this.send(frame.gesture, frame.placements, [], frame.ended, frame.arrangement)
          : this.send(frame.gesture, frame.placements, [], frame.ended)).catch(() => {});
      }
    } finally { this.busy = false; }
  }
}
