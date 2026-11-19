import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import 'oink-ui/styles.css';
import './app.css';
import { StartupWindow } from './window/StartupPanel';
import { App } from './App';
import { agent, start, waitForStartup } from './api/store';
import { setLanguage } from './i18n';
const settingsPreview=import.meta.env.DEV && new URLSearchParams(location.search).get('fixture')==='settings';
const preview=import.meta.env.DEV && new URLSearchParams(location.search).get('fixture')==='screen-groups';
const hotkeyPreview=import.meta.env.DEV && new URLSearchParams(location.search).get('fixture')==='profile-hotkeys';
if(hotkeyPreview) {
 void import('./arrange/ProfileHotkeyPreview').then(({ProfileHotkeyPreview})=>createRoot(document.getElementById('root')!).render(<StrictMode><ProfileHotkeyPreview/></StrictMode>));
} else if(settingsPreview) {
 void import('./settings/SettingsPreview').then(({SettingsPreview})=>createRoot(document.getElementById('root')!).render(<StrictMode><SettingsPreview/></StrictMode>));
} else if(preview) {
 void import('./arrange/GroupPreview').then(({GroupPreview})=>createRoot(document.getElementById('root')!).render(<StrictMode><GroupPreview/></StrictMode>));
} else {
 // Resolve the route, platform and preferences before React's first render.
 // start() subscribes before querying, preserving events during initial load.
 const view = new URLSearchParams(location.search).get('view') ?? 'main';
 const root = createRoot(document.getElementById('root')!);
 void (async () => {
  if (view === 'main') await waitForStartup(raw => root.render(<StartupWindow raw={raw}/>));
  await start(view);
  const preferences = agent.get().localPreferences;
  if (preferences) setLanguage(preferences.ui_language);
  root.render(<StrictMode><App /></StrictMode>);
 })();
}
