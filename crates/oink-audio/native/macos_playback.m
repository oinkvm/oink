// Direct HAL output: mHostTime is the time the first frame is passed to hardware,
// not callback entry and not an acoustic measurement. Never add the IO buffer twice.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#include <mach/mach_time.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

typedef struct {
    double sample_rate;
    uint32_t channels;
    uint32_t original_buffer_frames;
    uint32_t buffer_frames;
    uint32_t latency_frames;
    uint32_t safety_frames;
    uint32_t stream_latency_frames;
    int32_t buffer_request_status;
    uint32_t lease_changed;
} OinkOutputInfo;
typedef void (*OinkRender)(void *, float *, size_t, uint32_t, uint32_t, uint32_t, uint64_t, uint64_t);
extern uint64_t oink_audio_service_generation(void);
typedef struct {
    uint64_t service_generation;
    atomic_bool closing;
    AudioDeviceID device;
    AudioDeviceIOProcID io;
    bool started;
    OinkRender render;
    void *context;
    OinkOutputInfo info;
    float *scratch;
    uint32_t capacity_frames;
    mach_timebase_info_data_t timebase;
    _Atomic int failure;
} OinkOutput;

static uint64_t host_ns(OinkOutput *output, uint64_t ticks) {
    return (uint64_t)(((__uint128_t)ticks * output->timebase.numer) / output->timebase.denom);
}
static uint32_t property_frames(AudioObjectID object, AudioObjectPropertySelector selector,
                                AudioObjectPropertyScope scope) {
    AudioObjectPropertyAddress address = {selector, scope, kAudioObjectPropertyElementMain};
    uint32_t result = UINT32_MAX;
    UInt32 size = sizeof(result);
    if (AudioObjectGetPropertyData(object, &address, 0, NULL, &size, &result) != noErr) return UINT32_MAX;
    return result;
}
// Read-only enumeration: no IOProc, device start, buffer lease or TCC request.
double oink_audio_output_latency_ms(const char *uid_utf8) {
    @autoreleasepool {
        NSString *uid_string = [NSString stringWithUTF8String:uid_utf8];
        CFStringRef uid = (__bridge CFStringRef)uid_string;
        AudioDeviceID device = kAudioObjectUnknown;
        AudioObjectPropertyAddress address = {kAudioHardwarePropertyTranslateUIDToDevice,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        UInt32 size = sizeof(device);
        if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, sizeof(uid), &uid, &size, &device) != noErr || !device) return -1;
        Float64 rate = 0;
        address.mSelector = kAudioDevicePropertyNominalSampleRate;
        size = sizeof(rate);
        if (AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &rate) != noErr || rate <= 0) return -1;
        uint32_t latency = property_frames(device, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput);
        if (latency == UINT32_MAX) return -1;
        address.mSelector = kAudioDevicePropertyStreams;
        address.mScope = kAudioObjectPropertyScopeOutput;
        if (AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size) != noErr || !size) return -1;
        AudioStreamID *streams = malloc(size);
        if (!streams) return -1;
        if (AudioObjectGetPropertyData(device, &address, 0, NULL, &size, streams) != noErr) { free(streams); return -1; }
        uint32_t stream_latency = 0;
        for (UInt32 i = 0; i < size / sizeof(AudioStreamID); i++) {
            uint32_t frames = property_frames(streams[i], kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal);
            if (frames == UINT32_MAX) { free(streams); return -1; }
            if (frames > stream_latency) stream_latency = frames;
        }
        free(streams);
        return ((double)latency + stream_latency) * 1000.0 / rate;
    }
}
static OSStatus render_io(AudioObjectID device, const AudioTimeStamp *now,
                           const AudioBufferList *input, const AudioTimeStamp *input_time,
                           AudioBufferList *buffers, const AudioTimeStamp *output_time, void *context) {
    (void)device; (void)now; (void)input; (void)input_time;
    OinkOutput *output = context;
    if (atomic_load_explicit(&output->closing, memory_order_acquire)) return noErr;
    const uint64_t callback_ns = host_ns(output, mach_absolute_time());
    if (!buffers || !buffers->mNumberBuffers) return noErr;
    uint32_t channels = 0, frames = UINT32_MAX;
    for (UInt32 b = 0; b < buffers->mNumberBuffers; ++b) {
        const AudioBuffer *buffer = &buffers->mBuffers[b];
        if (!buffer->mNumberChannels || !buffer->mData) {
            atomic_store_explicit(&output->failure, 1, memory_order_relaxed);
            return noErr;
        }
        uint32_t count = buffer->mDataByteSize / (sizeof(float) * buffer->mNumberChannels);
        if (count < frames) frames = count;
        channels += buffer->mNumberChannels;
    }
    if (channels != output->info.channels || frames > output->capacity_frames) {
        atomic_store_explicit(&output->failure, 1, memory_order_relaxed);
        return noErr; // HAL zeroes output memory before calling us.
    }
    uint64_t playback_ns = output_time && (output_time->mFlags & kAudioTimeStampHostTimeValid)
        ? host_ns(output, output_time->mHostTime) : 0;
    memset(output->scratch, 0, (size_t)frames * channels * sizeof(float));
    output->render(output->context, output->scratch, (size_t)frames * channels,
                   (uint32_t)output->info.sample_rate, channels, frames, callback_ns, playback_ns);
    uint32_t channel_offset = 0;
    for (UInt32 b = 0; b < buffers->mNumberBuffers; ++b) {
        AudioBuffer *buffer = &buffers->mBuffers[b];
        float *destination = buffer->mData;
        for (uint32_t f = 0; f < frames; ++f)
            for (uint32_t c = 0; c < buffer->mNumberChannels; ++c)
                destination[(size_t)f * buffer->mNumberChannels + c] = output->scratch[(size_t)f * channels + channel_offset + c];
        channel_offset += buffer->mNumberChannels;
    }
    return noErr;
}

