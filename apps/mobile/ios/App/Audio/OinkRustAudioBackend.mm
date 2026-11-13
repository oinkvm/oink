#import "OinkRustAudioBackend.h"
#include "oink_mobile_audio.h"
#include <atomic>
#include <thread>
#include <pthread/qos.h>

namespace {
struct Input {
    OinkAudioInput *handle;
    dispatch_semaphore_t ready = dispatch_semaphore_create(0);
    std::atomic<bool> stopping{false}, wakePending{false};
    std::thread worker;
    Input(OinkAudioInput *h) : handle(h) {
        worker = std::thread([this] {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0);
            while (true) {
                dispatch_semaphore_wait(ready, DISPATCH_TIME_FOREVER);
                if (stopping.load(std::memory_order_acquire)) break;
                wakePending.store(false, std::memory_order_release);
                oink_audio_input_process(handle);
            }
        });
    }
    void stopWorker() {
        if (stopping.exchange(true, std::memory_order_acq_rel)) return;
        dispatch_semaphore_signal(ready);
        worker.join();
    }
    ~Input() { stopWorker(); oink_audio_input_destroy(handle); }
};
void render(void *opaque, float *pcm, uint32_t frames, uint64_t hostTimeNs) {
    oink_audio_output_pull_at(static_cast<OinkAudioOutput *>(opaque), pcm, frames, hostTimeNs);
}
void capture(void *opaque, const float *pcm, uint32_t frames, uint64_t ns) {
    auto input = static_cast<Input *>(opaque);
    if (oink_audio_input_push(input->handle, pcm, frames, ns) > 0 &&
        !input->wakePending.exchange(true, std::memory_order_acq_rel))
        dispatch_semaphore_signal(input->ready);
}
void revoke(void *out, void *in) {
    if (out) oink_audio_output_revoke(static_cast<OinkAudioOutput *>(out));
    if (in) oink_audio_input_revoke(static_cast<Input *>(in)->handle);
}
}

@implementation OinkRustAudioBackend {
    uint64_t _outputHandleAddress, _inputHandleAddress;
}
- (BOOL)prepareOutputRate:(double)outputRate inputRate:(double)inputRate
                channels:(uint32_t)channels generation:(uint64_t)generation
                  output:(void **)output input:(void **)input {
    // No live network binding is a preparation error, not a pretend live session.
    if (!self.attachHandles || !self.detachHandles) return NO;
    if (outputRate > 0) {
        auto handle = oink_audio_output_create(uint32_t(outputRate), 2, generation);
        if (!handle) return NO;
        *output = handle;
        _outputHandleAddress = reinterpret_cast<uint64_t>(handle);
    }
    if (inputRate > 0) {
        auto handle = oink_audio_input_create(uint32_t(inputRate), uint16_t(channels), generation);
        if (!handle) return NO;
        *input = new Input(handle);
        _inputHandleAddress = reinterpret_cast<uint64_t>(handle);
    }
    return self.attachHandles(generation, _outputHandleAddress, _inputHandleAddress);
}
- (void)releaseOutput:(void *)output input:(void *)input {
    if (input) static_cast<Input *>(input)->stopWorker();
    const auto outputAddress = _outputHandleAddress, inputAddress = _inputHandleAddress;
    _outputHandleAddress = 0; _inputHandleAddress = 0;
    if (output) {
        oink_audio_output_destroy(static_cast<OinkAudioOutput *>(output));
    }
    if (input) delete static_cast<Input *>(input);
    if (self.detachHandles) self.detachHandles(outputAddress, inputAddress);
}
- (OinkAudioBackend)callbacks {
    OinkAudioBackend backend{};
    backend.owner = (__bridge void *)self;
    backend.prepare = [](void *owner, double outputRate, double inputRate,
                         uint32_t channels, uint64_t generation, void **out, void **in) -> bool {
        return [(__bridge OinkRustAudioBackend *)owner prepareOutputRate:outputRate
            inputRate:inputRate channels:channels generation:generation output:out input:in];
    };
    backend.render = render;
    backend.capture = capture;
    backend.revoke = revoke;
    backend.releaseHandles = [](void *owner, void *out, void *in) {
        [(__bridge OinkRustAudioBackend *)owner releaseOutput:out input:in];
    };
    return backend;
}
- (uint64_t)outputHandleAddress { return _outputHandleAddress; }
- (uint64_t)inputHandleAddress { return _inputHandleAddress; }
@end
