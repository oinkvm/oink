#import "OinkAudioIO.h"
#import <AVFAudio/AVFAudio.h>
#import <mach/mach_time.h>
#include <atomic>
#include <memory>
#include <thread>

// Apple's legacy marker is deprecated in favor of InterruptionReasonKey. Read
// both when handling a delayed system event, without spreading deprecated API.
BOOL OinkAudioInterruptionWasSuspended(NSNotification *notification) {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    BOOL legacy = [notification.userInfo[AVAudioSessionInterruptionWasSuspendedKey] boolValue];
    NSNumber *reason = notification.userInfo[AVAudioSessionInterruptionReasonKey];
    return legacy || (reason && reason.unsignedIntegerValue == AVAudioSessionInterruptionReasonAppWasSuspended);
#pragma clang diagnostic pop
}

namespace {
constexpr uint32_t maxFrames = 4096;
constexpr uint64_t closed = uint64_t(1) << 63;
struct State {
    OinkAudioBackend backend;
    void *output = nullptr, *input = nullptr;
    std::atomic<uint64_t> admission{0}, rendered{0}, captured{0};
    std::atomic<uint32_t> maxOutputBatch{0}, maxInputBatch{0};
    double outputRate = 0, inputRate = 0;
    uint32_t inputChannels = 0, nativeInputChannels = 0;
    bool inputInterleaved = false;
    mach_timebase_info_data_t timebase;
    explicit State(OinkAudioBackend b) : backend(b) { mach_timebase_info(&timebase); }
    bool enter() {
        auto value = admission.load(std::memory_order_acquire);
        while (!(value & closed)) {
            if (admission.compare_exchange_weak(value, value + 1,
                    std::memory_order_acq_rel, std::memory_order_acquire)) return true;
        }
        return false;
    }
    void leave() { admission.fetch_sub(1, std::memory_order_release); }
    uint64_t ns(uint64_t host) const {
        return uint64_t((__uint128_t(host) * timebase.numer) / timebase.denom);
    }
    void drain() {
        admission.fetch_or(closed, std::memory_order_acq_rel);
        while (admission.load(std::memory_order_acquire) != closed) std::this_thread::yield();
    }
};
NSError *failure(NSString *message) {
    return [NSError errorWithDomain:@"OinkAudioIO" code:1
        userInfo:@{NSLocalizedDescriptionKey:message}];
}
}