// Quiesce IO before freeing the Rust context. Restore only if the buffer still
// equals our lease value. A crash cannot run this; same-value external writes
// cannot be distinguished by CoreAudio. The property affects all device users.
int32_t oink_audio_output_stop(void *opaque, bool *released) {
    *released = true;
    if (!opaque) return noErr;
    OinkOutput *output = opaque;
    atomic_store_explicit(&output->closing, true, memory_order_release);
    // Object IDs belong to a service incarnation. Never restore a buffer lease
    // or destroy an IOProc using a possibly recycled ID after srst. If teardown
    // cannot prove quiescence, retain native AND Rust context for late callbacks.
    // Recovery is notification based; actual reset validation remains manual.
    if (output->service_generation != oink_audio_service_generation() && output->io) {
        *released = false;
        return noErr;
    }
    OSStatus stopped = output->started ? AudioDeviceStop(output->device, output->io) : noErr;
    OSStatus destroyed = output->io ? AudioDeviceDestroyIOProcID(output->device, output->io) : noErr;
    if (stopped != noErr || destroyed != noErr) {
        *released = false;
        return stopped != noErr ? stopped : destroyed;
    }
    OSStatus status = noErr;
    if (output->service_generation == oink_audio_service_generation() &&
        output->info.lease_changed && property_frames(output->device, kAudioDevicePropertyBufferFrameSize,
        kAudioObjectPropertyScopeGlobal) == output->info.buffer_frames) {
        AudioObjectPropertyAddress address = {kAudioDevicePropertyBufferFrameSize,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        status = AudioObjectSetPropertyData(output->device, &address, 0, NULL,
            sizeof(output->info.original_buffer_frames), &output->info.original_buffer_frames);
    }
    free(output->scratch);
    free(output);
    return status;
}

void *oink_audio_output_start(const char *uid_utf8, uint32_t requested, OinkRender render,
                               void *context, OinkOutputInfo *info, bool *retained, char *error, size_t error_size) {
    @autoreleasepool {
        *retained = false;
        OinkOutput *output = calloc(1, sizeof(*output));
        if (!output) { snprintf(error, error_size, "Output allocation failed"); return NULL; }
        output->service_generation = oink_audio_service_generation();
        atomic_init(&output->closing, false);
        output->render = render;
        output->context = context;
        mach_timebase_info(&output->timebase);
        atomic_init(&output->failure, 0);
        const char *stage = "Resolve output UID";
        NSString *uid_string = [NSString stringWithUTF8String:uid_utf8];
        CFStringRef uid = (__bridge CFStringRef)uid_string;
        AudioObjectPropertyAddress address = {kAudioHardwarePropertyTranslateUIDToDevice,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        UInt32 size = sizeof(output->device);
        OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address,
            sizeof(uid), &uid, &size, &output->device);
        AudioStreamID *streams = NULL;
        AudioBufferList *configuration = NULL;
        if (status != noErr || !output->device) { status = kAudioHardwareBadDeviceError; goto failed; }
        stage = "Read output stream configuration";
        address.mSelector = kAudioDevicePropertyStreamConfiguration;
        address.mScope = kAudioObjectPropertyScopeOutput;
        status = AudioObjectGetPropertyDataSize(output->device, &address, 0, NULL, &size);
        if (status != noErr) goto failed;
        configuration = malloc(size);
        if (!configuration) { status = kAudioHardwareUnspecifiedError; goto failed; }
        status = AudioObjectGetPropertyData(output->device, &address, 0, NULL, &size, configuration);
        if (status != noErr) goto failed;
        for (UInt32 b = 0; b < configuration->mNumberBuffers; ++b)
            output->info.channels += configuration->mBuffers[b].mNumberChannels;
        free(configuration); configuration = NULL;
        if (!output->info.channels || output->info.channels > 64) { status = kAudioHardwareUnsupportedOperationError; goto failed; }
        address.mSelector = kAudioDevicePropertyStreams;
        status = AudioObjectGetPropertyDataSize(output->device, &address, 0, NULL, &size);
        if (status != noErr) goto failed;
        if (!size) { status = kAudioHardwareUnsupportedOperationError; goto failed; }
        streams = malloc(size);
        if (!streams) { status = kAudioHardwareUnspecifiedError; goto failed; }
        status = AudioObjectGetPropertyData(output->device, &address, 0, NULL, &size, streams);
        if (status != noErr) goto failed;
        UInt32 count = size / sizeof(AudioStreamID);
        stage = "Validate float32 HAL output format";
        output->info.stream_latency_frames = 0;
        for (UInt32 i = 0; i < count; ++i) {
            AudioStreamBasicDescription format = {0};
            address.mSelector = kAudioStreamPropertyVirtualFormat;
            address.mScope = kAudioObjectPropertyScopeGlobal;
            size = sizeof(format);
            status = AudioObjectGetPropertyData(streams[i], &address, 0, NULL, &size, &format);
            if (status != noErr) goto failed;
            if (format.mFormatID != kAudioFormatLinearPCM || !(format.mFormatFlags & kAudioFormatFlagIsFloat) ||
                (format.mFormatFlags & kAudioFormatFlagIsBigEndian) || format.mBitsPerChannel != 32 ||
                format.mBytesPerFrame != sizeof(float) * ((format.mFormatFlags & kAudioFormatFlagIsNonInterleaved) ? 1 : format.mChannelsPerFrame) ||
                format.mSampleRate < 8000 || format.mSampleRate > 192000 ||
                (i && format.mSampleRate != output->info.sample_rate)) {
                status = kAudioHardwareUnsupportedOperationError; goto failed;
            }
            output->info.sample_rate = format.mSampleRate;
            uint32_t latency = property_frames(streams[i], kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal);
            if (latency > output->info.stream_latency_frames) output->info.stream_latency_frames = latency;
        }
        free(streams); streams = NULL;
        output->info.original_buffer_frames = property_frames(output->device, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal);
        output->info.buffer_frames = output->info.original_buffer_frames;
        stage = "Read output buffer size";
        if (output->info.original_buffer_frames == UINT32_MAX) { status = kAudioHardwareUnsupportedOperationError; goto failed; }
        if (requested != UINT32_MAX) {
            AudioValueRange range = {0};
            address.mSelector = kAudioDevicePropertyBufferFrameSizeRange;
            size = sizeof(range);
            if (AudioObjectGetPropertyData(output->device, &address, 0, NULL, &size, &range) == noErr) {
                if (requested < range.mMinimum) requested = (uint32_t)range.mMinimum;
                if (requested > range.mMaximum) requested = (uint32_t)range.mMaximum;
            } else if (!requested) { requested = 128; }
            address.mSelector = kAudioDevicePropertyBufferFrameSize;
            output->info.buffer_request_status = AudioObjectSetPropertyData(output->device, &address, 0, NULL, sizeof(requested), &requested);
            output->info.buffer_frames = property_frames(output->device, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal);
            output->info.lease_changed = output->info.buffer_request_status == noErr &&
                output->info.buffer_frames != UINT32_MAX &&
                output->info.buffer_frames != output->info.original_buffer_frames;
        }
        output->info.latency_frames = property_frames(output->device, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput);
        output->info.safety_frames = property_frames(output->device, kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeOutput);
        output->capacity_frames = 16384; // Bounded allocation; no allocations in render_io.
        if (output->info.buffer_frames > output->capacity_frames) { status = kAudioHardwareUnsupportedOperationError; goto failed; }
        output->scratch = calloc((size_t)output->capacity_frames * output->info.channels, sizeof(float));
        if (!output->scratch) { status = kAudioHardwareUnspecifiedError; goto failed; }
        stage = "Register HAL output callback";
        status = AudioDeviceCreateIOProcID(output->device, render_io, output, &output->io);
        if (status != noErr) goto failed;
        // Do not enable microphone/input streams on duplex output devices.
        stage = "Disable HAL input streams for output-only IOProc";
        address.mSelector = kAudioDevicePropertyStreams;
        address.mScope = kAudioObjectPropertyScopeInput;
        size = 0;
        status = AudioObjectGetPropertyDataSize(output->device, &address, 0, NULL, &size);
        if (status != noErr) goto failed;
        if (size) {
            UInt32 input_count = size / sizeof(AudioStreamID);
            UInt32 usage_size = sizeof(AudioHardwareIOProcStreamUsage) + (input_count - 1) * sizeof(UInt32);
            AudioHardwareIOProcStreamUsage *usage = calloc(1, usage_size);
            if (!usage) { status = kAudioHardwareUnspecifiedError; goto failed; }
            usage->mIOProc = (void *)output->io;
            usage->mNumberStreams = input_count;
            address.mSelector = kAudioDevicePropertyIOProcStreamUsage;
            status = AudioObjectSetPropertyData(output->device, &address, 0, NULL, usage_size, usage);
            free(usage);
            if (status != noErr) goto failed;
        }
        stage = "Start HAL output";
        status = AudioDeviceStart(output->device, output->io);
        if (status != noErr) goto failed;
        output->started = true;
        *info = output->info;
        return output;
    failed:
        snprintf(error, error_size, "%s: Core Audio status %d", stage, (int)status);
        free(configuration);
        free(streams);
        bool released;
        int32_t restore_status = oink_audio_output_stop(output, &released);
        *retained = !released;
        if (restore_status != noErr) {
            size_t used = strlen(error);
            if (used < error_size) snprintf(error + used, error_size - used, "; buffer restoration failed: %d", restore_status);
        }
        return NULL;
    }
}

int32_t oink_audio_output_check(void *opaque, char *error, size_t error_size) {
    OinkOutput *output = opaque;
    if (output->service_generation != oink_audio_service_generation()) {
        snprintf(error, error_size, "HAL output service restarted"); return 2;
    }
    if (atomic_load_explicit(&output->failure, memory_order_relaxed)) {
        snprintf(error, error_size, "HAL output layout changed or callback exceeded capacity"); return 1;
    }
    UInt32 alive = 0, size = sizeof(alive);
    AudioObjectPropertyAddress address = {kAudioDevicePropertyDeviceIsAlive,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    OSStatus status = AudioObjectGetPropertyData(output->device, &address, 0, NULL, &size, &alive);
    if (status == kAudioHardwareBadDeviceError || status == kAudioHardwareBadObjectError ||
        (status == noErr && !alive)) {
        snprintf(error, error_size, "HAL output device is unavailable"); return 2;
    }
    if (status != noErr) {
        snprintf(error, error_size, "HAL output health query failed: %d", (int)status); return 1;
    }
    Float64 rate = 0;
    size = sizeof(rate);
    address.mSelector = kAudioDevicePropertyNominalSampleRate;
    if (AudioObjectGetPropertyData(output->device, &address, 0, NULL, &size, &rate) != noErr || rate != output->info.sample_rate) {
        snprintf(error, error_size, "HAL output sample rate changed; restart playback"); return 1;
    }
    return 0;
}

// Ordinary-thread query while the owned stream remains live. Do not re-resolve
// the default device: the auxiliary worker must join this IOProc's workgroup.
uint32_t oink_audio_output_device_id(void *opaque) {
    OinkOutput *output = opaque;
    return output ? output->device : kAudioObjectUnknown;
}
