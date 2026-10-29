import { ApiFailure } from '../api/transport';
import type { DeviceControlReply, DeviceControlResult } from '../api/cards';

export type ControlAction =
  | { kind:'card'; resource:'mouse'|'keyboard'|'clipboard'|'screen'|'game'|'pause'; desired:string|boolean|number }
  | { kind:'speaker'; mode:'unused'|'forwarding'|'exit' }
  | { kind:'microphone'; mode:'source'|'receiver'|'muted' }
  | ({ volume:number; muted:boolean; deferPersistence?:boolean; final?:boolean } & ({kind:'speakerVolume'; output:boolean}|{kind:'microphoneVolume'}));
export const controlResource = (action:ControlAction) => action.kind==='card' ? action.resource : action.kind;
const channel = (action:ControlAction) => controlResource(action).replace('Volume','');
export const CONTROL_TIMEOUT_MS = 10_000;
export const SEND_INTERVAL_MS = 40;
export interface ControlObservation {
  token:string|null;
  matches:boolean;
  volumeReadiness?:'ready'|'wait'|'drop';
  /** This command must finish a prerequisite before applying the final target. */
  intermediate?:boolean;
  /** Microphone claim revisions can arrive before the resolved mode projection. */
  microphoneMode?:'source'|'receiver'|'muted';
}
export interface ControlActionDeps {
  read:(device:string,action:ControlAction)=>ControlObservation;
  send:(id:string,device:string,action:ControlAction)=>Promise<DeviceControlReply>;
  refresh:()=>Promise<void>;
  show:(targets:ReadonlyMap<string,ControlAction[]>)=>void;
  fail:(reason:string,action:ControlAction)=>void;
}
interface Intent { device:string; action:ControlAction; force?:boolean; waiting?:boolean; deadline:ReturnType<typeof setTimeout> }
interface Flight { intent:Intent; id:string; before:ControlObservation; result?:DeviceControlResult }
interface Lane { queue:Intent[]; flight:Flight|null; spacing:ReturnType<typeof setTimeout>|null }

/** UI intent is independent of transport bookkeeping. Each resource shows the
 * newest choice over live device reports; removing it reveals the latest report.
 * Related mode/volume writes use fresh owner tokens in order, without disabling
 * controls. Other resources send independently. Every intent has a finite life. */
