// Core Audio system-output tap. No default-device or endpoint-volume changes.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <mach/mach_time.h>
#include <math.h>
#include <sys/sysctl.h>
#include <pthread.h>
#include <mach/mach.h>

// Single HAL producer, single Rust worker consumer; capacity is in stereo frames.
#define RING_FRAMES 8192u
// Bounded SPSC diagnostic ring: callback writes only POD and lock-free atomics.
// Losing diagnostic events never consumes/drops audio or changes queue policy.
#define EVENT_CAPACITY 1024u
_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "callback thread identity requires lock-free atomics");
_Static_assert(ATOMIC_LLONG_LOCK_FREE == 2, "callback diagnostics require lock-free counters");
typedef struct {
    uint64_t callback_ns, host_ns, sequence, gap_ns;
    uint64_t frames, queued_frames, dropped_frames;
} OinkCaptureEvent;
// UINT32_MAX means an unavailable HAL property, never a measured zero.
typedef struct {
    double sample_rate;
    double physical_sample_rate;
    double tap_sample_rate;
    uint32_t buffer_frames;
    uint32_t latency_frames;
    uint32_t safety_frames;
    uint32_t physical_device;
    uint32_t physical_buffer_frames;
    uint32_t physical_latency_frames;
    uint32_t physical_safety_frames;
    int32_t buffer_request_status;
    uint32_t physical_original_buffer_frames;
    uint32_t physical_lease_buffer_frames;
    uint32_t physical_lease_changed;
    int32_t physical_buffer_request_status;
    uint32_t capture_mode; // 0 global/tap-only; 1,2 device/master; 3,4 global/master
} OinkCaptureInfo;

typedef struct {
    uint64_t service_epoch;
    CFStringRef tap_uid, aggregate_uid;
    AudioObjectID tap;
    AudioDeviceID aggregate;
    AudioDeviceIOProcID io;
    bool started;
    double sample_rate;
    double tap_sample_rate;
    _Atomic uint64_t write_pos;
    _Atomic uint64_t read_pos;
    _Atomic bool closing;
    float samples[RING_FRAMES * 2];
    uint64_t host_ns[RING_FRAMES];
    uint64_t callback_ns[RING_FRAMES];
    uint64_t sequence[RING_FRAMES];
    OinkCaptureEvent events[EVENT_CAPACITY];
    _Atomic uint64_t event_write, event_read, event_dropped, audio_dropped_frames;
    _Atomic uint32_t callback_thread;
    uint64_t previous_callback_ns;
    _Atomic uint64_t trimmed_frames; // RT consumer writes; ordinary diagnostics reads
    semaphore_t wake;
    _Atomic bool wake_pending;
    uint64_t next_sequence; // HAL producer only; includes dropped frames.
    mach_timebase_info_data_t timebase;
    OinkCaptureInfo info;
    bool low_latency;
    bool explicit_master;
    bool buffer_before_tap;
    bool tap_private;
    uint32_t requested_frames;
    CFMutableArrayRef diagnostic_stages;
} OinkCapture;

static void capture_free(OinkCapture *capture) {
    if (capture->wake != MACH_PORT_NULL) semaphore_destroy(mach_task_self(), capture->wake);
    free(capture);
}
void oink_audio_capture_wake(void *opaque) {
    OinkCapture *capture = opaque;
    if (capture->wake != MACH_PORT_NULL && !atomic_exchange_explicit(&capture->wake_pending, true, memory_order_release))
        semaphore_signal(capture->wake);
}
int32_t oink_audio_capture_wait(void *opaque) {
    OinkCapture *capture = opaque;
    // A stopped/silent process tap may deliver no buffers. Let the encoder's
    // silence timer run without changing capture or the source's mute lease.
    mach_timespec_t timeout = { .tv_sec = 1, .tv_nsec = 0 };
    kern_return_t result = semaphore_timedwait(capture->wake, timeout);
    if (result == KERN_OPERATION_TIMED_OUT) result = KERN_SUCCESS;
    atomic_store_explicit(&capture->wake_pending, false, memory_order_release);
    return result;
}
uint32_t oink_audio_capture_device_id(void *opaque) { return ((OinkCapture *)opaque)->aggregate; }

// Sticky for this process: uncertain cleanup must never be followed by a new
// capture, including retries after an initialization error or endpoint switch.
static _Atomic bool capture_cleanup_failed = false;

bool oink_audio_capture_cleanup_failed(void) {
    return atomic_load_explicit(&capture_cleanup_failed, memory_order_acquire);
}

