import { beforeEach, describe, expect, it, vi } from 'vitest';
const {call}=vi.hoisted(()=>({call:vi.fn()}));
vi.mock('./client',()=>({call}));
import { pairingApi } from './pairing';
import { ApiFailure } from './transport';
beforeEach(()=>{call.mockReset();});
describe('pairing command outcomes',()=>{
  it('rejects a returned business problem rather than opening an unstarted pair',async()=>{
    call.mockResolvedValue({type:'problem',problem:{code:39,stage:1}});
    await expect(pairingApi.start('operation',{type:'nearby_device',device:'device'})).rejects.toMatchObject({problem:{code:39}});
  });
  it('maps native transport loss to the typed local-service failure',async()=>{
    call.mockRejectedValue(new ApiFailure({code:'unavailable',problem:{code:'local_service_unavailable',unknownCode:null,nativeCode:null,retryAfterMs:null}}));
    const error=await pairingApi.query('operation','1','192.0.2.1',47470).catch(error=>error);
    expect(error).toBeInstanceOf(ApiFailure);
    expect(error).toMatchObject({problem:{code:'local_service_unavailable'}});
  });
});

it('preserves backend validation failures instead of reporting an IPC disconnect',async()=>{
  const error=new ApiFailure({code:'invalid_address',problem:{code:'invalid_address',unknownCode:null,nativeCode:null,retryAfterMs:null}});
  call.mockRejectedValue(error);
  await expect(pairingApi.query('operation','1','invalid',47470)).rejects.toBe(error);
});
