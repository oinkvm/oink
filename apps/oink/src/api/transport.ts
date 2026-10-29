// The product UI calls its native shell over Tauri IPC.
import type { ApiError, UserFacingProblem, UpdateError } from './cards';

export class ApiFailure extends Error {
  code: ApiError['code'];
  problem: UserFacingProblem;
  updateError?: UpdateError;
  constructor(err: ApiError) {
    super(err.problem.code);
    this.code = err.code;
    this.problem = err.problem;
    this.updateError = err.update_error;
  }
}

export function toFailure(e: unknown): ApiFailure {
  if (e instanceof ApiFailure) return e;
  if (e && typeof e === 'object' && 'problem' in e && 'code' in e) return new ApiFailure(e as ApiError);
  return new ApiFailure({ code: 'internal', problem: {code:'backend_failed',unknownCode:null,nativeCode:null,retryAfterMs:null} });
}

export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export interface Transport {
  call<R>(request: { method: string; params?: Record<string, unknown> }): Promise<R>;
  shell<R>(command: string, args?: Record<string, unknown>): Promise<R>;
  onEvent(handler: (e: import('./store').ProductEvent) => void): Promise<() => void>;
}

const tauri: Transport = {
  async call(request) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("api_call", { request });
  },
  async shell(command, args) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke(command, args);
  },
  async onEvent(handler) {
    const { listen } = await import('@tauri-apps/api/event');
    return listen<import('./store').ProductEvent>('agent-event', e => handler(e.payload));
  },
};

export const transport: Transport = tauri;
