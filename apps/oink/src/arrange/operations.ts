import { api } from '../api/client';
import { LayoutProfileFailure, type LayoutOperation, type OperationReply } from '../api/layoutProfiles';
import { agent } from '../api/store';

const terminal = (op: LayoutOperation | null | undefined): boolean =>
  !!op && (op.status === 'applied' || op.status === 'partial' || op.status === 'failed');

/**
 * Submit one accepted arrangement operation and wait for its terminal outcome.
 *
 * Progress events are collected before the request leaves, so a completion that
 * overtakes the reply is not lost. Each operation resolves once; later duplicate
 * events for it are ignored. A lost reply is resolved by querying the same request
 * ID, never by a second write under a new one. `outcome_unknown` is not terminal:
 * the backend reports the resolved status later on the same operation.
 * Refusals before acceptance reject with `LayoutProfileFailure`.
 */
export async function runOperation(start: (requestId: string) => Promise<OperationReply>, onUnknown?: (op: LayoutOperation) => void): Promise<LayoutOperation> {
  const requestId = crypto.randomUUID();
  const seen = new Map<string, LayoutOperation>();
  let wake = () => {};
  const stop = agent.onEvent(event => {
    if (event.type !== 'layout_profile_operation') return;
    seen.set(event.data.operation.id, event.data.operation);
    wake();
  });
  try {
    let id: string;
    try {
      const reply = await start(requestId);
      agent.adoptProfiles(reply.state);
      id = reply.operation_id;
      if (reply.state.operation?.id === id && !seen.has(id)) seen.set(id, reply.state.operation);
    } catch (error) {
      if (error instanceof LayoutProfileFailure) throw error;
      const found = await api.layoutProfileOperation(requestId).catch(() => { throw error; });
      agent.adoptProfiles(found.state);
      id = found.operation.id;
      if (!seen.has(id)) seen.set(id, found.operation);
    }
    let warned = false;
    return await new Promise<LayoutOperation>(resolve => {
      wake = () => {
        const op = seen.get(id);
        if (op && terminal(op)) resolve(op);
        else if (op?.status === 'outcome_unknown' && !warned) { warned = true; onUnknown?.(op); }
      };
      wake();
    });
  } finally { stop(); }
}
