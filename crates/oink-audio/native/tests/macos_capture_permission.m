// Exercise the actual dynamic bridge with fake TCC providers. No HAL/TCC calls.
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdbool.h>
#include <assert.h>
static uint64_t current_right;
static bool missing;
static bool load_failed;
static bool exception;
static unsigned requests;
static void verify_service(CFStringRef service) {
    assert(CFEqual(service, CFSTR("kTCCServiceAudioCapture")));
}
static uint64_t fake_preflight(CFStringRef service) {
    verify_service(service);
    if (exception) @throw [NSException exceptionWithName:@"FakeTCCFailure" reason:nil userInfo:nil];
    return current_right;
}
static void fake_request(CFStringRef service, CFDictionaryRef options, void (^callback)(uint64_t)) {
    verify_service(service); assert(!options); requests++;
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0), ^{ callback(current_right); });
}
static void *fake_open(const char *path, int flags) { (void)path; (void)flags; return load_failed ? NULL : (void *)1; }
static void *fake_symbol(void *handle, const char *name) {
    assert(handle == (void *)1);
    if (missing) return NULL;
    if (!strcmp(name, "tcc_authorization_preflight")) return (void *)fake_preflight;
    if (!strcmp(name, "tcc_authorization_request")) return (void *)fake_request;
    assert(false && "wrong SPI or legacy enum"); return NULL;
}
static char *fake_error(void) { return "fake loader failure"; }
#define dlopen fake_open
#define dlsym fake_symbol
#define dlerror fake_error
#include "../capture_permission.m"
static dispatch_semaphore_t completed;
static uint64_t decision;
static void callback(uint64_t result) { decision = result; dispatch_semaphore_signal(completed); }
int main(void) { @autoreleasepool {
    char error[256] = {0}; uint64_t result;
    completed = dispatch_semaphore_create(0);
    // Distinguish the current authorization right from the reversed legacy enum.
    for (unsigned pass = 0; pass < 2; pass++) {
        for (uint64_t right = 0; right < 3; right++) {
            current_right = right;
            assert(oink_audio_capture_preflight("tcc_authorization_preflight", &result, error, sizeof(error)));
            assert(result == right);
            assert(requests == pass * 3 + right);
            assert(oink_audio_capture_request(callback) == 0);
            assert(dispatch_semaphore_wait(completed, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC)) == 0);
            assert(decision == result);
        }
    }
    current_right = 3;
    assert(oink_audio_capture_preflight("tcc_authorization_preflight", &result, error, sizeof(error)));
    assert(result == 3);
    assert(oink_audio_capture_request(callback) == 0);
    assert(dispatch_semaphore_wait(completed, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC)) == 0);
    assert(decision == 3);
    exception = true;
    assert(!oink_audio_capture_preflight("tcc_authorization_preflight", &result, error, sizeof(error)));
    exception = false; missing = true;
    assert(!oink_audio_capture_preflight("tcc_authorization_preflight", &result, error, sizeof(error)));
    assert(oink_audio_capture_request(callback) == -1);
    missing = false; load_failed = true;
    oink_tcc_once = 0; oink_tcc_handle = NULL; // Isolated fake loader, no pending request.
    assert(!oink_audio_capture_preflight("tcc_authorization_preflight", &result, error, sizeof(error)));
    assert(oink_audio_capture_request(callback) == -1);
    puts("PASS: live self-authority mapping, grant/revoke/regrant, async request decisions, SPI absence/errors; fake providers, no TCC or audio IO");
} return 0; }
