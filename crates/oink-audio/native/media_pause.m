#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <stdbool.h>

bool oink_pause_media(void) {
    @autoreleasepool {
        // MPRemoteCommandCenter receives commands in a player; it cannot send a
        // cross-app pause. A synthetic media key is a toggle and can START audio.
        // MediaRemote's flat C SendCommand ABI and pause=1 have existed for years
        // (theos/headers MediaRemote.h); no private object layouts or per-version
        // offsets are used. Resolve at runtime, and fail closed if unavailable.
        // We do not read metadata or bypass macOS 15.4's metadata entitlement.
        // This targets the system Now Playing app, not all simultaneous players.
        // Verified directly with an AVPlayer + public MPRemoteCommandCenter on
        // macOS 26.6.2: playing->paused, repeated pause never starts playback.
        typedef Boolean (*SendCommand)(int, CFDictionaryRef);
        static SendCommand send;
        static dispatch_once_t once;
        dispatch_once(&once, ^{
            void *framework = dlopen("/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote", RTLD_LAZY | RTLD_LOCAL);
            if (framework) send = (SendCommand)dlsym(framework, "MRMediaRemoteSendCommand");
            // Retain the framework for process lifetime while the pointer is used.
        });
        return send ? send(1, NULL) : false;
    }
}
