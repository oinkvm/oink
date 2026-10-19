# Oink desktop interface

The React interface uses [oink-ui](https://github.com/oinkvm/oink-ui) components.
The Tauri shell provides the main window, macOS menu bar, and Windows tray and
window menus. It starts the backend in a separate process and communicates through
`AgentApi` (`Request`, `Reply`, and `ApiEvent`) over local IPC. The backend owns
device discovery, pairing, input routing, clipboard sharing, and persisted state.

| Part | Location |
|---|---|
| React views, localization, and interface state | `src/` |
| Tauri windows, native menus, and backend process lifecycle | `src-tauri/` |
| Backend service and shared API | `../../crates/oink-agent/` |

From the repository root:

```bash
npm ci
npm run -w apps/oink dev        # Browser preview: http://127.0.0.1:1440
npm run -w apps/oink typecheck
npm run -w apps/oink test
npm run -w apps/oink build
```

Browser previews render the interface; native capabilities require the Tauri shell.
Use oink-ui components and `--ok-*` theme variables for shared styling, and add
interface text through `src/i18n/` for all supported languages.