export class ControlActions {
  private lanes=new Map<string,Lane>();
  private shown=new Map<string,Intent>();
  private owned=new Set<string>();
  private lastVolumeSend=new Map<string,number>();
  constructor(private deps:ControlActionDeps) {}
  private resourceKey(device:string,action:ControlAction) { return `${device}:${controlResource(action)}`; }
  private laneKey(device:string,action:ControlAction) { return `${device}:${channel(action)}`; }
  set(device:string,action:ControlAction) {
    const key=this.laneKey(device,action), resource=this.resourceKey(device,action);
    let lane=this.lanes.get(key);
    if(!lane)this.lanes.set(key,lane={queue:[],flight:null,spacing:null});
    // Coalesce only unsent writes to the same resource. Keep mode/volume ordering
    // and the current request's token fence; neither controls the displayed value.
    for(const previous of lane.queue.filter(i=>this.resourceKey(i.device,i.action)===resource))clearTimeout(previous.deadline);
    lane.queue=lane.queue.filter(i=>this.resourceKey(i.device,i.action)!==resource);
    const intent:Intent={device,action,deadline:setTimeout(()=>this.expire(key,intent),CONTROL_TIMEOUT_MS)};
    this.shown.delete(resource);this.shown.set(resource,intent);
    lane.queue.push(intent);
    if((action.kind==='speakerVolume'||action.kind==='microphoneVolume')&&action.final&&lane.spacing){clearTimeout(lane.spacing);lane.spacing=null;}
    this.publish();this.pump(key,lane);
  }
  targets() {
    const targets=new Map<string,ControlAction[]>();
    for(const {device,action} of this.shown.values())targets.set(device,[...(targets.get(device)??[]),action]);
    return targets;
  }
  /** Claim duplicate/late receipts too; they must not raise an obsolete toast. */
  complete(id:string,result:DeviceControlResult) {
    if(!this.owned.has(id))return false;
    for(const [key,lane] of this.lanes)if(lane.flight?.id===id&&!lane.flight.result){
      const flight=lane.flight;
      flight.result=result;
      if(result.outcome==='failed'||result.outcome==='refused') {
        if(result.reason!=='stale_revision') {
          this.forceSuccessor(lane,flight.intent);
          this.retire(flight.intent,result.reason);
          lane.flight=null;
          this.pump(key,lane);
        }
      }
      this.snapshot();
      void this.deps.refresh().then(()=>this.snapshot(),()=>{
        // A supplementary query cannot invalidate an owner success receipt.
        // Live subscribed facts can still settle it under the original deadline.
        if(result.outcome==='applied'||result.outcome==='no_change')return;
        // A failed refresh cannot erase a newer request/intent on this lane.
        if(this.lanes.get(key)===lane&&lane.flight===flight){
          this.forceSuccessor(lane,flight.intent);
          this.retire(flight.intent,'outcome_unknown');lane.flight=null;this.pump(key,lane);
        }
      });
      break;
    }
    return true;
  }
  snapshot() {
    for(const [key,lane] of this.lanes) {
      const flight=lane.flight;
      if(flight?.result) {
        const now=this.deps.read(flight.intent.device,flight.intent.action);
        const latest=this.shown.get(this.resourceKey(flight.intent.device,flight.intent.action));
        const superseded=latest!==flight.intent;
        const result=flight.result;
        const refused=result.outcome==='failed'||result.outcome==='refused';
        const fresh=now.token!==flight.before.token;
        // A published claim revision is not a completed microphone transition:
        // the audio owner's resolved mode can still lag behind that revision.
        // Advancing here can release an already-released source again (NotExit).
        const transitioned=fresh&&(flight.before.microphoneMode===undefined||now.microphoneMode!==flight.before.microphoneMode);
        if(refused) {
          // Concurrent resources/external changes can spend a token. Rebase the
          // latest intent on fresh facts, bounded by its original deadline.
          if(fresh){lane.flight=null;if(superseded)this.retire(flight.intent);else lane.queue.unshift(flight.intent);}
        } else if(flight.before.intermediate&&!superseded&&transitioned) {
          // Release may look like Local when the virtual device is absent, but
          // receiving is still enabled. Finish the explicit disable step.
          lane.flight=null;flight.intent.force=true;lane.queue.unshift(flight.intent);
        } else if(now.matches || (superseded && (transitioned||(result.outcome==='no_change'&&flight.before.microphoneMode===undefined)))) {
          this.retire(flight.intent);lane.flight=null;
        }
      }
      this.pump(key,lane);
    }
  }
  clear() {this.dispose();this.publish();}
  dispose() {
    for(const lane of this.lanes.values()){
      if(lane.spacing)clearTimeout(lane.spacing);
      for(const intent of lane.queue)clearTimeout(intent.deadline);
      if(lane.flight)clearTimeout(lane.flight.intent.deadline);
    }
    this.lanes.clear();this.shown.clear();this.lastVolumeSend.clear();
  }
  private publish(){this.deps.show(this.targets());}
  private retire(intent:Intent,reason?:string) {
    clearTimeout(intent.deadline);
    const key=this.resourceKey(intent.device,intent.action);
    if(this.shown.get(key)===intent){
      this.shown.delete(key);this.publish();
      if(reason&&reason!=='volume_unsupported')this.deps.fail(reason,intent.action);
    }
  }
  private forceSuccessor(lane:Lane,intent:Intent) {
    for(const next of lane.queue)if(controlResource(next.action)===controlResource(intent.action)) {
      // A failed call may still have reached the owner. Send the newer explicit
      // setter even if our last report already matches. Pause tokens, unlike
      // setters, authorize only the opposite of their observed state.
      if(next.action.kind!=='card'||next.action.resource!=='pause')next.force=true;
    }
  }
  private expire(key:string,intent:Intent) {
    const lane=this.lanes.get(key);
    if(!lane)return;
    if(lane.flight?.intent===intent){this.forceSuccessor(lane,intent);lane.flight=null;}
    lane.queue=lane.queue.filter(i=>i!==intent);
    this.retire(intent,'outcome_unknown');
    this.pump(key,lane);
  }
  private reject(key:string,lane:Lane,id:string,error:unknown) {
    const flight=lane.flight;
    if(!flight||flight.id!==id)return;
    if(flight.intent.action.kind!=='microphone'&&flight.intent.action.kind!=='microphoneVolume'){
      this.complete(id,{outcome:'failed',reason:'outcome_unknown',unknownCode:null});return;
    }
    this.forceSuccessor(lane,flight.intent);
    this.retire(flight.intent,error instanceof ApiFailure?error.problem.code:'outcome_unknown');
    lane.flight=null;this.pump(key,lane);
    void this.deps.refresh().then(()=>this.snapshot(),()=>{});
  }
  private pump(key:string,lane:Lane) {
    if(this.lanes.get(key)!==lane||lane.flight||lane.spacing)return;
    const intent=lane.queue[0];
    if(!intent){this.lanes.delete(key);return;}
    const action=intent.action;
    const before=this.deps.read(intent.device,action);
    if(before.matches&&!intent.force&&action.kind!=='speakerVolume'&&action.kind!=='microphoneVolume'){
      lane.queue.shift();this.retire(intent);this.pump(key,lane);return;
    }
    if(action.kind==='speakerVolume'||action.kind==='microphoneVolume') {
      const obsolete=lane.queue.slice(1).some(({action:next})=>action.kind==='microphoneVolume'
        ?next.kind==='microphone'&&next.mode!=='source'
        :next.kind==='speaker'&&(next.mode==='exit')!==action.output);
      if(before.volumeReadiness==='drop'||obsolete){
        lane.queue.shift();this.retire(intent);this.pump(key,lane);return;
      }
      if(before.volumeReadiness==='wait'){
        // Refresh once, then wait for published facts under the original deadline.
        // Retain the latest absolute setter, including durable drag release.
        if(!intent.waiting){intent.waiting=true;void this.deps.refresh().then(()=>this.snapshot(),()=>{});}
        return;
      }
      const wait=(this.lastVolumeSend.get(key)??-Infinity)+SEND_INTERVAL_MS-Date.now();
      if(wait>0&&!action.final){lane.spacing=setTimeout(()=>{lane.spacing=null;this.pump(key,lane);},wait);return;}
      this.lastVolumeSend.set(key,Date.now());
    }
    lane.queue.shift();
    intent.force=false;
    const id=crypto.randomUUID();
    this.owned.add(id);
    // Bound duplicate receipt history without coupling it to the current lane.
    if(this.owned.size>1024)this.owned.delete(this.owned.values().next().value!);
    lane.flight={intent,id,before};
    try {
      this.deps.send(id,intent.device,action).then(reply=>{
        if(reply.type==='device_control_completed')this.complete(id,reply.data.result);
      },error=>this.reject(key,lane,id,error));
    } catch(error) { this.reject(key,lane,id,error); }
  }
}