@implementation OinkAudioIO {
    OinkAudioBackend _backend;
    AVAudioEngine *_engine;
    std::shared_ptr<State> _state;
}
- (instancetype)initWithBackend:(OinkAudioBackend)backend {
    if ((self = [super init])) _backend = backend;
    return self;
}
- (BOOL)startWithPlayback:(BOOL)playback microphone:(BOOL)microphone
                   epoch:(uint64_t)epoch error:(NSError **)error {
    NSAssert([NSThread isMainThread], @"audio control must be serialized on main");
    [self stop];
    _engine = [AVAudioEngine new];
    auto state = std::make_shared<State>(_backend);
    _state = state;
    state->outputRate = [_engine.outputNode inputFormatForBus:0].sampleRate;
    AVAudioFormat *inputFormat = microphone ? [_engine.inputNode outputFormatForBus:0] : nil;
    state->inputRate = inputFormat.sampleRate;
    state->nativeInputChannels = inputFormat.channelCount;
    state->inputChannels = MIN(state->nativeInputChannels, 2u);
    state->inputInterleaved = inputFormat.interleaved;
    if (state->outputRate <= 0 || (microphone &&
        (state->inputRate <= 0 || !state->inputChannels || inputFormat.commonFormat != AVAudioPCMFormatFloat32))) {
        if (error) *error = failure(@"Audio route has no usable PCM format");
        [self stop]; return NO;
    }
    if (!_backend.prepare || !_backend.render || !_backend.capture || !_backend.releaseHandles ||
        !_backend.prepare(_backend.owner, playback ? state->outputRate : 0,
            state->inputRate, state->inputChannels, epoch, &state->output, &state->input)) {
        if (error) *error = failure(@"Audio media core is not prepared");
        [self stop]; return NO;
    }
    AVAudioFormat *format = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:state->outputRate channels:2];
    AVAudioSourceNode *source = [[AVAudioSourceNode alloc] initWithFormat:format renderBlock:
        ^OSStatus(BOOL *silent, const AudioTimeStamp *time, AVAudioFrameCount frames, AudioBufferList *list) {
            for (UInt32 b = 0; b < list->mNumberBuffers; ++b)
                memset(list->mBuffers[b].mData, 0, list->mBuffers[b].mDataByteSize);
            *silent = YES;
            if (!state->enter()) return noErr;
            if (playback && state->output) {
                float pcm[maxFrames * 2];
                const uint64_t host = (time->mFlags & kAudioTimeStampHostTimeValid) ? state->ns(time->mHostTime) : 0;
                for (uint32_t offset = 0; offset < frames; offset += maxFrames) {
                    const auto n = MIN(maxFrames, frames - offset);
                    memset(pcm, 0, n * 2 * sizeof(float));
                    state->backend.render(state->output, pcm, n,
                        host ? host + uint64_t(offset * 1e9 / state->outputRate) : 0);
                    for (uint32_t c = 0; c < MIN(list->mNumberBuffers, 2u); ++c) {
                        auto dst = static_cast<float *>(list->mBuffers[c].mData) + offset;
                        for (uint32_t i = 0; i < n; ++i) dst[i] = pcm[i * 2 + c];
                    }
                }
                *silent = NO;
            }
            if (frames > state->maxOutputBatch.load(std::memory_order_relaxed))
                state->maxOutputBatch.store(frames, std::memory_order_relaxed);
            state->rendered.fetch_add(frames, std::memory_order_relaxed);
            state->leave(); return noErr;
        }];
    [_engine attachNode:source];
    [_engine connect:source to:_engine.mainMixerNode format:format];
    if (microphone) {
        // On iOS 26.6.1, installTap(bufferSize:240) delivered 4800-frame
        // batches (100 ms at 48 kHz), including in the background. That exceeds
        // the core's bounded 20 ms handoff and loses speech. Sink receives the
        // actual input I/O cycle without tap batching or engine format conversion.
        AVAudioSinkNode *sink = [[AVAudioSinkNode alloc] initWithReceiverBlock:
            ^OSStatus(const AudioTimeStamp *time, AVAudioFrameCount frameCount, const AudioBufferList *list) {
                if (!state->enter()) return noErr;
                const uint32_t channels = state->inputChannels;
                const uint64_t host = (time->mFlags & kAudioTimeStampHostTimeValid) ? state->ns(time->mHostTime) : 0;
                float pcm[maxFrames * 2];
                for (uint32_t offset = 0; offset < frameCount; offset += maxFrames) {
                    const auto n = MIN(maxFrames, frameCount - offset);
                    for (uint32_t i = 0; i < n; ++i)
                        for (uint32_t c = 0; c < channels; ++c)
                            pcm[i * channels + c] = state->inputInterleaved
                                ? static_cast<const float *>(list->mBuffers[0].mData)[(offset + i) * state->nativeInputChannels + c]
                                : static_cast<const float *>(list->mBuffers[c].mData)[offset + i];
                    if (state->input) state->backend.capture(state->input, pcm, n,
                        host ? host + uint64_t(offset * 1e9 / state->inputRate) : 0);
                }
                if (frameCount > state->maxInputBatch.load(std::memory_order_relaxed))
                    state->maxInputBatch.store(frameCount, std::memory_order_relaxed);
                state->captured.fetch_add(frameCount, std::memory_order_relaxed);
                state->leave(); return noErr;
            }];
        [_engine attachNode:sink];
        [_engine connect:_engine.inputNode to:sink format:inputFormat];
    }
    [_engine prepare];
    if (![_engine startAndReturnError:error]) { [self stop]; return NO; }
    return YES;
}
- (void)stop {
    NSAssert([NSThread isMainThread], @"audio control must be serialized on main");
    // Close admission first: a late callback may still hold State, but can no
    // longer touch Rust handles. Drain on control thread, never the render thread.
    if (_state) {
        if (_backend.revoke && (_state->output || _state->input)) _backend.revoke(_state->output, _state->input);
        _state->admission.fetch_or(closed, std::memory_order_acq_rel);
    }
    [_engine stop];
    if (_state) {
        _state->drain();
        if (_backend.releaseHandles && (_state->output || _state->input))
            _backend.releaseHandles(_backend.owner, _state->output, _state->input);
        _state->output = nullptr; _state->input = nullptr;
    }
    _engine = nil;
    // Retain counters for diagnostics until the next start; State captured by
    // native blocks owns no Rust resources after the quiescence barrier.
}
- (BOOL)ownsEngine:(id)engine { return engine == _engine; }
- (uint64_t)renderedFrames { return _state ? _state->rendered.load() : 0; }
- (uint64_t)capturedFrames { return _state ? _state->captured.load() : 0; }
- (uint32_t)maximumOutputBatchFrames { return _state ? _state->maxOutputBatch.load() : 0; }
- (uint32_t)maximumInputBatchFrames { return _state ? _state->maxInputBatch.load() : 0; }
- (double)outputRate { return _state ? _state->outputRate : 0; }
- (double)inputRate { return _state ? _state->inputRate : 0; }
- (void)dealloc { if (_engine) [self stop]; }
@end
