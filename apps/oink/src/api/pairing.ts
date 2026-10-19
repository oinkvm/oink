export type * from '../../../../crates/oink-agent/src/api/pairing';
import type * as P from '../../../../crates/oink-agent/src/api/pairing';
import { call } from './client';
export const pairProblem=(code:number,stage=0):P.PairProblem=>({code,stage,retry_after_ms:null,attempts_left:null,winner:null,needed:null,limit:null,protocol_code:null,limit_kind:null,cause:null});
let onboardingPolicy: Promise<unknown> = Promise.resolve();
export class PairFailure extends Error { constructor(readonly problem:P.PairProblem) {super(`PairProblem:${problem.code}`);} }
async function pairCall<R>(method:string,params?:Record<string,unknown>):Promise<R> {
  const reply=await call<R|{type:'problem';problem:P.PairProblem}>(method,params);
  if(reply && typeof reply==='object' && 'type' in reply && reply.type==='problem' && 'problem' in reply)throw new PairFailure(reply.problem as P.PairProblem);
  return reply as R;
}
export const pairingApi = {
  nearby: () => pairCall<P.NearbySnapshot>('nearby'),
  refresh: (operation_id: string) => pairCall<P.Accepted>('refresh_nearby',{operation_id}),
  pairings: () => pairCall<P.PairingSnapshot>('pairings'),
  onboardingConnect: (active: boolean): Promise<null> => {
    const result = onboardingPolicy.catch(() => {}).then(() => pairCall<null>('set_onboarding_connect', { active }));
    onboardingPolicy = result;
    return result;
  },
  query: (operation_id:string,generation:string,ip:string,port:number) => pairCall<P.Accepted>('query_pairing_address',{operation_id,generation,ip,port}),
  release: (query_id:string,generation:string) => pairCall<null>('release_pairing_query',{query_id,generation}),
  start: (operation_id:string,target:P.PairTarget) => pairCall<P.Started>('start_pairing',{operation_id,target}),
  accept: (pair_id:string,expected_revision:string) => pairCall<P.Accepted>('accept_pairing',{pair_id,expected_revision}),
  decline: (pair_id:string,expected_revision:string) => pairCall<P.Accepted>('decline_pairing',{pair_id,expected_revision}),
  code: (pair_id:string) => pairCall<{six_digits:string}>('pairing_code',{pair_id}),
  submit: (pair_id:string,expected_revision:string,attempt:number,code:string) => pairCall<P.Accepted>('submit_pairing_code',{pair_id,expected_revision,attempt,code}),
  cancel: (pair_id:string,expected_revision:string) => pairCall<P.CancelResult>('cancel_pairing',{pair_id,expected_revision}),
  undo: (operation_id:string,handle:P.UndoHandle) => pairCall<P.Accepted>('undo_pairing',{operation_id,handle}),
  subscribe: (after_event_id:string) => pairCall<P.PairingEvent[]>('subscribe_pairing_events',{after_event_id}),
  ack: (event_id:string) => pairCall<null>('ack_pairing_event',{event_id}),
};
// A reset creates a new device identity and restarts the journal's event IDs.
const cursorKey=(device:string)=>`oink:pairing-cursor:${device}`;
export function savePairingCursor(device:string,id:string):void {localStorage.setItem(cursorKey(device),id);}
export function pairingCursor(device:string):string { try { const value=localStorage.getItem(cursorKey(device)); return value && /^\d+$/.test(value) ? value : '0'; } catch { return '0'; } }