// AudioHardware.h: service reset invalidates client state, including listeners.
// Keep the callback process-lived; re-arm outside it before creating a new tap.
static _Atomic uint64_t service_epoch = 1;
static uint64_t listening_epoch;
static pthread_mutex_t service_listener_mutex = PTHREAD_MUTEX_INITIALIZER;
static AudioObjectPropertyAddress service_address = {kAudioHardwarePropertyServiceRestarted,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
static OSStatus service_restarted(AudioObjectID object, UInt32 count,
                                const AudioObjectPropertyAddress *addresses, void *context) {
    (void)object; (void)count; (void)addresses; (void)context;
    atomic_fetch_add_explicit(&service_epoch, 1, memory_order_release);
    return noErr;
}
static OSStatus capture_watch_service(void) {
    pthread_mutex_lock(&service_listener_mutex);
    uint64_t epoch = atomic_load_explicit(&service_epoch, memory_order_acquire);
    OSStatus status = noErr;
    if (listening_epoch != epoch) {
        if (listening_epoch)
            AudioObjectRemovePropertyListener(kAudioObjectSystemObject, &service_address, service_restarted, NULL);
        status = AudioObjectAddPropertyListener(kAudioObjectSystemObject, &service_address, service_restarted, NULL);
        if (status == noErr) listening_epoch = epoch;
    }
    pthread_mutex_unlock(&service_listener_mutex);
    return status;
}
// Do not infer daemon death from !dev/!obj alone: a device can disappear without
// releasing a mute elsewhere. Require a reset newer than this capture AND fresh
// successful UID translations proving BOTH owned objects absent. Numeric IDs may
// already have been reused; never destroy/restore through them after a reset.
// The SDK does not promise that local IOProc callbacks are drained by reset, so
// retain the closed context even when mute release is proven. No volume/default
// route is written. Failure reproduced on 14.7.8; recovery fault-injected only,
// real reset validation on 14.7.8/26.6.2 still pending (requires user admin action).
static bool capture_uid_absent(CFStringRef uid, AudioObjectPropertySelector selector) {
    if (!uid) return true; // This object was never created.
    AudioObjectPropertyAddress address = {selector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioObjectID object = kAudioObjectUnknown;
    UInt32 size = sizeof(object);
    return AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, sizeof(uid), &uid,
        &size, &object) == noErr && size == sizeof(object) && object == kAudioObjectUnknown;
}
static bool capture_objects_absent(OinkCapture *capture) {
    return (!capture->tap || capture->tap_uid) && (!capture->aggregate || capture->aggregate_uid)
        && capture_uid_absent(capture->tap_uid, kAudioHardwarePropertyTranslateUIDToTap)
        && capture_uid_absent(capture->aggregate_uid, kAudioHardwarePropertyTranslateUIDToDevice);
}
static bool capture_same_service(OinkCapture *capture) {
    return capture->service_epoch == atomic_load_explicit(&service_epoch, memory_order_acquire);
}
static bool capture_reset_proven(OinkCapture *capture) {
    uint64_t epoch = atomic_load_explicit(&service_epoch, memory_order_acquire);
    return epoch != capture->service_epoch
        && capture_objects_absent(capture)
        && epoch == atomic_load_explicit(&service_epoch, memory_order_acquire);
}
static void capture_release_metadata(OinkCapture *capture) {
    if (capture->diagnostic_stages) CFRelease(capture->diagnostic_stages);
    if (capture->tap_uid) CFRelease(capture->tap_uid);
    if (capture->aggregate_uid) CFRelease(capture->aggregate_uid);
}

static bool device_scope(uint32_t mode) { return mode == 1 || mode == 2 || mode == 5 || mode == 6; }
static bool physical_master(uint32_t mode) { return mode >= 1 && mode <= 4; }
static bool tap_drift(uint32_t mode) { return mode == 0 || mode == 2 || mode == 3 || mode == 6; }
static void capture_record_snapshot(OinkCapture *capture, NSString *stage);

static void capture_event(OinkCapture *capture, uint64_t callback_ns, uint64_t host_ns,
                          uint64_t sequence, uint64_t frames, uint64_t write, uint64_t read) {
    uint64_t available = RING_FRAMES - (write - read);
    if (frames > available) atomic_fetch_add_explicit(&capture->audio_dropped_frames, frames - available, memory_order_relaxed);
    uint64_t gap_ns = capture->previous_callback_ns ? callback_ns - capture->previous_callback_ns : 0;
    capture->previous_callback_ns = callback_ns;
    atomic_store_explicit(&capture->callback_thread, pthread_mach_thread_np(pthread_self()), memory_order_relaxed);
    uint64_t ew = atomic_load_explicit(&capture->event_write, memory_order_relaxed);
    uint64_t er = atomic_load_explicit(&capture->event_read, memory_order_acquire);
    if (ew - er < EVENT_CAPACITY) {
        capture->events[ew & (EVENT_CAPACITY - 1)] = (OinkCaptureEvent){
            callback_ns, host_ns, sequence, gap_ns, frames, write - read,
            frames > available ? frames - available : 0
        };
        atomic_store_explicit(&capture->event_write, ew + 1, memory_order_release);
    } else {
        atomic_fetch_add_explicit(&capture->event_dropped, 1, memory_order_relaxed);
    }
}

static OSStatus capture_io(AudioObjectID device, const AudioTimeStamp *now,
                           const AudioBufferList *input, const AudioTimeStamp *input_time,
                           AudioBufferList *output, const AudioTimeStamp *output_time,
                           void *context) {
    (void)device; (void)now; (void)output; (void)output_time;
    OinkCapture *capture = context;
    if (atomic_load_explicit(&capture->closing, memory_order_acquire)) return noErr;
    uint64_t callback_ns = (uint64_t)(((__uint128_t)mach_absolute_time() * capture->timebase.numer) /
                                     capture->timebase.denom);
    uint64_t host_ns = 0;
    if (input_time && (input_time->mFlags & kAudioTimeStampHostTimeValid)) {
        host_ns = (uint64_t)(((__uint128_t)input_time->mHostTime * capture->timebase.numer) /
                             capture->timebase.denom);
    }
    if (!input || !input->mNumberBuffers) goto empty_callback;
    const AudioBuffer *first = &input->mBuffers[0];
    if (!first->mData || !first->mNumberChannels) goto empty_callback;
    uint32_t frames = first->mDataByteSize / (sizeof(float) * first->mNumberChannels);
    const float *left = first->mData;
    const float *right = left;
    uint32_t stride = first->mNumberChannels;
    uint32_t right_stride = stride;
    if (stride >= 2) {
        right++;
    } else if (input->mNumberBuffers >= 2 && input->mBuffers[1].mData) {
        const AudioBuffer *second = &input->mBuffers[1];
        if (second->mNumberChannels == 0) goto empty_callback;
        right = second->mData;
        right_stride = second->mNumberChannels;
        uint32_t right_frames = second->mDataByteSize / (sizeof(float) * right_stride);
        if (right_frames < frames) frames = right_frames;
    }
    uint64_t write = atomic_load_explicit(&capture->write_pos, memory_order_relaxed);
    uint64_t read = atomic_load_explicit(&capture->read_pos, memory_order_acquire);
    uint64_t sequence = capture->next_sequence;
    capture->next_sequence += frames;
    uint64_t available = RING_FRAMES - (write - read);
    capture_event(capture, callback_ns, host_ns, sequence, frames, write, read);
    if (frames > available) frames = (uint32_t)available;
    for (uint32_t i = 0; i < frames; ++i) {
        size_t index = ((write + i) & (RING_FRAMES - 1)) * 2;
        capture->samples[index] = left[i * stride];
        capture->samples[index + 1] = right[i * right_stride];
        capture->host_ns[index / 2] = host_ns ? host_ns + (uint64_t)(i * 1e9 / capture->sample_rate) : 0;
        capture->sequence[index / 2] = sequence + i;
        capture->callback_ns[index / 2] = callback_ns;
    }
    atomic_store_explicit(&capture->write_pos, write + frames, memory_order_release);
    oink_audio_capture_wake(capture);
    return noErr;
empty_callback:
    capture_event(capture, callback_ns, host_ns, capture->next_sequence, 0,
        atomic_load_explicit(&capture->write_pos, memory_order_relaxed),
        atomic_load_explicit(&capture->read_pos, memory_order_acquire));
    return noErr;
}

static void capture_cleanup_failure(OSStatus status, const char *stage,
                                    OSStatus *first_error, bool *missing_only, char *error, size_t capacity) {
    if (status == noErr) return;
    if (status != kAudioHardwareBadDeviceError && status != kAudioHardwareBadObjectError)
        *missing_only = false;
    if (*first_error == noErr) *first_error = status;
    if (!error || !capacity) return;
    size_t used = strlen(error);
    if (used < capacity) snprintf(error + used, capacity - used, "%s%s: Core Audio status %d",
                                 used ? "; " : "", stage, (int)status);
}

// Called only after the Rust consumer has stopped. HAL cleanup is fallible:
// never free its callback context unless both stop and unregister succeeded.
// A retained context is intentionally not retried or handed to a new capture.
int32_t oink_audio_capture_stop(void *opaque, char *error, size_t capacity) {
    if (error && capacity) error[0] = 0;
    if (!opaque) return noErr;
    OinkCapture *capture = opaque;
    atomic_store_explicit(&capture->closing, true, memory_order_release);
    // A reset is sufficient reason to discard old IDs, but not sufficient
    // evidence to clear mute uncertainty until the UID lookups also succeed.
    if (capture->service_epoch != atomic_load_explicit(&service_epoch, memory_order_acquire)) {
        bool proven = capture_reset_proven(capture);
        capture_release_metadata(capture);
        if (!capture->io) capture_free(capture);
        if (proven) return noErr;
        atomic_store_explicit(&capture_cleanup_failed, true, memory_order_release);
        if (error && capacity) snprintf(error, capacity, "Audio service reset; absence of old tap/aggregate could not be confirmed");
        return kAudioHardwareUnspecifiedError;
    }
    OSStatus status = noErr;
    bool retain_context = false;
    bool missing_only = true;
    if (capture->io) {
        // Also stop after a failed Start: HAL may have partially begun IO.
        OSStatus stop_status = (capture_same_service(capture) ? AudioDeviceStop(capture->aggregate, capture->io) : kAudioHardwareBadObjectError);
        capture_cleanup_failure(stop_status, "AudioDeviceStop", &status, &missing_only, error, capacity);
        OSStatus unregister_status = (capture_same_service(capture) ? AudioDeviceDestroyIOProcID(capture->aggregate, capture->io) : kAudioHardwareBadObjectError);
        capture_cleanup_failure(unregister_status, "AudioDeviceDestroyIOProcID", &status, &missing_only, error, capacity);
        retain_context = stop_status != noErr || unregister_status != noErr;
    }
    if (capture->aggregate) {
        capture_cleanup_failure((capture_same_service(capture) ? AudioHardwareDestroyAggregateDevice(capture->aggregate) : kAudioHardwareBadObjectError),
                                "AudioHardwareDestroyAggregateDevice", &status, &missing_only, error, capacity);
    }
    if (capture->tap) {
        if (@available(macOS 14.2, *)) {
            capture_cleanup_failure((capture_same_service(capture) ? AudioHardwareDestroyProcessTap(capture->tap) : kAudioHardwareBadObjectError),
                                    "AudioHardwareDestroyProcessTap", &status, &missing_only, error, capacity);
        }
    }
    // This device-global lease can be restored while other IO is active, just
    // as it can be acquired then. Attempt restoration even if other cleanup
    // failed; never overwrite a value that no longer matches our lease.
    if (capture->info.physical_lease_changed && capture->service_epoch == atomic_load_explicit(&service_epoch, memory_order_acquire)) {
        AudioObjectPropertyAddress address = {kAudioDevicePropertyBufferFrameSize,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        UInt32 current = 0, size = sizeof(current);
        OSStatus read_status = AudioObjectGetPropertyData(capture->info.physical_device,
            &address, 0, NULL, &size, &current);
        if (read_status != noErr) {
            capture_cleanup_failure(read_status, "Read physical buffer for restoration", &status, &missing_only, error, capacity);
        } else if (capture_same_service(capture) && current == capture->info.physical_lease_buffer_frames) {
            OSStatus restore_status = AudioObjectSetPropertyData(capture->info.physical_device, &address,
                0, NULL, sizeof(capture->info.physical_original_buffer_frames),
                &capture->info.physical_original_buffer_frames);
            capture_cleanup_failure(restore_status, "Restore physical buffer lease", &status, &missing_only, error, capacity);
        }
    }
    // A reset notification can race teardown. The alternative proof accepts
    // ONLY missing-object errors plus successful fresh absence checks for both
    // unique objects. A permission, I/O, or buffer-write failure stays terminal.
    if (status != noErr && missing_only && capture_objects_absent(capture)) {
        status = noErr;
        if (error && capacity) error[0] = 0;
    }
    if (status != noErr)
        atomic_store_explicit(&capture_cleanup_failed, true, memory_order_release);
    capture_release_metadata(capture);
    if (retain_context) {
        // An in-flight or orphaned callback may still touch the ring. Keep all
        // of its storage alive; late callbacks observe closing and return.
        if (status != noErr && error && capacity) {
            size_t used = strlen(error);
            if (used < capacity) snprintf(error + used, capacity - used,
                "; callback context intentionally retained because HAL quiescence is unconfirmed");
        }
    } else {
        capture_free(capture);
    }
    return status;
}

static uint32_t device_frames(AudioDeviceID device, AudioObjectPropertySelector selector,
                              AudioObjectPropertyScope scope) {
    AudioObjectPropertyAddress address = {selector, scope, kAudioObjectPropertyElementMain};
    UInt32 value = UINT32_MAX, size = sizeof(value);
    if (AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &value) != noErr)
        return UINT32_MAX;
    return value;
}

// A process tap can fix its safety margin from the source device's buffer at
// creation. Acquire the optional physical lease BEFORE creating the tap. This
// never changes default routing or volume; the buffer itself is device-global.
static void capture_resolve_physical(OinkCapture *capture, const char *selected_uid) {
    OinkCaptureInfo *info = &capture->info;
    info->physical_original_buffer_frames = UINT32_MAX;
    AudioObjectPropertyAddress address = {kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(info->physical_device);
    OSStatus status;
    if (selected_uid) {
        CFStringRef uid = CFStringCreateWithCString(NULL, selected_uid, kCFStringEncodingUTF8);
        if (!uid) { info->physical_buffer_request_status = kAudioHardwareBadDeviceError; return; }
        address.mSelector = kAudioHardwarePropertyTranslateUIDToDevice;
        status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address,
            sizeof(uid), &uid, &size, &info->physical_device);
        CFRelease(uid);
    } else {
        status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL,
                                           &size, &info->physical_device);
    }
    if (status != noErr || !info->physical_device) {
        info->physical_buffer_request_status = status != noErr ? status : kAudioHardwareBadDeviceError;
        return;
    }
    info->physical_original_buffer_frames = device_frames(info->physical_device,
        kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal);
}

