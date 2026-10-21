import { LayoutProfileFailure, type PreviewReply, type PreviewRequest } from '../api/layoutProfiles';
import type { Point, ScreenKey } from '../api/types';

/**
 * Backend previews for dragging one native screen. At most one request is in
 * flight; pointer positions that arrive meanwhile collapse into the newest one.
 * Replies from an older gesture or sequence are discarded. The preview is
 * read-only, so a `stale_state` refusal is retried once with the current token.
 */
export class ScreenPreview {
  private gesture = 0;
  private sequence = 0n;
  private flight: Promise<void> | null = null;
  private wanted: Point | null = null;
  private sent: Point | null = null;
  private last: { at: Point; reply: PreviewReply } | null = null;
  private failure: unknown = null;
  private target: { device: string; screen: ScreenKey } | null = null;
  constructor(
    private readonly request: (request: PreviewRequest) => Promise<PreviewReply>,
    private readonly token: () => string,
    private readonly onReply: (reply: PreviewReply) => void,
  ) {}
  get active() { return this.target !== null; }
  /** Starts a gesture for `screen`, cancelling any other. */
  begin(screen: ScreenKey) {
    this.gesture++; this.target = { device: screen.device, screen };
    this.flight = null; this.wanted = this.sent = null; this.last = null; this.failure = null;
  }
  update(desired: Point) {
    if (!this.target) return;
    this.wanted = desired;
    if (!this.flight) this.flight = this.pump(this.gesture);
  }
  /** The reply for the final position, after every coalesced request settled. */
  async finish(): Promise<PreviewReply> {
    const gesture = this.gesture;
    while (this.flight && gesture === this.gesture) await this.flight;
    const final = this.wanted ?? this.sent;
    if (gesture !== this.gesture) throw new Error('cancelled');
    if (this.failure) throw this.failure;
    if (!this.last || !final || this.last.at.x !== final.x || this.last.at.y !== final.y) throw new Error('no preview');
    return this.last.reply;
  }
  cancel() { this.gesture++; this.target = null; this.flight = null; this.wanted = null; }
  end() { this.target = null; }
  private async pump(gesture: number) {
    while (this.wanted && gesture === this.gesture && this.target) {
      const at = this.wanted; this.wanted = null; this.sent = at;
      const sequence = String(++this.sequence);
      const ask = () => this.request({ sequence, expected_token: this.token(), device: this.target!.device, moved_screen: this.target!.screen, desired_canvas_origin: at });
      try {
        let reply: PreviewReply;
        try { reply = await ask(); } catch (error) {
          if (!(error instanceof LayoutProfileFailure) || error.problem.code !== 'stale_state') throw error;
          reply = await ask();
        }
        if (gesture !== this.gesture || reply.sequence !== sequence) continue;
        this.failure = null; this.last = { at, reply }; this.onReply(reply);
      } catch (error) {
        if (gesture === this.gesture) { this.failure = error; this.last = null; }
      }
    }
    if (gesture === this.gesture) this.flight = null;
  }
}
