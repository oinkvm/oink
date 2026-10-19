// Audio-only TCC observation and explicit request. Never opens recording IO.
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdbool.h>
#include <stdint.h>

// TCC may retain asynchronous completion blocks; keep the framework loaded for
// the process lifetime instead of unloading code that owns an outstanding alert.
static void *oink_tcc_handle;
static dispatch_once_t oink_tcc_once;
static void *tcc_library(void) {
    dispatch_once(&oink_tcc_once, ^{
        oink_tcc_handle = dlopen("/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC",
                                 RTLD_NOW | RTLD_LOCAL);
    });
    return oink_tcc_handle;
}

// There is no public audio-only preflight. The legacy TCCAccessPreflight bridge
// returned unknown forever in the installed signed host, including after an
// Audio Only grant. Query the current self-authorization path instead. Its
// prompt policy is preflight-only; it uses the calling process's credential,
// not another process's audit token (which needs private manager entitlements).
// This SPI returns an authorization right (0 denied, 1 unknown, 2 allowed),
// NOT the legacy preflight enum (0 allowed, 1 denied, 2 unknown). Symbols resolve
// on 14.7 and 26; signed-host grant/revocation acceptance is an install test.
// Missing SPI stays unavailable; never use Screen Recording as an audio proxy,
// never open a tap to poll, and never persist a presumed grant.
bool oink_audio_capture_preflight(const char *symbol, uint64_t *result,
                                  char *error, size_t error_size) {
    void *handle = tcc_library();
    if (!handle) {
        snprintf(error, error_size, "TCC dlopen failed: %s", dlerror());
        return false;
    }
    uint64_t (*preflight)(CFStringRef) = dlsym(handle, symbol);
    if (!preflight) {
        snprintf(error, error_size, "TCC dlsym failed: %s", dlerror());
        return false;
    }
    bool ok = true;
    @try {
        *result = preflight(CFSTR("kTCCServiceAudioCapture"));
    } @catch (NSException *exception) {
        snprintf(error, error_size, "TCC preflight exception: %s", exception.name.UTF8String);
        ok = false;
    }
    return ok;
}

// Only an explicit Grant action enters this path in the responsible backend.
// AudioDeviceStart returning noErr reports HAL setup, not authorization. This
// request uses the same self-authorization service as the preflight. Its async
// completion is an authorization right, not a Boolean. Do not block the IPC
// worker for the lifetime of a user dialog or mistake submission for a grant.
int32_t oink_audio_capture_request(void (*completion)(uint64_t)) {
    void *handle = tcc_library();
    if (!handle) return -1;
    void (*request)(CFStringRef, CFDictionaryRef, void (^)(uint64_t)) = dlsym(handle, "tcc_authorization_request");
    if (!request) return -1;
    request(CFSTR("kTCCServiceAudioCapture"), NULL, ^(uint64_t right) {
        completion(right);
    });
    return 0;
}
