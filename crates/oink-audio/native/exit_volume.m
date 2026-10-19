// Exact-output volume observation/control. Never changes the system default,
// captures PCM, or participates in the source-mute restoration lease.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <ApplicationServices/ApplicationServices.h>
#include <stdatomic.h>
#include <math.h>

static const AudioObjectPropertyAddress exit_controls = {
    kAudioObjectPropertySelectorWildcard, kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementWildcard};
static const AudioObjectPropertyAddress exit_alive = {
    kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
static const AudioObjectPropertyAddress exit_service = {
    kAudioHardwarePropertyServiceRestarted, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};

@interface OinkExitVolume : NSObject {
@public
    NSString *uid;
    AudioDeviceID device;
    dispatch_queue_t queue;
    AudioObjectPropertyListenerBlock listener;
    bool subscribed[3];
    void (*wake)(void *);
    void *context;
    _Atomic bool dirty, invalidated, closing;
}
@end
@implementation OinkExitVolume
@end

// 0 Ready, 1 Unsupported, 2 Unavailable, 3 PermissionDenied, 4 BackendFailed.
static int exit_error(OSStatus status, int32_t *native) {
    *native = status;
    if (status == kAudioDevicePermissionsError) return 3;
    if (status == kAudioHardwareUnknownPropertyError) return 1;
    if (status == kAudioHardwareBadDeviceError || status == kAudioHardwareBadObjectError) return 2;
    return status ? 4 : 0;
}
static OSStatus exit_lookup(NSString *uid, AudioDeviceID *device) {
    CFStringRef value = (__bridge CFStringRef)uid;
    AudioObjectPropertyAddress address = {kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(*device);
    *device = kAudioObjectUnknown;
    return AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, sizeof(value), &value, &size, device);
}
static int exit_identity(OinkExitVolume *o, int32_t *native) {
    if (atomic_load(&o->invalidated)) return 2;
    AudioDeviceID current = kAudioObjectUnknown;
    OSStatus status = exit_lookup(o->uid, &current);
    if (status) return exit_error(status, native);
    if (!current || current != o->device) return 2;
    UInt32 alive = 0, size = sizeof(alive);
    status = AudioObjectGetPropertyData(current, &exit_alive, 0, NULL, &size, &alive);
    if (status) return exit_error(status, native);
    return alive ? 0 : 2;
}
static int exit_properties(OinkExitVolume *o, int32_t *native) {
    for (unsigned i = 0; i < 2; ++i) {
        AudioObjectPropertyAddress address = {i ? kAudioDevicePropertyMute : kAudioDevicePropertyVolumeScalar,
            kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
        if (!AudioObjectHasProperty(o->device, &address)) return 1;
        Boolean settable = false;
        OSStatus status = AudioObjectIsPropertySettable(o->device, &address, &settable);
        if (status) return exit_error(status, native);
        if (!settable) return 1;
    }
    return 0;
}
void oink_exit_volume_close(void *opaque) {
    if (!opaque) return;
    OinkExitVolume *o = CFBridgingRelease(opaque);
    atomic_store(&o->closing, true);
    const AudioObjectPropertyAddress *addresses[] = {&exit_controls, &exit_alive, &exit_service};
    const AudioObjectID objects[] = {o->device, o->device, kAudioObjectSystemObject};
    bool failed = false;
    for (unsigned i = 0; i < 3; ++i)
        if (o->subscribed[i] && AudioObjectRemovePropertyListenerBlock(objects[i], addresses[i], o->queue, o->listener))
            failed = true;
    dispatch_sync(o->queue, ^{});
    // A failed removal leaves HAL retaining a block with a weak reference, never
    // a dangling owner. Drain already-queued callbacks before releasing the owner.
    if (!failed) o->listener = nil;
}
void *oink_exit_volume_open(const char *raw_uid, int *result, int32_t *native, void (*wake)(void *), void *context) {
    @autoreleasepool {
        *native = 0;
        OinkExitVolume *o = [OinkExitVolume new];
        o->wake = wake; o->context = context;
        o->uid = [NSString stringWithUTF8String:raw_uid];
        OSStatus status = exit_lookup(o->uid, &o->device);
        if (status) { *result = exit_error(status, native); return NULL; }
        if (!o->device) { *result = 2; return NULL; }
        *result = exit_properties(o, native);
        if (*result) return NULL;
        o->queue = dispatch_queue_create("com.hachiwii.oink.exit-volume", DISPATCH_QUEUE_SERIAL);
        atomic_store(&o->dirty, true);
        __weak OinkExitVolume *weak = o;
        o->listener = ^(UInt32 count, const AudioObjectPropertyAddress *changed) {
            OinkExitVolume *o = weak;
            if (!o || atomic_load(&o->closing)) return;
            for (UInt32 i = 0; i < count; ++i) {
                AudioObjectPropertySelector selector = changed[i].mSelector;
                if (selector == kAudioHardwarePropertyServiceRestarted || selector == kAudioDevicePropertyDeviceIsAlive)
                    atomic_store(&o->invalidated, true);
            }
            atomic_store(&o->dirty, true);
            if (o->wake) o->wake(o->context);
        };
        const AudioObjectPropertyAddress *addresses[] = {&exit_controls, &exit_alive, &exit_service};
        const AudioObjectID objects[] = {o->device, o->device, kAudioObjectSystemObject};
        for (unsigned i = 0; i < 3; ++i) {
            status = AudioObjectAddPropertyListenerBlock(objects[i], addresses[i], o->queue, o->listener);
            if (status) {
                *result = exit_error(status, native);
                oink_exit_volume_close((void *)CFBridgingRetain(o));
                return NULL;
            }
            o->subscribed[i] = true;
        }
        *result = 0;
        return (void *)CFBridgingRetain(o);
    }
}
bool oink_exit_volume_changed(void *opaque) {
    OinkExitVolume *o = (__bridge OinkExitVolume *)opaque;
    return atomic_load(&o->dirty);
}
int oink_exit_volume_read(void *opaque, float *volume, bool *muted, int32_t *native) {
    OinkExitVolume *o = (__bridge OinkExitVolume *)opaque;
    *native = 0;
    atomic_store(&o->dirty, false);
    int result = exit_identity(o, native);
    if (result) return result;
    AudioObjectPropertyAddress address = {kAudioDevicePropertyVolumeScalar,
        kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(*volume);
    OSStatus status = AudioObjectGetPropertyData(o->device, &address, 0, NULL, &size, volume);
    if (status) return exit_error(status, native);
    if (!isfinite(*volume) || *volume < 0 || *volume > 1) return 4;
    address.mSelector = kAudioDevicePropertyMute;
    UInt32 mute = 0; size = sizeof(mute);
    status = AudioObjectGetPropertyData(o->device, &address, 0, NULL, &size, &mute);
    if (status) return exit_error(status, native);
    *muted = mute != 0;
    return 0;
}
int oink_exit_volume_set(void *opaque, float volume, bool muted, int32_t *native) {
    OinkExitVolume *o = (__bridge OinkExitVolume *)opaque;
    *native = 0;
    if (!isfinite(volume) || volume < 0 || volume > 1) return 4;
    int result = exit_identity(o, native);
    if (!result) result = exit_properties(o, native);
    if (result) return result;
    AudioObjectPropertyAddress address = {kAudioDevicePropertyVolumeScalar,
        kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
    OSStatus status = AudioObjectSetPropertyData(o->device, &address, 0, NULL, sizeof(volume), &volume);
    if (status) return exit_error(status, native);
    // Confirm exact UID again before the second property; a disappearing device
    // must never redirect a command to a new default or reused object ID.
    result = exit_identity(o, native);
    if (result) return result;
    address.mSelector = kAudioDevicePropertyMute;
    UInt32 mute = muted;
    status = AudioObjectSetPropertyData(o->device, &address, 0, NULL, sizeof(mute), &mute);
    atomic_store(&o->dirty, true);
    return exit_error(status, native);
}

int oink_exit_volume_key(void *opaque, int action, uint8_t modifiers, int32_t *native) {
    float volume; bool muted;
    int result = oink_exit_volume_read(opaque, &volume, &muted, native);
    if (result) return result;
    OinkExitVolume *o = (__bridge OinkExitVolume *)opaque;
    AudioDeviceID current = kAudioObjectUnknown;
    AudioObjectPropertyAddress address = {kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(current);
    OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, &size, &current);
    if (status) return exit_error(status, native);
    if (current == o->device) {
        // HAL writes bypass macOS's volume bezel. Post the same NX AUX pair as
        // a media keyboard instead. Use CG fields (83/149/150), as in the input
        // injector: NSEvent eventWithCGEvent on a worker can trigger HIToolbox's
        // main-thread assertion. Only the current default can be addressed by NX;
        // explicitly selected other speakers must keep exact-output HAL control.
        // The existing input tag makes our tap pass this without re-routing it.
        // CGEventPost uses the app's existing Accessibility grant; never prompt.
        // HUD, mute and quarter steps verified on macOS 26.6.2.
        if (!CGPreflightPostEventAccess()) return 3;
        CGEventRef events[2] = {CGEventCreate(NULL), CGEventCreate(NULL)};
        if (!events[0] || !events[1]) {
            if (events[0]) CFRelease(events[0]);
            if (events[1]) CFRelease(events[1]);
            return 4;
        }
        CGEventFlags flags = 0;
        if (modifiers & 0x11) flags |= kCGEventFlagMaskControl;
        if (modifiers & 0x22) flags |= kCGEventFlagMaskShift;
        if (modifiers & 0x44) flags |= kCGEventFlagMaskAlternate;
        if (modifiers & 0x88) flags |= kCGEventFlagMaskCommand;
        int key = action > 0 ? 0 : action < 0 ? 1 : 7; // NX SOUND_UP/DOWN/MUTE
        for (unsigned i = 0; i < 2; ++i) {
            int state = i ? 0xB00 : 0xA00;
            CGEventSetType(events[i], (CGEventType)14); // NX_SYSDEFINED
            CGEventSetFlags(events[i], flags | state);
            CGEventSetIntegerValueField(events[i], (CGEventField)83, 8); // AUX_CONTROL_BUTTONS
            CGEventSetIntegerValueField(events[i], (CGEventField)149, (key << 16) | state);
            CGEventSetIntegerValueField(events[i], (CGEventField)150, -1);
            CGEventSetIntegerValueField(events[i], kCGEventSourceUserData, 0x4F494E4B);
            CGEventPost(kCGHIDEventTap, events[i]);
            CFRelease(events[i]);
        }
        // Delivery is asynchronous. HAL listeners wake the owner for actual
        // readback/publication; do not invent an optimistic slider value here.
        return 0;
    }
    if (!action) muted = !muted;
    else {
        // A native key cannot address a nondefault output. Preserve its identity
        // and Option+Shift quarter-step semantics without changing OS settings.
        float steps = (modifiers & 0x22) && (modifiers & 0x44) ? 64.0f : 16.0f;
        volume = action > 0 ? (floorf(volume * steps + 0.0001f) + 1) / steps
                            : (ceilf(volume * steps - 0.0001f) - 1) / steps;
        volume = fmaxf(0, fminf(1, volume)); muted = false;
    }
    return oink_exit_volume_set(opaque, volume, muted, native);
}
