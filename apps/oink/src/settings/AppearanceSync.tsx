import { useEffect } from 'react';
import { useOink, type Accent } from 'oink-ui';
import type { LocalPreferences, PresentationPreferences } from './model';
import { ACCENT_SWATCHES } from './appearance';
export function AppearanceSync({ local, presentation }: {local:LocalPreferences|null;presentation:PresentationPreferences}) {
  const {resolvedTheme,setTheme,setAccent,setMotion} = useOink();
  useEffect(()=>{
    if(!local)return;
    setTheme(local.theme_mode);
    const preset=ACCENT_SWATCHES.find(p=>p.color.toLowerCase()===local.theme_accent.light.toLowerCase()&&p.dark.toLowerCase()===local.theme_accent.dark.toLowerCase());
    setAccent(preset?.value??local.theme_accent[resolvedTheme] as Accent);
  },[local,resolvedTheme,setTheme,setAccent]);
  useEffect(()=>setMotion(presentation.motion),[presentation.motion,setMotion]);
  return null;
}
