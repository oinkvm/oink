import {expect,it,vi} from 'vitest';
import {api} from './client';
import {transport} from './transport';
it('forwards restart only on explicit API invocation',async()=>{
 const call=vi.spyOn(transport,'call').mockResolvedValue(null);
 expect(call).not.toHaveBeenCalled();
 await api.restartSystem();
 expect(call).toHaveBeenCalledExactlyOnceWith({method:'restart_system'});
});
