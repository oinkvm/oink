// Endpoint controls only. Never writes volume, selects a device, or captures PCM.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#include <stdatomic.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>

static AudioObjectPropertyAddress controls = {kAudioObjectPropertySelectorWildcard,
    kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementWildcard};
static AudioObjectPropertyAddress defaults = {kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
static AudioObjectPropertyAddress service = {kAudioHardwarePropertyServiceRestarted,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};

@interface OinkMuteObserver : NSObject {
@public
    AudioDeviceID device, initial_default;
    NSString *uid;
    dispatch_queue_t queue;
    AudioObjectPropertyListenerBlock listener;
    void (*wake_owner)(void *);
    void *wake_context;
    bool subscribed[3];
    float initial_volume;
    _Atomic bool expected_mute;
    _Atomic bool revoked, mute_revoked, default_changed, service_changed, closing;
}
@end
@implementation OinkMuteObserver
@end

static void observe_controls(OinkMuteObserver *o, float volume, bool muted) {
    if (volume != o->initial_volume) atomic_store(&o->revoked, true);
    if (muted != atomic_load(&o->expected_mute)) {
        atomic_store(&o->mute_revoked, true);
        atomic_store(&o->revoked, true);
    }
}

static OSStatus endpoint_lookup(NSString *uid, AudioDeviceID *device) {
    CFStringRef value = (__bridge CFStringRef)uid;
    AudioObjectPropertyAddress a = {kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(*device);
    *device = kAudioObjectUnknown;
    return AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, sizeof(value), &value, &size, device);
}
static OSStatus endpoint_controls(AudioDeviceID device, float *volume, bool *muted) {
    AudioObjectPropertyAddress a = {kAudioDevicePropertyVolumeScalar,
        kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(*volume);
    OSStatus status = AudioObjectGetPropertyData(device, &a, 0, NULL, &size, volume);
    if (status) return status;
    a.mSelector = kAudioDevicePropertyMute;
    UInt32 mute = 0; size = sizeof(mute);
    status = AudioObjectGetPropertyData(device, &a, 0, NULL, &size, &mute);
    *muted = mute != 0;
    return status;
}
static OSStatus endpoint_default(AudioDeviceID *device) {
    UInt32 size = sizeof(*device);
    return AudioObjectGetPropertyData(kAudioObjectSystemObject, &defaults, 0, NULL, &size, device);
}

// Read only: absence of a UID does not prove permanent removal. CoreAudio may
// retain hardware mute preferences under that UID for a later reconnect.
int oink_audio_endpoint_exists(const char *uid, char *error, size_t capacity) {
    @autoreleasepool {
        NSString *value = uid ? [NSString stringWithUTF8String:uid] : nil;
        if (!value) { snprintf(error, capacity, "Invalid source endpoint UID"); return -1; }
        AudioDeviceID device = kAudioObjectUnknown;
        OSStatus status = endpoint_lookup(value, &device);
        if (status) { snprintf(error, capacity, "Look up source endpoint: Core Audio status %d", (int)status); return -1; }
        return device != kAudioObjectUnknown;
    }
}

// 1 exact endpoint exists, 0 absent/reset, -1 query error. IDs are never persisted
// or reused after service reset: journal identity is the stable device UID.
int oink_audio_endpoint_read(void *opaque, float *volume, bool *muted, char *error, size_t capacity) {
    OinkMuteObserver *o = (__bridge OinkMuteObserver *)opaque;
    if (atomic_load(&o->service_changed)) return 0;
    AudioDeviceID current = kAudioObjectUnknown;
    OSStatus status = endpoint_lookup(o->uid, &current);
    if (!current || current != o->device) return 0;
    if (!status) status = endpoint_controls(current, volume, muted);
    if (status) { snprintf(error, capacity, "Read source endpoint controls: Core Audio status %d", (int)status); return -1; }
    // Readback closes notification-delivery races during startup/restoration.
    // No level is ever written, including after this ownership loss.
    observe_controls(o, *volume, *muted);
    return 1;
}

void oink_audio_endpoint_close(void *opaque) {
    if (!opaque) return;
    OinkMuteObserver *o = CFBridgingRelease(opaque);
    atomic_store(&o->closing, true);
    const AudioObjectPropertyAddress *addresses[] = {&controls, &defaults, &service};
    const AudioObjectID objects[] = {o->device, kAudioObjectSystemObject, kAudioObjectSystemObject};
    bool failed = false;
    for (unsigned i = 0; i < 3; ++i)
        if (o->subscribed[i] && AudioObjectRemovePropertyListenerBlock(objects[i], addresses[i], o->queue, o->listener))
            failed = true;
    dispatch_sync(o->queue, ^{}); // Retire already queued notifications outside their queue.
    // HAL retains its registered block. It holds only a weak observer reference,
    // so even failed deregistration cannot leave a dangling callback context.
    if (!failed) o->listener = nil;
}

// Registration and callbacks share the serial notification queue. Close drains
// it before Rust drops the retained Thread context, including failed removal.
void oink_audio_endpoint_on_change(void *opaque, void (*wake)(void *), void *context) {
    OinkMuteObserver *o = (__bridge OinkMuteObserver *)opaque;
    dispatch_sync(o->queue, ^{ o->wake_owner = wake; o->wake_context = context; });
}

void *oink_audio_endpoint_open(const char *raw_uid, char *error, size_t capacity) {
    @autoreleasepool {
        OinkMuteObserver *o = [OinkMuteObserver new];
        o->uid = [NSString stringWithUTF8String:raw_uid];
        OSStatus status = endpoint_lookup(o->uid, &o->device);
        if (status || !o->device) { snprintf(error, capacity, "Source endpoint unavailable: Core Audio status %d", (int)status); return NULL; }
        AudioObjectPropertyAddress mute = {kAudioDevicePropertyMute,
            kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
        Boolean settable = false;
        float volume; bool muted;
        status = AudioObjectIsPropertySettable(o->device, &mute, &settable);
        if (!status && settable) status = endpoint_controls(o->device, &volume, &muted);
        if (status || !settable) { snprintf(error, capacity, "Source output has no readable hardware volume/writable mute: Core Audio status %d", (int)status); return NULL; }
        o->initial_volume = volume;
        atomic_store(&o->expected_mute, muted);
        status = endpoint_default(&o->initial_default);
        if (status) { snprintf(error, capacity, "Read default source endpoint: Core Audio status %d", (int)status); return NULL; }
        o->queue = dispatch_queue_create("com.hachiwii.oink.audio-mute", DISPATCH_QUEUE_SERIAL);
        __weak OinkMuteObserver *weak_observer = o;
        o->listener = ^(UInt32 count, const AudioObjectPropertyAddress *addresses) {
            OinkMuteObserver *o = weak_observer;
            if (!o || atomic_load(&o->closing)) return;
            // This is an ordinary serial HAL notification queue, never the
            // realtime audio callback. No journal/storage, writes, or input
            // routing occurs here. CoreAudio has no WASAPI caller GUID or
            // historical value. Ignore matching own/redundant mute echoes;
            // any observed unmute permanently gives the user's control back.
            for (UInt32 i = 0; i < count; ++i) {
                AudioObjectPropertySelector selector = addresses[i].mSelector;
                if (selector == kAudioHardwarePropertyServiceRestarted) {
                    atomic_store(&o->service_changed, true);
                } else if (selector == kAudioHardwarePropertyDefaultOutputDevice) {
                    AudioDeviceID current = kAudioObjectUnknown;
                    if (endpoint_default(&current) || current != o->initial_default)
                        atomic_store(&o->default_changed, true);
                } else if (selector == kAudioDevicePropertyVolumeScalar || selector == kAudioDevicePropertyVolumeDecibels) {
                    atomic_store(&o->revoked, true);
                } else if (selector == kAudioDevicePropertyMute) {
                    float v; bool m;
                    if (endpoint_controls(o->device, &v, &m)) {
                        atomic_store(&o->mute_revoked, true);
                        atomic_store(&o->revoked, true);
                    } else observe_controls(o, v, m);
                }
            }
            if (o->wake_owner) o->wake_owner(o->wake_context);
        };
        const AudioObjectPropertyAddress *addresses[] = {&controls, &defaults, &service};
        const AudioObjectID objects[] = {o->device, kAudioObjectSystemObject, kAudioObjectSystemObject};
        for (unsigned i = 0; i < 3; ++i) {
            status = AudioObjectAddPropertyListenerBlock(objects[i], addresses[i], o->queue, o->listener);
            if (status) {
                snprintf(error, capacity, "Observe source endpoint controls: Core Audio status %d", (int)status);
                oink_audio_endpoint_close((void *)CFBridgingRetain(o));
                return NULL;
            }
            o->subscribed[i] = true;
        }
        // Registration must not hide a default switch during setup.
        AudioDeviceID current = kAudioObjectUnknown;
        if (endpoint_default(&current) || current != o->initial_default)
            atomic_store(&o->default_changed, true);
        return (void *)CFBridgingRetain(o);
    }
}

unsigned oink_audio_endpoint_flags(void *opaque) {
    OinkMuteObserver *o = (__bridge OinkMuteObserver *)opaque;
    return (atomic_load(&o->revoked) ? 1u : 0u) |
        (atomic_load(&o->mute_revoked) ? 2u : 0u) |
        (atomic_load(&o->default_changed) ? 4u : 0u);
}

int oink_audio_endpoint_set_mute(void *opaque, bool muted, bool release_only, char *error, size_t capacity) {
    OinkMuteObserver *o = (__bridge OinkMuteObserver *)opaque;
    __block OSStatus status = noErr;
    dispatch_sync(o->queue, ^{
        if (atomic_load(&o->closing) || atomic_load(&o->service_changed) ||
            atomic_load(release_only ? &o->mute_revoked : &o->revoked)) {
            status = kAudioHardwareIllegalOperationError; return;
        }
        AudioDeviceID current = kAudioObjectUnknown;
        status = endpoint_lookup(o->uid, &current);
        if (status || current != o->device) { status = kAudioHardwareBadDeviceError; return; }
        float volume; bool old_mute;
        status = endpoint_controls(current, &volume, &old_mute);
        if (status) return;
        observe_controls(o, volume, old_mute);
        if (atomic_load(release_only ? &o->mute_revoked : &o->revoked)) {
            status = kAudioHardwareIllegalOperationError; return;
        }
        if (old_mute == muted) return;
        AudioObjectPropertyAddress a = {kAudioDevicePropertyMute,
            kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
        UInt32 value = muted;
        status = AudioObjectSetPropertyData(current, &a, 0, NULL, sizeof(value), &value);
        if (!status) atomic_store(&o->expected_mute, muted);
    });
    // Already queued own notifications finish before the Rust readback. HAL
    // notifications have no caller identity/history; value matching is best effort.
    dispatch_sync(o->queue, ^{});
    if (status) snprintf(error, capacity, "Set source endpoint mute: Core Audio status %d", (int)status);
    return status;
}

// Endpoint controls are machine-wide, including different OINK_HOME/user
// sessions. The shared file contains no data and is opened read-only; flock
// does not require write access on Darwin. No user can use this as a file write.
// O_NOFOLLOW/regular-file checks reject symlinks or nonregular locks. Private journals remain
// per-user, just as with Windows' Global mutex and private recovery directory.
int oink_audio_mute_lock(void) {
    const char *path = "/tmp/com.hachiwii.oink.audio-mute.lock";
    int fd = open(path, O_CREAT | O_EXCL | O_RDONLY | O_CLOEXEC | O_NOFOLLOW, 0444);
    if (fd >= 0) {
        if (fchmod(fd, 0444)) { close(fd); return -1; }
    } else if (errno == EEXIST) {
        fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    }
    if (fd < 0) return -1;
    struct stat s;
    if (fstat(fd, &s) || !S_ISREG(s.st_mode) || flock(fd, LOCK_EX | LOCK_NB)) {
        close(fd); return -1;
    }
    return fd;
}
void oink_audio_mute_unlock(int fd) {
    // A forked child can retain this open file description until exec/exit,
    // even with O_CLOEXEC. End our lock ownership before closing our reference.
    flock(fd, LOCK_UN);
    close(fd);
}
