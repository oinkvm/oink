// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';
import { AddressQueryOwner, pairProblemText } from './model';
import type { FoundPeer, PairingEvent } from '../api/pairing';
import { setLanguage } from '../i18n';
const found=(generation:string):FoundPeer=>({query_id:'query',generation,prepared:true,peer:{device:'2'.repeat(64),name:'Device',os:'windows',listen:null,remote:{family:0,address:[192,0,2,4],port:47470,interface_scope:0}}});
describe('manual prepared-channel ownership',()=>{
  const setup=()=>{
    const api={query:vi.fn(async(_id:string,_generation:string,_ip:string,_port:number)=>({type:'accepted'} as const)),release:vi.fn(async()=>null)};
    return {api,owner:new AddressQueryOwner(api)};
  };
  const result=(operation:string,generation:string):PairingEvent=>({type:'address_query_finished',data:{operation_id:operation,generation,result:{type:'found',peer:found(generation)}}});
  it('resolves only its correlated query and retains the lease until start or close',async()=>{
    const {api,owner}=setup(),pending=owner.search('192.0.2.4',47470);await Promise.resolve();
    const [operation,generation]=api.query.mock.calls[0];
    owner.receive(result('another operation',generation));
    owner.receive(result(operation,generation));
    expect((await pending).query_id).toBe('query');expect(owner.get('2'.repeat(64)).prepared).toBe(true);
    owner.consume('query');owner.close();expect(api.release).not.toHaveBeenCalled();
  });
  it('releases a late result after the dialog closes without presenting or starting it',async()=>{
    const {api,owner}=setup(),pending=owner.search('192.0.2.4',47470);await Promise.resolve();
    const [operation,generation]=api.query.mock.calls[0];
    const rejection=expect(pending).rejects.toThrow('cancelled');owner.close();await rejection;
    owner.receive(result(operation,generation));expect(api.release).toHaveBeenCalledWith('query',generation);
    expect(()=>owner.get('2'.repeat(64))).toThrow();
  });
  it('translates numeric failures and unknown codes without backend prose',()=>{
    setLanguage('en');expect(pairProblemText({code:36})).toContain('search again');expect(pairProblemText({code:999})).toBe('Pairing state unavailable');
    setLanguage('zh-Hans');expect(pairProblemText({code:36})).toContain('重新搜索');
  });
});

it.each(['no_route','no_network_interface','peer_unreachable','socket_error','network_stopped'] as const)('keeps local network failure %s distinct from IPC failure',network_problem=>{
  for(const language of ['en','zh-Hans']) {
    setLanguage(language);
    const message=pairProblemText({code:4,network_problem});
    expect(message).not.toBe(pairProblemText({code:44}));
    expect(message).not.toBe(language==='en'?'Unavailable':'不可用');
  }
});
