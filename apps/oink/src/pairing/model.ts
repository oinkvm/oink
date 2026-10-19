import type { FoundPeer, PairProblem, PeerView, SocketAddress, PairingEvent } from '../api/pairing';
import { PairFailure, pairProblem, pairingApi } from '../api/pairing';
import { fingerprint } from '../api/identity';
import { problemText, tr } from '../i18n';
import type { MessageKey } from '../i18n';
import type { Peer } from './data';

export const addressText = (address:SocketAddress) => address.family===0 && address.address.length===4 ? address.address.join('.') : '';
export function presentPeer(peer:PeerView):Peer {
  return {compatible:true,id:peer.device,device:peer.device,deviceId:fingerprint(peer.device),name:peer.name,os:peer.os==='macos'?'mac':peer.os==='windows'?'win':peer.os==='linux'?'linux':'other',ip:addressText(peer.remote),port:peer.remote.port};
}
export { PairFailure } from '../api/pairing';
export function pairProblemText(problem:Pick<PairProblem,'code'|'network_problem'>):string {
  if(problem.network_problem)return problemText(problem.network_problem);
  const messages:Record<number,MessageKey>={
    0:'请输入有效的 IPv4 地址',1:'端口应在 1–65535 之间',3:'不能与当前设备配对',
    4:'目标主机不可达',5:'连接被拒绝',6:'连接超时',7:'该地址上没有 Oink 设备',
    8:'设备身份已变化，请重新搜索',9:'对方的协议版本不兼容',10:'对方不支持所需功能',
    11:'设备正忙，请稍后重试',12:'操作过于频繁，请稍后重试',13:'对方拒绝了配对',
    14:'对方取消了配对',15:'已取消配对',16:'对方关闭了配对',17:'连接已断开',
    18:'请输入六位配对码',19:'配对码不正确，请重新输入',20:'尝试次数已用完',
    21:'此配对已结束',22:'设备已经连接',23:'已由另一配对会话接替',24:'设备组已变化',
    25:'设备数量已达上限',26:'设备组数据超过限制',27:'无法验证设备组成员',28:'设备组合并冲突',
    30:'无法保存配对结果',31:'无法确认提交结果',32:'无法获取合并证明',33:'设备成员资格已撤销',
    34:'设备组已变化，无法撤销',35:'不允许此操作',36:'搜索结果的连接已断开，请重新搜索',
    37:'配对状态已变化，请重试',38:'配对协议异常',39:'设备状态已变化，请重新搜索',40:'配对已经提交',
    41:'无法验证设备身份',42:'没有共同支持的加密方式',43:'不支持该屏幕排列',44:'配对服务不可用',
    45:'网络操作超时',46:'对方等待处理的请求过多，请稍后重试',
  };
  return tr(messages[problem.code]??'配对状态未知');
}

/** One manual dialog owns a generation and a prepared channel lease. */
export class AddressQueryOwner {
  private generation=0n;
  private prepared:FoundPeer|null=null;
  private requests=new Map<string,{generation:string;resolve:(peer:FoundPeer)=>void;reject:(error:Error)=>void}>();
  constructor(private api:Pick<typeof pairingApi,'query'|'release'>=pairingApi) {}
  receive(event:PairingEvent) {
    if(event.type!=='address_query_finished')return;
    const request=this.requests.get(event.data.operation_id);
    if(!request||request.generation!==event.data.generation)return;
    this.requests.delete(event.data.operation_id);
    const result=event.data.result;
    if(event.data.generation!==this.generation.toString()) {
      if(result.type==='found')void this.api.release(result.peer.query_id,result.peer.generation).catch(()=>{});
      return;
    }
    if(result.type==='problem')request.reject(new PairFailure(result.problem));
    else if(!result.peer.prepared)request.reject(new PairFailure(pairProblem(36)));
    else {this.prepared=result.peer;request.resolve(result.peer);}
  }
  async release() { const peer=this.prepared;this.prepared=null;if(peer)await this.api.release(peer.query_id,peer.generation); }
  close() {this.generation++;for(const request of this.requests.values())request.reject(new Error('cancelled'));return this.release();}
  get(peer:string):FoundPeer { if(this.prepared?.peer.device!==peer)throw new PairFailure(pairProblem(36));return this.prepared; }
  consume(query:string) {if(this.prepared?.query_id===query)this.prepared=null;}
  async search(ip:string,port:number):Promise<FoundPeer> {
    this.generation++;for(const request of this.requests.values())request.reject(new Error('cancelled'));await this.release();
    const generation=this.generation.toString(),operation=crypto.randomUUID();
    return new Promise((resolve,reject)=>{
      this.requests.set(operation,{generation,resolve,reject});
      void this.api.query(operation,generation,ip,port).catch(error=>{this.requests.delete(operation);reject(error);});
    });
  }
}
