import {useState} from 'react';
import {Select, toast} from 'oink-ui';
import {api} from '../api/client';
import {agent,useAgent} from '../api/store';
import {tr} from '../i18n';

export function MicrophoneInput() {
  const {audio}=useAgent();
  const [pending,setPending]=useState(false);
  // The backend filters its current and legacy virtual endpoint identities.
  // Display names can be changed by the OS/user and are never the identity key.
  const inputs=audio?.microphone_inputs??[];
  const selected=audio?.microphone_input??null;
  const missing=selected!==null&&!inputs.some(input=>input.id===selected);
  const save=async(input:string|null)=>{
    setPending(true);
    try { await api.setMicrophoneInput(input); await agent.query('audio',api.audioStatus); }
    catch {toast.danger({title:tr('无法更改麦克风设置')});}
    finally {setPending(false);}
  };
  return <Select size="sm" aria-label={tr('共享的麦克风')} disabled={pending||!audio}
    value={selected??'default'} onChange={value=>{void save(value==='default'?null:value);}}
    options={[
      {value:'default',label:tr('跟随系统默认')},
      ...inputs.map(input=>({value:input.id,label:input.name})),
      ...(missing?[{value:selected!,label:tr('所选麦克风不可用'),disabled:true}]:[]),
    ]}/>;
}
