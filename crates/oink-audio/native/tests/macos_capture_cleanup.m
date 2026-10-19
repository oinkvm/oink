// Offline fault injection only. Cleanup/property HAL calls are replaced by stubs.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#include <assert.h>
#include <stdlib.h>
#include <stdio.h>
static OSStatus test_stop(AudioObjectID, AudioDeviceIOProcID);
static OSStatus test_unregister(AudioObjectID, AudioDeviceIOProcID);
static OSStatus test_aggregate(AudioObjectID);
static OSStatus test_tap(AudioObjectID);
static OSStatus test_get(AudioObjectID, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32 *, void *);
static OSStatus test_set(AudioObjectID, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32, const void *);
static void test_free(void *);
#define AudioDeviceStop test_stop
#define AudioDeviceDestroyIOProcID test_unregister
#define AudioHardwareDestroyAggregateDevice test_aggregate
#define AudioHardwareDestroyProcessTap test_tap
#define AudioObjectGetPropertyData test_get
#define AudioObjectSetPropertyData test_set
#define free test_free
#include "../macos.m"
#undef free
static OSStatus outcomes[6];
static unsigned calls[6], frees;
static UInt32 current_buffer;
static OSStatus lookup_status;
static AudioObjectID tap_found, aggregate_found;
static void *tracked;
static OSStatus test_stop(AudioObjectID device, AudioDeviceIOProcID io) { calls[0]++; return outcomes[0]; }
static OSStatus test_unregister(AudioObjectID device, AudioDeviceIOProcID io) { calls[1]++; return outcomes[1]; }
static OSStatus test_aggregate(AudioObjectID device) { calls[2]++; return outcomes[2]; }
static OSStatus test_tap(AudioObjectID device) { calls[3]++; return outcomes[3]; }
static OSStatus test_get(AudioObjectID device, const AudioObjectPropertyAddress *a, UInt32 qsize, const void *q, UInt32 *size, void *data) {
    if (a->mSelector == kAudioHardwarePropertyTranslateUIDToTap || a->mSelector == kAudioHardwarePropertyTranslateUIDToDevice) {
        assert(device == kAudioObjectSystemObject && qsize == sizeof(CFStringRef) && q);
        *(AudioObjectID *)data = a->mSelector == kAudioHardwarePropertyTranslateUIDToTap ? tap_found : aggregate_found;
        *size = sizeof(AudioObjectID);
        return lookup_status;
    }
    assert(a->mSelector == kAudioDevicePropertyBufferFrameSize); calls[4]++;
    *(UInt32 *)data = current_buffer; *size = sizeof(UInt32); return outcomes[4];
}
static OSStatus test_set(AudioObjectID device, const AudioObjectPropertyAddress *a, UInt32 qsize, const void *q, UInt32 size, const void *data) {
    assert(a->mSelector == kAudioDevicePropertyBufferFrameSize); calls[5]++;
    if (!outcomes[5]) current_buffer = *(const UInt32 *)data; return outcomes[5];
}
static void test_free(void *p) { if (p == tracked) frees++; free(p); }
static OinkCapture *fixture(bool with_io) {
    memset(outcomes, 0, sizeof(outcomes)); memset(calls, 0, sizeof(calls)); frees = 0; current_buffer = 128;
    lookup_status = noErr; tap_found = aggregate_found = kAudioObjectUnknown;
    atomic_store(&capture_cleanup_failed, false); // Test-only reset; production has no reset API.
    OinkCapture *c = calloc(1, sizeof(*c)); tracked = c;
    c->service_epoch = atomic_load(&service_epoch);
    c->tap_uid = CFRetain(CFSTR("test-tap")); c->aggregate_uid = CFRetain(CFSTR("test-aggregate"));
    atomic_init(&c->closing, false); atomic_init(&c->write_pos, 0); atomic_init(&c->read_pos, 0);
    c->io = with_io ? (AudioDeviceIOProcID)capture_io : NULL; c->aggregate = 123; c->tap = 456;
    c->info.physical_device = 789; c->info.physical_lease_changed = true;
    c->info.physical_lease_buffer_frames = 128; c->info.physical_original_buffer_frames = 512;
    return c;
}
int main(void) {
    char error[1024];
    OinkCapture *c = fixture(true);
    assert(oink_audio_capture_stop(c, error, sizeof(error)) == 0);
    assert(frees == 1 && current_buffer == 512 && !oink_audio_capture_cleanup_failed());
    assert(calls[0] == 1 && calls[1] == 1 && error[0] == 0);
    // Simulates a registered IOProc after Start failed: started remains false.
    c = fixture(true); outcomes[0] = -11; outcomes[1] = -12; outcomes[2] = -13; outcomes[3] = -14;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) == -11);
    assert(!frees && current_buffer == 512 && oink_audio_capture_cleanup_failed());
    assert(strstr(error, "AudioDeviceStop") && strstr(error, "AudioDeviceDestroyIOProcID"));
    assert(strstr(error, "AudioHardwareDestroyAggregateDevice") && strstr(error, "AudioHardwareDestroyProcessTap"));
    assert(strstr(error, "intentionally retained"));
    assert(capture_io(0, NULL, NULL, NULL, NULL, NULL, c) == 0); // Orphaned callback safely exits.
    unsigned reads = calls[4]; double rate = 0;
    assert(oink_audio_capture_start(false, 128, 1, NULL, true, true, false, false, &rate, error, sizeof(error)) == NULL);
    assert(calls[4] == reads && strstr(error, "Capture disabled after cleanup failure"));
    free(c); // Only this test owns/proves absence of a real callback.
    c = fixture(true); outcomes[1] = -12;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) == -12 && !frees); free(c);
    c = fixture(true); outcomes[3] = -14;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) == -14 && frees == 1);
    assert(oink_audio_capture_cleanup_failed());
    c = fixture(false); outcomes[5] = -15;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) == -15 && frees == 1);
    assert(oink_audio_capture_cleanup_failed() && strstr(error, "Restore physical buffer lease"));
    c = fixture(false); current_buffer = 256;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) == 0 && !calls[5] && current_buffer == 256);
    c = fixture(false); outcomes[4] = -16;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) == -16 && !calls[5]);
    assert(oink_audio_capture_cleanup_failed());
    // Reset evidence prevents all old-ID writes/destruction and keeps late
    // callback storage alive. Repeated resets must not depend on numeric IDs.
    for (unsigned i = 0; i < 2; i++) {
        c = fixture(true);
        service_restarted(0, 0, NULL, NULL);
        assert(oink_audio_capture_check(c, error, sizeof(error)) == 2);
        assert(oink_audio_capture_stop(c, error, sizeof(error)) == noErr);
        assert(!frees && !oink_audio_capture_cleanup_failed());
        for (unsigned j = 0; j < 6; j++) assert(calls[j] == 0);
        assert(capture_io(0, NULL, NULL, NULL, NULL, NULL, c) == noErr);
        free(c);
    }
    // Notification alone is insufficient: surviving either UID or a failed
    // fresh query leaves the conservative process latch set.
    for (unsigned i = 0; i < 3; i++) {
        c = fixture(true); service_restarted(0, 0, NULL, NULL);
        if (i == 0) tap_found = 456;
        if (i == 1) aggregate_found = 123;
        if (i == 2) lookup_status = kAudioHardwareUnknownPropertyError;
        assert(oink_audio_capture_stop(c, error, sizeof(error)) != noErr);
        assert(!frees && oink_audio_capture_cleanup_failed()); free(c);
    }
    // Notification delayed: exclusively !dev/!obj + both fresh UID lookups
    // absent prove release. Mixed failures are never excused by those lookups.
    c = fixture(true);
    outcomes[0] = outcomes[1] = kAudioHardwareBadDeviceError;
    outcomes[4] = kAudioHardwareBadObjectError;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) == noErr);
    assert(!frees && !oink_audio_capture_cleanup_failed()); free(c);
    c = fixture(true);
    outcomes[0] = kAudioHardwareBadDeviceError; outcomes[3] = -14;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) != noErr);
    assert(oink_audio_capture_cleanup_failed()); free(c);
    c = fixture(true); outcomes[0] = kAudioHardwareBadDeviceError; tap_found = 456;
    assert(oink_audio_capture_stop(c, error, sizeof(error)) != noErr);
    assert(oink_audio_capture_cleanup_failed()); free(c);
    puts("15 synthetic cleanup scenarios passed; no live device/tap/IOProc created");
}
