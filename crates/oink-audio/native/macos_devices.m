// Metadata-only device discovery. Never construct an AudioUnit or query formats.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#include <stdatomic.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>

// Process lifetime registrations: callbacks have no borrowed context to outlive.
// On macOS 14.7.8/26.6.2, CPAL description() created temporary AudioUnits:
// 150 s A/B yielded 111/137/1 >=20 ms send gaps (normal/speaker polling off/all
// device polling off). Moving that work to another thread did not prevent HAL
// disruption. Listeners only increment an atomic and notify; the Rust cache worker reads
// IDs/names/stream direction after notification, startup, or explicit refresh.
_Static_assert(ATOMIC_LLONG_LOCK_FREE == 2, "device notification counter must be lock-free");
typedef void (*WakeCallback)(void);
static _Atomic(WakeCallback) wake_callback;
void oink_audio_devices_set_wake(WakeCallback wake) { atomic_store(&wake_callback, wake); }
void oink_audio_devices_wake(void) {
    WakeCallback wake = atomic_load(&wake_callback);
    if (wake) wake();
}
static atomic_uint_fast64_t generation = 1;
static atomic_uint_fast64_t service_generation = 1;
static uint64_t watched_service_generation = 1;
static pthread_once_t listener_once = PTHREAD_ONCE_INIT;
static OSStatus listener_status = noErr;
static NSMutableSet<NSNumber *> *watched;
static AudioObjectPropertyAddress system_properties[] = {
    {kAudioHardwarePropertyServiceRestarted, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
    {kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
    {kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
};
static AudioObjectPropertyAddress device_properties[] = {
    {kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
    {kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain},
};
static OSStatus changed(AudioObjectID object, UInt32 count,
                        const AudioObjectPropertyAddress addresses[], void *context) {
    (void)object; (void)count; (void)addresses; (void)context;
    for (UInt32 i = 0; i < count; ++i)
        if (addresses[i].mSelector == kAudioHardwarePropertyServiceRestarted)
            atomic_fetch_add_explicit(&service_generation, 1, memory_order_release);
    atomic_fetch_add_explicit(&generation, 1, memory_order_release);
    oink_audio_devices_wake();
    return noErr;
}
static void install_listeners(void) {
    for (size_t i = 0; i < sizeof(system_properties) / sizeof(system_properties[0]); ++i) {
        OSStatus result = AudioObjectAddPropertyListener(kAudioObjectSystemObject,
            &system_properties[i], changed, NULL);
        if (result != noErr && listener_status == noErr) listener_status = result;
    }
}
uint64_t oink_audio_devices_generation(void) {
    pthread_once(&listener_once, install_listeners);
    return atomic_load_explicit(&generation, memory_order_acquire);
}
uint64_t oink_audio_service_generation(void) {
    pthread_once(&listener_once, install_listeners);
    return atomic_load_explicit(&service_generation, memory_order_acquire);
}
void oink_audio_devices_invalidate(void) {
    atomic_fetch_add_explicit(&generation, 1, memory_order_release);
    oink_audio_devices_wake();
}
static NSString *text_property(AudioDeviceID device, AudioObjectPropertySelector selector) {
    AudioObjectPropertyAddress address = {selector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    CFStringRef value = NULL;
    UInt32 size = sizeof(value);
    if (AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &value) != noErr || !value) return nil;
    return CFBridgingRelease(value);
}
static bool has_streams(AudioDeviceID device, AudioObjectPropertyScope scope, OSStatus *status) {
    AudioObjectPropertyAddress address = {kAudioDevicePropertyStreams, scope, kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    *status = AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size);
    return *status == noErr && size >= sizeof(AudioStreamID);
}
// Serialized by the Rust shared cache mutex; no callback touches watched.
char *oink_audio_devices_snapshot(char *error, size_t capacity) {
    @autoreleasepool {
        oink_audio_devices_generation();
        uint64_t service = atomic_load_explicit(&service_generation, memory_order_acquire);
        if (service != watched_service_generation) {
            // AudioHardware.h requires listeners/caches to be re-established
            // after ServiceRestarted; IDs may be reused by the new service.
            for (NSNumber *device in watched)
                for (size_t i = 0; i < sizeof(device_properties) / sizeof(device_properties[0]); ++i)
                    AudioObjectRemovePropertyListener(device.unsignedIntValue, &device_properties[i], changed, NULL);
            [watched removeAllObjects];
            for (size_t i = 0; i < sizeof(system_properties) / sizeof(system_properties[0]); ++i)
                AudioObjectRemovePropertyListener(kAudioObjectSystemObject, &system_properties[i], changed, NULL);
            listener_status = noErr;
            install_listeners();
            watched_service_generation = service;
        }
        if (listener_status != noErr) {
            snprintf(error, capacity, "CoreAudio device notifications unavailable: %d", (int)listener_status);
            return NULL;
        }
        NSMutableData *storage = nil;
        NSMutableArray *devices = nil;
        NSMutableSet<NSNumber *> *next = nil;
        NSData *json = nil;
        char *result = NULL;
        AudioObjectPropertyAddress address = system_properties[1];
        UInt32 size = 0;
        OSStatus status = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &address, 0, NULL, &size);
        if (status != noErr) goto failed;
        storage = [NSMutableData dataWithLength:size];
        if (size) {
            status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, &size, storage.mutableBytes);
            if (status != noErr) goto failed;
        }
        AudioDeviceID default_output = kAudioObjectUnknown;
        UInt32 default_size = sizeof(default_output);
        status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &system_properties[2],
            0, NULL, &default_size, &default_output);
        if (status != noErr) goto failed;
        devices = [NSMutableArray array];
        next = [NSMutableSet set];
        AudioDeviceID *ids = storage.mutableBytes;
        for (size_t i = 0; i < size / sizeof(AudioDeviceID); ++i) {
            AudioDeviceID device = ids[i];
            NSString *uid = text_property(device, kAudioDevicePropertyDeviceUID);
            NSString *name = text_property(device, kAudioObjectPropertyName);
            if (!uid || !name) { status = kAudioHardwareBadDeviceError; goto failed; }
            bool output = has_streams(device, kAudioObjectPropertyScopeOutput, &status);
            if (status != noErr) goto failed;
            // C comparison expressions box as integers; emit JSON booleans for
            // the typed Rust bridge, including is_default.
            [devices addObject:@{ @"uid": uid, @"name": name,
                @"output": output ? @YES : @NO, @"is_default": device == default_output ? @YES : @NO }];
            [next addObject:@(device)];
        }
        if (!watched) watched = [NSMutableSet set];
        for (NSNumber *device in next) {
            if ([watched containsObject:device]) continue;
            for (size_t i = 0; i < sizeof(device_properties) / sizeof(device_properties[0]); ++i) {
                // Removed devices invalidate the next generation. Metadata
                // listener failure is surfaced rather than silently polling.
                status = AudioObjectAddPropertyListener(device.unsignedIntValue,
                    &device_properties[i], changed, NULL);
                if (status != noErr) {
                    for (size_t j = 0; j < i; ++j)
                        AudioObjectRemovePropertyListener(device.unsignedIntValue, &device_properties[j], changed, NULL);
                    goto failed;
                }
            }
            [watched addObject:device];
        }
        for (NSNumber *old in watched) {
            if ([next containsObject:old]) continue;
            for (size_t i = 0; i < sizeof(device_properties) / sizeof(device_properties[0]); ++i)
                AudioObjectRemovePropertyListener(old.unsignedIntValue, &device_properties[i], changed, NULL);
        }
        [watched intersectSet:next];
        json = [NSJSONSerialization dataWithJSONObject:devices options:0 error:nil];
        if (!json) { status = kAudioHardwareUnspecifiedError; goto failed; }
        result = malloc(json.length + 1);
        if (!result) { status = kAudioHardwareUnspecifiedError; goto failed; }
        memcpy(result, json.bytes, json.length); result[json.length] = 0;
        return result;
    failed:
        snprintf(error, capacity, "Read CoreAudio device metadata: %d", (int)status);
        return NULL;
    }
}
void oink_audio_devices_free(char *value) { free(value); }
