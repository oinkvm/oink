declare const __OINK_RELEASE_VERSION__: string;

// Vite embeds the same release label as native boot_info, including before IPC connects.
export const version = __OINK_RELEASE_VERSION__;
