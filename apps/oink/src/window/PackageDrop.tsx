import { useEffect, useRef, useSyncExternalStore } from 'react';
import { Icon } from 'oink-ui';
import { inTauri } from '../api/transport';
import { tr } from '../i18n';
import type { Platform } from './DesktopWindow';
let active=false;
const listeners=new Set<()=>void>();
const subscribe=(listener:()=>void)=>{listeners.add(listener);return()=>{listeners.delete(listener);};};
export function setPackageDrag(value:boolean) {if(active===value)return;active=value;listeners.forEach(listener=>listener());}
export const usePackageDrag=()=>useSyncExternalStore(subscribe,()=>active);
export function localPackagePath(paths:readonly string[],platform:Platform|'any'='any'):string|null {
  const extension=platform==='mac'?/\.dmg$/i:platform==='win'?/\.msi$/i:/\.(dmg|msi)$/i;
  return paths.length===1&&extension.test(paths[0])?paths[0]:null;
}
/** Enter supplies filenames on Wry's filesystem path. Over supplies positions
 * only, so retain Enter's decision; an empty list means unavailable names. */
export function packageDragAllowed(paths:readonly string[]|null,platform:Platform):boolean {
  return paths===null||paths.length===0||localPackagePath(paths,platform)!==null;
}
export function usePackageDropReceiver(platform:Platform,receive:(path:string)=>void) {
  const receiver=useRef(receive);receiver.current=receive;
  useEffect(()=>{
    if(!inTauri)return;
    let disposed=false;const stops:(()=>void)[]=[];
    const drop=(paths:readonly string[])=>{setPackageDrag(false);const path=localPackagePath(paths,platform);if(path)receiver.current(path);};
    void import('@tauri-apps/api/event').then(async({listen})=>{
      for(const stop of await Promise.all([
        listen<string>('local-package-drop',event=>drop([event.payload])),
        listen<boolean>('local-package-drag',event=>setPackageDrag(event.payload)),
      ])) {if(disposed)stop();else stops.push(stop);}
    }).catch(()=>{});
    void import('@tauri-apps/api/webview').then(async({getCurrentWebview})=>{
      const stop=await getCurrentWebview().onDragDropEvent(event=>{
        const payload=event.payload;
        if(payload.type==='enter')setPackageDrag(packageDragAllowed(payload.paths,platform));
        else if(payload.type==='leave')setPackageDrag(false);
        else if(payload.type==='drop')drop(payload.paths);
      });
      if(disposed)stop();else stops.push(stop);
    }).catch(()=>{});
    return()=>{disposed=true;stops.forEach(stop=>stop());setPackageDrag(false);};
  },[platform]);
}
export function PackageDropArea({platform}:{platform:Platform}) {
  return <div className="package-drop" data-platform={platform}><div className="package-drop__area" role="region" aria-label={tr('放下安装包以更新 Oink')}>
    <Icon name="download"/><strong>{tr('放下安装包以更新 Oink')}</strong>
    <span>{tr(platform==='mac'?'支持 Oink 的 .dmg 安装包':'支持 Oink 的 .msi 安装包')}</span>
  </div></div>;
}