static void capture_physical_lease(OinkCapture *capture, uint32_t requested) {
    OinkCaptureInfo *info = &capture->info;
    if (requested == UINT32_MAX) return; // Explicit baseline: no physical mutation.
    AudioObjectPropertyAddress address = {kAudioDevicePropertyBufferFrameSizeRange,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size;
    if (info->physical_original_buffer_frames == UINT32_MAX) {
        info->physical_buffer_request_status = kAudioHardwareUnsupportedOperationError;
        return; // No mutation without a restoration value.
    }
    AudioValueRange range = {0};
    address.mSelector = kAudioDevicePropertyBufferFrameSizeRange;
    size = sizeof(range);
    if (AudioObjectGetPropertyData(info->physical_device, &address, 0, NULL, &size, &range) == noErr) {
        if (requested < range.mMinimum) requested = (uint32_t)range.mMinimum;
        if (requested > range.mMaximum) requested = (uint32_t)range.mMaximum;
    } else if (!requested) {
        requested = 128;
    }
    address.mSelector = kAudioDevicePropertyBufferFrameSize;
    info->physical_buffer_request_status = AudioObjectSetPropertyData(info->physical_device,
        &address, 0, NULL, sizeof(requested), &requested);
    if (info->physical_buffer_request_status == noErr) {
        uint32_t actual = device_frames(info->physical_device, kAudioDevicePropertyBufferFrameSize,
                                       kAudioObjectPropertyScopeGlobal);
        // A failed readback must not leave an untracked successful mutation.
        info->physical_lease_buffer_frames = actual == UINT32_MAX ? requested : actual;
        info->physical_lease_changed = info->physical_lease_buffer_frames != info->physical_original_buffer_frames;
    }
}

static void capture_info(OinkCapture *capture) {
    OinkCaptureInfo *info = &capture->info;
    info->sample_rate = capture->sample_rate;
    info->tap_sample_rate = capture->tap_sample_rate;
    info->buffer_frames = device_frames(capture->aggregate, kAudioDevicePropertyBufferFrameSize,
                                        kAudioObjectPropertyScopeGlobal);
    info->latency_frames = device_frames(capture->aggregate, kAudioDevicePropertyLatency,
                                         kAudioObjectPropertyScopeInput);
    info->safety_frames = device_frames(capture->aggregate, kAudioDevicePropertySafetyOffset,
                                        kAudioObjectPropertyScopeInput);
    AudioObjectPropertyAddress address = {kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size;
    info->physical_buffer_frames = device_frames(info->physical_device, kAudioDevicePropertyBufferFrameSize,
                                                 kAudioObjectPropertyScopeGlobal);
    info->physical_latency_frames = device_frames(info->physical_device, kAudioDevicePropertyLatency,
                                                  kAudioObjectPropertyScopeOutput);
    info->physical_safety_frames = device_frames(info->physical_device, kAudioDevicePropertySafetyOffset,
                                                 kAudioObjectPropertyScopeOutput);
    address.mSelector = kAudioDevicePropertyNominalSampleRate;
    size = sizeof(info->physical_sample_rate);
    AudioObjectGetPropertyData(info->physical_device, &address, 0, NULL, &size, &info->physical_sample_rate);
}

void oink_audio_capture_info(void *opaque, OinkCaptureInfo *info) {
    *info = ((OinkCapture *)opaque)->info;
}

bool oink_audio_capture_physical_uid(void *opaque, char *buffer, size_t capacity) {
    OinkCapture *capture = opaque;
    AudioObjectPropertyAddress address = {kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    CFStringRef uid = NULL;
    UInt32 size = sizeof(uid);
    OSStatus status = AudioObjectGetPropertyData(capture->info.physical_device, &address,
                                                0, NULL, &size, &uid);
    if (status != noErr || !uid) return false;
    bool result = CFStringGetCString(uid, buffer, capacity, kCFStringEncodingUTF8);
    CFRelease(uid);
    return result;
}

int32_t oink_audio_default_output_uid(char *buffer, size_t capacity) {
    AudioObjectPropertyAddress address = {kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioDeviceID device = kAudioObjectUnknown;
    UInt32 size = sizeof(device);
    OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, &size, &device);
    if (status != noErr) return status;
    address.mSelector = kAudioDevicePropertyDeviceUID;
    CFStringRef uid = NULL;
    size = sizeof(uid);
    status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &uid);
    if (status != noErr || !uid) return status != noErr ? status : kAudioHardwareBadDeviceError;
    bool copied = CFStringGetCString(uid, buffer, capacity, kCFStringEncodingUTF8);
    CFRelease(uid);
    return copied ? noErr : kAudioHardwareBadPropertySizeError;
}

uint64_t oink_audio_host_time_ns(void) {
    mach_timebase_info_data_t timebase;
    mach_timebase_info(&timebase);
    return (uint64_t)(((__uint128_t)mach_absolute_time() * timebase.numer) / timebase.denom);
}

// The tap format need not equal the aggregate's IO format (for example a
// 44.1 kHz global tap with a 48 kHz explicitly selected physical master). Read
// each input stream's actual virtual format; these are the buffers HAL delivers.
static OSStatus capture_io_format(OinkCapture *capture, double *sample_rate) {
    AudioObjectPropertyAddress address = {kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeInput, kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(capture->aggregate, &address, 0, NULL, &size);
    if (status != noErr) return status;
    if (!size) return kAudioHardwareUnsupportedOperationError;
    AudioStreamID *streams = malloc(size);
    if (!streams) return kAudioHardwareUnspecifiedError;
    status = AudioObjectGetPropertyData(capture->aggregate, &address, 0, NULL, &size, streams);
    UInt32 count = size / sizeof(AudioStreamID), channels = 0;
    double rate = 0;
    for (UInt32 i = 0; status == noErr && i < count; ++i) {
        AudioStreamBasicDescription format = {0};
        address.mSelector = kAudioStreamPropertyVirtualFormat;
        address.mScope = kAudioObjectPropertyScopeGlobal;
        size = sizeof(format);
        status = AudioObjectGetPropertyData(streams[i], &address, 0, NULL, &size, &format);
        if (status != noErr) break;
        if (format.mFormatID != kAudioFormatLinearPCM ||
            !(format.mFormatFlags & kAudioFormatFlagIsFloat) ||
            (format.mFormatFlags & kAudioFormatFlagIsBigEndian) ||
            format.mBitsPerChannel != 32 || !format.mChannelsPerFrame ||
            format.mSampleRate < 8000 || format.mSampleRate > 192000 ||
            (i && format.mSampleRate != rate)) {
            status = kAudioHardwareUnsupportedOperationError;
            break;
        }
        rate = format.mSampleRate;
        channels += format.mChannelsPerFrame;
    }
    free(streams);
    if (status == noErr && channels != 2) status = kAudioHardwareUnsupportedOperationError;
    if (status == noErr) *sample_rate = rate;
    return status;
}

// This experiment permits only output-only physical endpoints. Rejecting
// duplex devices avoids enabling a microphone merely to obtain its clock.
static OSStatus require_output_only_device(AudioDeviceID device) {
    AudioObjectPropertyAddress address = {kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeInput, kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size);
    if (status != noErr) return status;
    return size == 0 ? noErr : kAudioHardwareUnsupportedOperationError;
}

static OSStatus require_single_output_stream(AudioDeviceID device, UInt32 *count) {
    AudioObjectPropertyAddress address = {kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size);
    if (status != noErr) return status;
    *count = size / sizeof(AudioStreamID);
    return *count == 1 ? noErr : kAudioHardwareUnsupportedOperationError;
}

static OSStatus configure_clock_only_capture(OinkCapture *capture) {
    // No physical inputs were admitted; verify the aggregate exposes exactly
    // the two tap channels expected by capture_io before starting any IO.
    AudioObjectPropertyAddress address = {kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeInput, kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(capture->aggregate, &address, 0, NULL, &size);
    if (status != noErr) return status;
    AudioBufferList *configuration = malloc(size);
    if (!configuration) return kAudioHardwareUnspecifiedError;
    status = AudioObjectGetPropertyData(capture->aggregate, &address, 0, NULL, &size, configuration);
    UInt32 channels = 0;
    if (status == noErr) {
        for (UInt32 i = 0; i < configuration->mNumberBuffers; ++i)
            channels += configuration->mBuffers[i].mNumberChannels;
    }
    free(configuration);
    if (status != noErr) return status;
    if (channels != 2) return kAudioHardwareUnsupportedOperationError;

    // Physical output is a clock reference only. Disable every output stream
    // for this IOProc; never submit copied tap audio or silence to the output.
    address.mSelector = kAudioDevicePropertyStreams;
    address.mScope = kAudioObjectPropertyScopeOutput;
    status = AudioObjectGetPropertyDataSize(capture->aggregate, &address, 0, NULL, &size);
    if (status != noErr || size == 0) return status;
    UInt32 count = size / sizeof(AudioStreamID);
    UInt32 usage_size = sizeof(AudioHardwareIOProcStreamUsage) + (count - 1) * sizeof(UInt32);
    AudioHardwareIOProcStreamUsage *usage = calloc(1, usage_size);
    if (!usage) return kAudioHardwareUnspecifiedError;
    usage->mIOProc = (void *)capture->io;
    usage->mNumberStreams = count;
    address.mSelector = kAudioDevicePropertyIOProcStreamUsage;
    status = AudioObjectSetPropertyData(capture->aggregate, &address, 0, NULL, usage_size, usage);
    if (status == noErr) {
        size = usage_size;
        status = AudioObjectGetPropertyData(capture->aggregate, &address, 0, NULL, &size, usage);
        if (status == noErr) {
            if (usage->mNumberStreams != count) status = kAudioHardwareUnsupportedOperationError;
            for (UInt32 i = 0; i < count; ++i)
                if (usage->mStreamIsOn[i]) status = kAudioHardwareUnsupportedOperationError;
        }
    }
    free(usage);
    return status;
}

// Diagnostics run on the control worker, never in the realtime callback. Every
// read preserves its status and settable flag: unavailable is not measured zero.
static NSDictionary *diagnostic_number(AudioObjectID object, AudioObjectPropertySelector selector,
                                        AudioObjectPropertyScope scope, unsigned kind) {
    AudioObjectPropertyAddress address = {selector, scope, kAudioObjectPropertyElementMain};
    Boolean settable = false;
    OSStatus settable_status = AudioObjectIsPropertySettable(object, &address, &settable);
    union { UInt32 integer; Float64 double_value; Float32 float_value; } value = {0};
    UInt32 size = kind == 1 ? sizeof(Float64) : sizeof(UInt32);
    OSStatus status = AudioObjectGetPropertyData(object, &address, 0, NULL, &size, &value);
    id result = [NSNull null];
    if (status == noErr) {
        if (kind == 0) result = @(value.integer);
        else if (kind == 1 && isfinite(value.double_value)) result = @(value.double_value);
        else if (kind == 2 && isfinite(value.float_value)) result = @(value.float_value);
    }
    return @{ @"status": @(status), @"value": result,
              @"settable_status": @(settable_status),
              @"settable": settable_status == noErr ? @(settable) : (id)[NSNull null] };
}

static NSDictionary *diagnostic_cf(AudioObjectID object, AudioObjectPropertySelector selector) {
    AudioObjectPropertyAddress address = {selector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    CFTypeRef value = NULL;
    UInt32 size = sizeof(value);
    OSStatus status = AudioObjectGetPropertyData(object, &address, 0, NULL, &size, &value);
    id result = [NSNull null];
    if (status == noErr && value) {
        id owned = CFBridgingRelease(value);
        result = [NSJSONSerialization isValidJSONObject:@[owned]] ? owned : [owned description];
    }
    return @{ @"status": @(status), @"value": result };
}

static NSDictionary *diagnostic_format(AudioObjectID object, AudioObjectPropertySelector selector) {
    AudioObjectPropertyAddress address = {selector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioStreamBasicDescription format = {0};
    UInt32 size = sizeof(format);
    OSStatus status = AudioObjectGetPropertyData(object, &address, 0, NULL, &size, &format);
    return @{ @"status": @(status), @"sample_rate": @(format.mSampleRate),
        @"format_id": @(format.mFormatID), @"format_flags": @(format.mFormatFlags),
        @"bytes_per_packet": @(format.mBytesPerPacket), @"frames_per_packet": @(format.mFramesPerPacket),
        @"bytes_per_frame": @(format.mBytesPerFrame), @"channels": @(format.mChannelsPerFrame),
        @"bits_per_channel": @(format.mBitsPerChannel) };
}

static NSDictionary *diagnostic_object_list_qualified(AudioObjectID object, AudioObjectPropertySelector selector,
                                            AudioObjectPropertyScope scope, bool streams, AudioClassID class_filter) {
    AudioObjectPropertyAddress address = {selector, scope, kAudioObjectPropertyElementMain};
    UInt32 qualifier_size = class_filter ? sizeof(class_filter) : 0;
    const void *qualifier = class_filter ? &class_filter : NULL;
    UInt32 size = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(object, &address, qualifier_size, qualifier, &size);
    NSMutableArray *items = [NSMutableArray array];
    if (status == noErr && size) {
        AudioObjectID *objects = malloc(size);
        if (!objects) return @{ @"status": @(kAudioHardwareUnspecifiedError), @"items": items };
        status = AudioObjectGetPropertyData(object, &address, qualifier_size, qualifier, &size, objects);
        if (status == noErr) {
            for (UInt32 i = 0; i < size / sizeof(AudioObjectID); ++i) {
                AudioObjectID item = objects[i];
                if (streams) {
                    [items addObject:@{ @"id": @(item),
                        @"virtual_format": diagnostic_format(item, kAudioStreamPropertyVirtualFormat),
                        @"physical_format": diagnostic_format(item, kAudioStreamPropertyPhysicalFormat),
                        @"latency_frames": diagnostic_number(item, kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal, 0),
                        @"is_active": diagnostic_number(item, kAudioStreamPropertyIsActive, kAudioObjectPropertyScopeGlobal, 0) }];
                } else {
                    [items addObject:@{ @"id": @(item),
                        @"class": diagnostic_number(item, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0),
                        @"drift_enabled": diagnostic_number(item, kAudioSubTapPropertyDriftCompensation, kAudioObjectPropertyScopeGlobal, 0),
                        @"drift_quality": diagnostic_number(item, kAudioSubTapPropertyDriftCompensationQuality, kAudioObjectPropertyScopeGlobal, 0),
                        @"extra_latency_global_frames": diagnostic_number(item, kAudioSubTapPropertyExtraLatency, kAudioObjectPropertyScopeGlobal, 1),
                        @"extra_latency_input_frames": diagnostic_number(item, kAudioSubTapPropertyExtraLatency, kAudioObjectPropertyScopeInput, 1),
                        @"extra_latency_output_frames": diagnostic_number(item, kAudioSubTapPropertyExtraLatency, kAudioObjectPropertyScopeOutput, 1) }];
                }
            }
        }
        free(objects);
    }
    return @{ @"status": @(status), @"items": items };
}

static NSDictionary *diagnostic_object_list(AudioObjectID object, AudioObjectPropertySelector selector,
                                            AudioObjectPropertyScope scope, bool streams) {
    return diagnostic_object_list_qualified(object, selector, scope, streams, 0);
}

static NSDictionary *diagnostic_device(AudioObjectID device) {
    if (!device) return @{ @"id": @0, @"unavailable": @YES };
    AudioObjectPropertyAddress range_address = {kAudioDevicePropertyBufferFrameSizeRange,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioValueRange range = {0};
    UInt32 size = sizeof(range);
    OSStatus range_status = AudioObjectGetPropertyData(device, &range_address, 0, NULL, &size, &range);
    return @{ @"id": @(device), @"uid": diagnostic_cf(device, kAudioDevicePropertyDeviceUID),
        @"name": diagnostic_cf(device, kAudioObjectPropertyName),
        @"manufacturer": diagnostic_cf(device, kAudioObjectPropertyManufacturer),
        @"model_uid": diagnostic_cf(device, kAudioDevicePropertyModelUID),
        @"running": diagnostic_number(device, kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal, 0),
        @"running_somewhere": diagnostic_number(device, kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioObjectPropertyScopeGlobal, 0),
        @"transport_type": diagnostic_number(device, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal, 0),
        @"alive": diagnostic_number(device, kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal, 0),
        @"sample_rate": diagnostic_number(device, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, 1),
        @"actual_sample_rate": diagnostic_number(device, kAudioDevicePropertyActualSampleRate, kAudioObjectPropertyScopeGlobal, 1),
        @"io_cycle_usage": diagnostic_number(device, kAudioDevicePropertyIOCycleUsage, kAudioObjectPropertyScopeGlobal, 2),
        @"buffer_frames": diagnostic_number(device, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal, 0),
        @"buffer_range": @{ @"status": @(range_status), @"minimum": @(range.mMinimum), @"maximum": @(range.mMaximum) },
        @"input_safety_frames": diagnostic_number(device, kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeInput, 0),
        @"output_safety_frames": diagnostic_number(device, kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeOutput, 0),
        @"input_latency_frames": diagnostic_number(device, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeInput, 0),
        @"output_latency_frames": diagnostic_number(device, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput, 0),
        @"input_streams": diagnostic_object_list(device, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput, true),
        @"output_streams": diagnostic_object_list(device, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput, true) };
}

static NSDictionary *diagnostic_tap(AudioObjectID tap) {
    if (!tap) return @{ @"id": @0, @"unavailable": @YES };
    if (@available(macOS 14.2, *)) {
        AudioObjectPropertyAddress address = {kAudioTapPropertyDescription,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        void *raw = NULL;
        UInt32 size = sizeof(raw);
        OSStatus status = AudioObjectGetPropertyData(tap, &address, 0, NULL, &size, &raw);
        CATapDescription *description = status == noErr && raw ? CFBridgingRelease(raw) : nil;
        id config = [NSNull null];
        if (description) config = @{ @"private": @(description.privateTap),
            @"device_uid": description.deviceUID ?: (id)[NSNull null],
            @"stream": description.stream ?: (id)[NSNull null],
            @"exclusive": @(description.exclusive), @"mixdown": @(description.mixdown),
            @"mono": @(description.mono), @"mute_behavior": @(description.muteBehavior) };
        return @{ @"id": @(tap), @"format": diagnostic_format(tap, kAudioTapPropertyFormat),
            @"description_status": @(status), @"description": config };
    }
    return @{ @"id": @(tap), @"unavailable": @YES };
}

static void capture_record_snapshot(OinkCapture *capture, NSString *stage) {
    if (!capture->diagnostic_stages) return;
    AudioObjectPropertyAddress address = {kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioDeviceID default_device = kAudioObjectUnknown;
    UInt32 size = sizeof(default_device);
    OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, &size, &default_device);
    NSDictionary *entry = @{ @"stage": stage, @"default_output_status": @(status),
        @"default_output": diagnostic_device(default_device),
        @"selected_output": diagnostic_device(capture->info.physical_device),
        @"tap": diagnostic_tap(capture->tap), @"aggregate": diagnostic_device(capture->aggregate),
        @"aggregate_main_subdevice": diagnostic_cf(capture->aggregate, kAudioAggregateDevicePropertyMainSubDevice),
        @"aggregate_clock_device": diagnostic_cf(capture->aggregate, kAudioAggregateDevicePropertyClockDevice),
        @"aggregate_composition": diagnostic_cf(capture->aggregate, kAudioAggregateDevicePropertyComposition),
        @"aggregate_full_subdevice_uids": diagnostic_cf(capture->aggregate, kAudioAggregateDevicePropertyFullSubDeviceList),
        @"aggregate_tap_uids": diagnostic_cf(capture->aggregate, kAudioAggregateDevicePropertyTapList),
        @"aggregate_owned_subdevices": diagnostic_object_list_qualified(capture->aggregate, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, false, kAudioSubDeviceClassID),
        @"aggregate_owned_subtaps": diagnostic_object_list_qualified(capture->aggregate, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, false, kAudioSubTapClassID),
        @"aggregate_subtaps": diagnostic_object_list(capture->aggregate, kAudioAggregateDevicePropertySubTapList, kAudioObjectPropertyScopeGlobal, false),
        @"aggregate_subdevices": diagnostic_object_list(capture->aggregate, kAudioAggregateDevicePropertyActiveSubDeviceList, kAudioObjectPropertyScopeGlobal, false) };
    CFArrayAppendValue(capture->diagnostic_stages, (__bridge CFDictionaryRef)entry);
}

size_t oink_audio_capture_diagnostics(void *opaque, char *buffer, size_t capacity) {
    @autoreleasepool {
        OinkCapture *capture = opaque;
        NSOperatingSystemVersion os = [NSProcessInfo processInfo].operatingSystemVersion;
        char model[128] = {0};
        size_t model_size = sizeof(model);
        int model_status = sysctlbyname("hw.model", model, &model_size, NULL, 0);
        NSDictionary *report = @{ @"schema": @"oink-coreaudio-capture-diagnostics-v1",
            @"os": @{ @"major": @(os.majorVersion), @"minor": @(os.minorVersion), @"patch": @(os.patchVersion),
                       @"description": [NSProcessInfo processInfo].operatingSystemVersionString },
            @"hardware_model": model_status == 0 ? [NSString stringWithUTF8String:model] : (id)[NSNull null],
            @"global_tap_requested": @(!device_scope(capture->info.capture_mode)),
            @"physical_master_requested": @(physical_master(capture->info.capture_mode)),
            @"tap_drift_requested": @(tap_drift(capture->info.capture_mode)),
            @"tap_private_requested": @(capture->tap_private),
            @"physical_buffer_before_tap": @(capture->buffer_before_tap),
            @"explicit_output_binding": @(capture->explicit_master),
            @"requested_buffer_frames": capture->requested_frames == UINT32_MAX ? (id)[NSNull null] : @(capture->requested_frames),
            @"timeline": capture->diagnostic_stages ? (__bridge NSArray *)capture->diagnostic_stages : @[] };
        NSData *json = [NSJSONSerialization dataWithJSONObject:report options:0 error:NULL];
        if (!json) return 0;
        size_t required = json.length + 1;
        if (buffer && capacity >= required) {
            memcpy(buffer, json.bytes, json.length);
            buffer[json.length] = 0;
        }
        return required;
    }
}

void *oink_audio_capture_start(bool mute, uint32_t requested_frames, uint32_t capture_mode, const char *selected_uid, bool buffer_before_tap, bool tap_private, bool collect_diagnostics, bool realtime, double *sample_rate, char *error, size_t error_size) {
    (void)mute; // ABI retained; the durable endpoint lease owns local silence.
    @autoreleasepool {
        if (oink_audio_capture_cleanup_failed()) {
            snprintf(error, error_size, "Capture disabled after cleanup failure; tap/mute release may be incomplete. Exit this process, verify local audio, then restart the application");
            return NULL;
        }
        if (@available(macOS 14.2, *)) {
            OSStatus watch_status = capture_watch_service();
            if (watch_status != noErr) {
                snprintf(error, error_size, "Watch audio service restart: Core Audio status %d", (int)watch_status);
                return NULL;
            }
            OinkCapture *capture = calloc(1, sizeof(*capture));
            if (!capture) { snprintf(error, error_size, "Audio capture allocation failed"); return NULL; }
            capture->service_epoch = atomic_load_explicit(&service_epoch, memory_order_acquire);
            mach_timebase_info(&capture->timebase);
            atomic_init(&capture->read_pos, 0);
            atomic_init(&capture->write_pos, 0);
            atomic_init(&capture->closing, false);
            atomic_init(&capture->wake_pending, false);
            atomic_init(&capture->trimmed_frames, 0);
            const char *stage = "Resolve Oink audio process";
            char detail[256] = {0};
            CATapDescription *description = nil;
            NSDictionary *aggregate = nil;
            NSString *physical_uid = nil;
            capture->info.capture_mode = capture_mode;
            capture->explicit_master = selected_uid != NULL;
            capture->buffer_before_tap = buffer_before_tap;
            capture->tap_private = tap_private;
            capture->requested_frames = requested_frames;
            if (collect_diagnostics)
                capture->diagnostic_stages = CFArrayCreateMutable(NULL, 0, &kCFTypeArrayCallBacks);
            pid_t pid = getpid();
            AudioObjectID own_process = kAudioObjectUnknown;
            UInt32 size = sizeof(own_process);
            AudioObjectPropertyAddress address = {
                kAudioHardwarePropertyTranslatePIDToProcessObject,
                kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain
            };
            OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address,
                sizeof(pid), &pid, &size, &own_process);
            if (status != noErr) goto failed;
            if (realtime) {
                stage = "Create capture realtime wake";
                status = semaphore_create(mach_task_self(), &capture->wake, SYNC_POLICY_FIFO, 0);
                if (status != KERN_SUCCESS) goto failed;
            }
            if (own_process == kAudioObjectUnknown) {
                status = kAudioHardwareBadObjectError;
                goto failed;
            }
            capture_resolve_physical(capture, selected_uid);
            if (selected_uid && !capture->info.physical_device) {
                stage = "Resolve explicitly selected capture master UID (default output unchanged)";
                status = capture->info.physical_buffer_request_status;
                goto failed;
            }
            capture_record_snapshot(capture, @"physical_before_lease");
            if (physical_master(capture_mode)) {
                stage = "Physical clock master must be an output-only endpoint (duplex devices rejected)";
                status = require_output_only_device(capture->info.physical_device);
                if (status != noErr) goto failed;
            }
            if (device_scope(capture_mode)) {
                UInt32 stream_count = 0;
                status = require_single_output_stream(capture->info.physical_device, &stream_count);
                snprintf(detail, sizeof(detail), "Device-specific capture requires exactly one output stream; found %u (multi-stream capture unsupported)", stream_count);
                stage = detail;
                if (status != noErr) goto failed;
            }
            if (physical_master(capture_mode) || device_scope(capture_mode)) {
                CFStringRef uid = NULL;
                address.mSelector = kAudioDevicePropertyDeviceUID;
                size = sizeof(uid);
                stage = "Resolve selected physical output UID";
                status = AudioObjectGetPropertyData(capture->info.physical_device, &address, 0, NULL, &size, &uid);
                if (status != noErr) goto failed;
                physical_uid = CFBridgingRelease(uid);
                if (!physical_uid) { status = kAudioHardwareBadDeviceError; goto failed; }
            }
            if (buffer_before_tap) capture_physical_lease(capture, requested_frames);
            capture_record_snapshot(capture, @"physical_at_tap_creation");
            if (device_scope(capture_mode)) {
                description = [[CATapDescription alloc] initExcludingProcesses:@[@(own_process)]
                    andDeviceUID:physical_uid withStream:0];
            } else {
                description = [[CATapDescription alloc]
                    initStereoGlobalTapButExcludeProcesses:@[@(own_process)]];
            }
            description.name = @"Oink system audio";
            description.privateTap = tap_private;
            // A long-lived CATapMutedWhenTapped tap on macOS 14 can capture a
            // newly started output client without muting its original output.
            // Endpoint mute covers future clients too. Measurements on macOS
            // 14/26 confirm process-tap PCM is upstream of hardware mute/level.
            description.muteBehavior = CATapUnmuted;
            stage = "Create system audio tap (requires System Audio Recording permission)";
            status = AudioHardwareCreateProcessTap(description, &capture->tap);
            if (status != noErr) goto failed;
            capture->tap_uid = CFBridgingRetain(description.UUID.UUIDString);
            capture_record_snapshot(capture, @"after_tap_creation");
            AudioStreamBasicDescription format = {0};
            address.mSelector = kAudioTapPropertyFormat;
            size = sizeof(format);
            stage = "Read system audio format";
            status = AudioObjectGetPropertyData(capture->tap, &address, 0, NULL, &size, &format);
            if (status != noErr) goto failed;
            if (format.mFormatID != kAudioFormatLinearPCM ||
                !(format.mFormatFlags & kAudioFormatFlagIsFloat) ||
                (format.mFormatFlags & kAudioFormatFlagIsBigEndian) ||
                format.mBitsPerChannel != 32 || format.mChannelsPerFrame != 2 ||
                format.mSampleRate < 8000 || format.mSampleRate > 192000) {
                stage = "Unsupported system audio format (expected float32 stereo)";
                status = kAudioHardwareUnsupportedOperationError;
                goto failed;
            }
            aggregate = @{
                @kAudioAggregateDeviceNameKey: @"Oink private audio capture",
                @kAudioAggregateDeviceUIDKey: [[NSUUID UUID] UUIDString],
                @kAudioAggregateDeviceIsPrivateKey: @YES,
                @kAudioAggregateDeviceTapAutoStartKey: @YES,
                @kAudioAggregateDeviceTapListKey: @[@{
                    @kAudioSubTapUIDKey: description.UUID.UUIDString,
                    @kAudioSubTapDriftCompensationKey: @(tap_drift(capture_mode))
                }]
            };
            if (physical_master(capture_mode)) {
                aggregate = @{
                    @kAudioAggregateDeviceNameKey: @"Oink experimental device-clock capture",
                    @kAudioAggregateDeviceUIDKey: [[NSUUID UUID] UUIDString],
                    @kAudioAggregateDeviceIsPrivateKey: @YES,
                    @kAudioAggregateDeviceTapAutoStartKey: @YES,
                    @kAudioAggregateDeviceMainSubDeviceKey: physical_uid,
                    @kAudioAggregateDeviceSubDeviceListKey: @[@{
                        @kAudioSubDeviceUIDKey: physical_uid,
                        @kAudioSubDeviceInputChannelsKey: @0,
                        @kAudioSubDeviceDriftCompensationKey: @NO
                    }],
                    @kAudioAggregateDeviceTapListKey: @[@{
                        @kAudioSubTapUIDKey: description.UUID.UUIDString,
                        @kAudioSubTapDriftCompensationKey: @(tap_drift(capture_mode))
                    }]
                };
            }
            capture->aggregate_uid = CFBridgingRetain(aggregate[@kAudioAggregateDeviceUIDKey]);
            stage = "Create private audio aggregate";
            status = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)aggregate,
                                                        &capture->aggregate);
            if (status != noErr) goto failed;
            capture->tap_sample_rate = format.mSampleRate;
            capture_record_snapshot(capture, @"after_aggregate_creation_before_buffer_request");
            if (!buffer_before_tap) capture_physical_lease(capture, requested_frames);
            // Configure the private aggregate separately from the source lease.
            capture->low_latency = requested_frames != UINT32_MAX;
            if (capture->low_latency) {
                AudioValueRange range = {0};
                address.mSelector = kAudioDevicePropertyBufferFrameSizeRange;
                size = sizeof(range);
                if (AudioObjectGetPropertyData(capture->aggregate, &address, 0, NULL, &size, &range) == noErr) {
                    if (requested_frames < range.mMinimum) requested_frames = (UInt32)range.mMinimum;
                    if (requested_frames > range.mMaximum) requested_frames = (UInt32)range.mMaximum;
                } else if (requested_frames == 0) {
                    requested_frames = 128;
                }
                address.mSelector = kAudioDevicePropertyBufferFrameSize;
                capture->info.buffer_request_status = AudioObjectSetPropertyData(capture->aggregate,
                    &address, 0, NULL, sizeof(requested_frames), &requested_frames);
            }
            stage = "Read actual aggregate input IO format (float32 stereo required)";
            status = capture_io_format(capture, &capture->sample_rate);
            if (status != noErr) goto failed;
            stage = "Register audio capture callback";
            status = AudioDeviceCreateIOProcID(capture->aggregate, capture_io, capture, &capture->io);
            if (status != noErr) goto failed;
            if (physical_master(capture_mode)) {
                stage = "Configure tap-only input and disable all aggregate outputs";
                status = configure_clock_only_capture(capture);
                if (status != noErr) goto failed;
            }
            capture_record_snapshot(capture, @"before_io_start_after_buffer_requests");
            stage = "Start system audio recording (check System Audio Recording permission)";
            status = AudioDeviceStart(capture->aggregate, capture->io);
            if (status != noErr) goto failed;
            capture->started = true;
            capture_info(capture);
            capture_record_snapshot(capture, @"after_io_start");
            *sample_rate = capture->sample_rate;
            return capture;
        failed:
            snprintf(error, error_size, "%s%s: Core Audio status %d",
                status == kAudioDevicePermissionsError ? "capture_permission_denied: " : "", stage, (int)status);
            char cleanup_error[768] = {0};
            int32_t cleanup_status = oink_audio_capture_stop(capture, cleanup_error, sizeof(cleanup_error));
            if (cleanup_status != noErr) {
                size_t used = strlen(error);
                if (used < error_size) snprintf(error + used, error_size - used,
                    "; capture cleanup failed; tap/mute release may be incomplete; capture disabled for this process. Exit, verify local audio, then restart the application: %s", cleanup_error);
            }
            return NULL;
        }
        snprintf(error, error_size, "System audio forwarding requires macOS 14.2 or newer");
        return NULL;
    }
}

// Pure classification shared by tap and aggregate IO health checks.
int32_t oink_audio_capture_format_health(int32_t status, bool supported, bool changed) {
    // A readable supported rate change retires only the physical capture,
    // just like a default switch. Returning terminal error would end the owner
    // ticket and leave forwarding enabled without a working source. Keep read
    // failures/unsupported shapes terminal; no topology or format fallback.
    if (status != noErr || !supported) return 1;
    return changed ? 2 : 0;
}

// Invoked by the consumer worker, never by HAL's real-time callback.
// 0 healthy, 1 terminal error, 2 default output/service reset (caller rebuilds).
int32_t oink_audio_capture_check(void *opaque, char *error, size_t error_size) {
    OinkCapture *capture = opaque;
    if (capture->service_epoch != atomic_load_explicit(&service_epoch, memory_order_acquire)) {
        snprintf(error, error_size, "Audio service restarted; verify old tap/aggregate absence and rebuild selected topology");
        return 2;
    }
    AudioObjectPropertyAddress address = {
        kAudioDevicePropertyDeviceIsAlive,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain
    };
    UInt32 alive = 0;
    UInt32 size = sizeof(alive);
    if ((device_scope(capture->info.capture_mode) || physical_master(capture->info.capture_mode)) && !capture->explicit_master) {
        AudioObjectPropertyAddress default_address = {kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        AudioDeviceID current = kAudioObjectUnknown;
        UInt32 current_size = sizeof(current);
        OSStatus current_status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &default_address, 0, NULL, &current_size, &current);
        if (current_status == noErr && current != capture->info.physical_device) {
            snprintf(error, error_size, "System default output changed; release old capture and restart the same selected topology");
            return 2;
        }
    }
    OSStatus status = AudioObjectGetPropertyData(capture->aggregate, &address, 0, NULL, &size, &alive);
    if (status != noErr || !alive) {
        snprintf(error, error_size, "Audio capture aggregate unavailable: Core Audio status %d", (int)status);
        if ((status == kAudioHardwareBadObjectError || status == kAudioHardwareBadDeviceError)
            && capture_objects_absent(capture)) {
            snprintf(error, error_size, "Old capture objects disappeared; verify cleanup and rebuild selected topology");
            return 2;
        }
        return 1;
    }
    if (capture->explicit_master) {
        if (device_frames(capture->info.physical_device,
            kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal) != 1) {
            snprintf(error, error_size, "Explicitly selected capture master is unavailable; restart capture");
            return 1;
        }
        double physical_rate = 0;
        address.mSelector = kAudioDevicePropertyNominalSampleRate;
        size = sizeof(physical_rate);
        status = AudioObjectGetPropertyData(capture->info.physical_device, &address, 0, NULL, &size, &physical_rate);
        if (status != noErr || physical_rate != capture->info.physical_sample_rate) {
            snprintf(error, error_size, "Explicitly selected capture master format changed; restart capture");
            return 1;
        }
    }
    if (device_scope(capture->info.capture_mode)) {
        UInt32 streams = 0;
        status = require_single_output_stream(capture->info.physical_device, &streams);
        if (status != noErr) {
            snprintf(error, error_size, "Selected output topology changed: %u output streams; exactly one required (status %d)", streams, (int)status);
            return 1;
        }
    }
    AudioStreamBasicDescription format = {0};
    address.mSelector = kAudioTapPropertyFormat;
    size = sizeof(format);
    status = AudioObjectGetPropertyData(capture->tap, &address, 0, NULL, &size, &format);
    int32_t format_health = oink_audio_capture_format_health(status,
        format.mFormatID == kAudioFormatLinearPCM &&
        (format.mFormatFlags & kAudioFormatFlagIsFloat) &&
        !(format.mFormatFlags & kAudioFormatFlagIsBigEndian) &&
        format.mBitsPerChannel == 32 && format.mChannelsPerFrame == 2,
        format.mSampleRate != capture->tap_sample_rate);
    if (format_health != 0) {
        snprintf(error, error_size, "Audio capture tap unavailable or format changed; restart capture (status %d)", (int)status);
        return format_health;
    }
    double io_rate = 0;
    status = capture_io_format(capture, &io_rate);
    format_health = oink_audio_capture_format_health(status, true, io_rate != capture->sample_rate);
    if (format_health != 0) {
        snprintf(error, error_size, "Aggregate input IO format changed; restart capture (status %d)", (int)status);
        return format_health;
    }
    return 0;
}

size_t oink_audio_capture_read(void *opaque, float *output, size_t capacity_frames,
                                uint64_t *host_ns, uint64_t *callback_ns, uint64_t *sequence) {
    OinkCapture *capture = opaque;
    uint64_t read = atomic_load_explicit(&capture->read_pos, memory_order_relaxed);
    uint64_t write = atomic_load_explicit(&capture->write_pos, memory_order_acquire);
    size_t frames = (size_t)(write - read);
    if (frames > capacity_frames) frames = capacity_frames;
    // Drop stale backlog on the consumer side, preserving SPSC ownership.
    // A sequence jump tells Rust to discard its interpolation carry frame.
    size_t backlog_limit = capture->info.buffer_frames == UINT32_MAX ? 256 : capture->info.buffer_frames * 2;
    if (backlog_limit < 256) backlog_limit = 256;
    if (capture->low_latency && write - read > backlog_limit) {
        capture->trimmed_frames += write - read - backlog_limit;
        read = write - backlog_limit;
        frames = backlog_limit < capacity_frames ? backlog_limit : capacity_frames;
    }
    if (!frames) return 0;
    *host_ns = capture->host_ns[read & (RING_FRAMES - 1)];
    *sequence = capture->sequence[read & (RING_FRAMES - 1)];
    *callback_ns = capture->callback_ns[read & (RING_FRAMES - 1)];
    for (size_t i = 0; i < frames; ++i) {
        size_t index = ((read + i) & (RING_FRAMES - 1)) * 2;
        // Do not splice across ring overflow or timestamp-validity changes.
        if (capture->sequence[index / 2] != *sequence + i ||
            capture->callback_ns[index / 2] != *callback_ns ||
            (capture->host_ns[index / 2] == 0) != (*host_ns == 0)) {
            frames = i;
            break;
        }
        output[i * 2] = capture->samples[index];
        output[i * 2 + 1] = capture->samples[index + 1];
    }
    atomic_store_explicit(&capture->read_pos, read + frames, memory_order_release);
    return frames;
}

// Called exclusively by the Rust capture consumer, never by the IOProc.
size_t oink_audio_capture_events(void *opaque, OinkCaptureEvent *events, size_t capacity,
                                  uint64_t *dropped, uint64_t *trimmed, uint64_t *audio_dropped) {
    OinkCapture *capture = opaque;
    uint64_t read = atomic_load_explicit(&capture->event_read, memory_order_relaxed);
    uint64_t write = atomic_load_explicit(&capture->event_write, memory_order_acquire);
    size_t count = write - read;
    if (count > capacity) count = capacity;
    for (size_t i = 0; i < count; i++) events[i] = capture->events[(read + i) & (EVENT_CAPACITY - 1)];
    atomic_store_explicit(&capture->event_read, read + count, memory_order_release);
    *dropped = atomic_load_explicit(&capture->event_dropped, memory_order_relaxed);
    *trimmed = capture->trimmed_frames;
    *audio_dropped = atomic_load_explicit(&capture->audio_dropped_frames, memory_order_relaxed);
    return count;
}

// Read-only scheduling inspection off the callback thread. No priority changes.
void oink_audio_thread_scheduling(void *opaque, int32_t *qos, int32_t *relative,
                                    int32_t *policy, int32_t *priority, int32_t *error) {
    // Store the existing Mach port name, not a pthread_t pointer that could be
    // freed if CoreAudio replaces its IO thread. Mach queries fail safely when
    // that port is gone. pthread_mach_thread_np does not allocate a send right.
    thread_t thread = opaque ? atomic_load_explicit(
        &((OinkCapture *)opaque)->callback_thread, memory_order_relaxed) : pthread_mach_thread_np(pthread_self());
    *qos = -1; *relative = 0; *policy = 0; *priority = 0; *error = -1;
    if (!thread) return;
    if (!opaque) {
        qos_class_t native_qos = QOS_CLASS_UNSPECIFIED;
        int qos_error = pthread_get_qos_class_np(pthread_self(), &native_qos, relative);
        *qos = qos_error ? -qos_error : (int32_t)native_qos;
    }
    thread_extended_info_data_t info = {0};
    mach_msg_type_number_t count = THREAD_EXTENDED_INFO_COUNT;
    *error = thread_info(thread, THREAD_EXTENDED_INFO, (thread_info_t)&info, &count);
    if (*error == KERN_SUCCESS) {
        *policy = info.pth_policy;
        *priority = info.pth_curpri;
    }
}
