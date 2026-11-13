#include <aaudio/AAudio.h>
#include <android/log.h>
#include <jni.h>
#include <sys/eventfd.h>
#include <sys/resource.h>
#include <poll.h>
#include <unistd.h>
#include <atomic>
#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cerrno>
#include <thread>
#include <system_error>
#include <time.h>
#include "oink_mobile_audio.h"

namespace {
JavaVM* vm = nullptr;
struct Audio {
    AAudioStream* stream = nullptr;
    OinkAudioInput* input = nullptr;
    OinkAudioOutput* output = nullptr;
    uint64_t generation;
    int sampleRate = 48000;
    int channels = 0;
    int event = -1;
    std::atomic<bool> stopping{false};
    std::atomic<int> error{AAUDIO_OK};
    std::atomic<uint64_t> callbacks{0}, framesSeen{0}, nonzeroInputCallbacks{0};
    std::atomic<int> maxBatch{0};
    // Capture level windows, aggregate only (no PCM is retained): the data
    // callback owns the running sums and publishes each finished 5 s window
    // to the worker, which logs it off the real-time thread.
    int levelFrames = 0;
    float levelPeak = 0.0f;
    double levelSquares = 0.0;
    std::atomic<float> windowPeak{0.0f}, windowMeanSquare{0.0f};
    std::atomic<uint64_t> levelWindows{0}, refusedFrames{0};
    uint64_t loggedWindows = 0;
    // Callback-thread clock anchor: the last AAudio timestamp pair (frame
    // position, CLOCK_MONOTONIC time of that frame).
    int64_t anchorPosition = 0;
    int64_t anchorNanos = 0;
    // Timestamp window (5 s of frames), aggregate counts only: callbacks
    // stamped from a fresh AAudio timestamp, extrapolated from the last
    // anchor, or left unknown, and the range of (first frame - timestamp
    // frame) offsets. The callback publishes, the worker logs.
    struct TimestampWindow {
        uint64_t exact = 0, extrapolated = 0, unknown = 0;
        int64_t offsetMin = INT64_MAX, offsetMax = INT64_MIN;
    };
    TimestampWindow stamps;
    int stampFrames = 0;
    std::atomic<uint64_t> windowExact{0}, windowExtrapolated{0}, windowUnknown{0};
    std::atomic<int64_t> windowOffsetMin{0}, windowOffsetMax{0};
    std::atomic<uint64_t> stampWindows{0};
    uint64_t loggedStampWindows = 0;
    std::thread worker;
    jobject listener = nullptr;
    jmethodID onError = nullptr;

    void signal() const {
        uint64_t one = 1;
        // Nonblocking eventfd coalesces wakeups. EAGAIN means a wake is already pending.
        ssize_t written;
        do { written = write(event, &one, sizeof(one)); } while (written < 0 && errno == EINTR);
    }
    // CLOCK_MONOTONIC time of the first frame of this callback, or 0 when
    // unknown (never callback arrival time).
    //
    // AAudioStream_getTimestamp returns a pair: frame `position` of the stream
    // was presented (output) or captured (input) at `nanos`. It is not the
    // time of this callback's first frame; HALs refresh it once per period,
    // so using `nanos` directly jittered by up to a burst and, for output,
    // stamped frames a whole buffer early. The first frame's time is
    // nanos + (first frame - position) / rate.
    //
    // First frame index: output uses getFramesWritten, which excludes the
    // frames this callback is about to write on both the MMAP and the legacy
    // path. Input uses getFramesRead: on the MMAP path it already includes
    // this callback's frames (read before the callback), on the legacy path
    // it may not. If it includes them the stamp is one callback late, which
    // only adds delay before the echo; AEC3's delay estimator absorbs that,
    // while a stamp that is too early would put the echo before its
    // reference, where it cannot be cancelled. So no subtraction.
    //
    // A failed query keeps the last anchor for up to one second of frames
    // (a valid clock line; drift over a second is ~0.1 ms) and counts it.
    uint64_t firstFrameNs(AAudioStream* stream) {
        const int64_t first = output ? AAudioStream_getFramesWritten(stream) : AAudioStream_getFramesRead(stream);
        if (first < 0) { stamps.unknown++; return 0; }
        int64_t position = 0;
        int64_t nanos = 0;
        if (AAudioStream_getTimestamp(stream, CLOCK_MONOTONIC, &position, &nanos) == AAUDIO_OK && nanos > 0) {
            anchorPosition = position;
            anchorNanos = nanos;
            stamps.exact++;
        } else if (anchorNanos > 0 && first - anchorPosition <= sampleRate && anchorPosition - first <= sampleRate) {
            stamps.extrapolated++;
        } else {
            stamps.unknown++;
            return 0;
        }
        const int64_t offset = first - anchorPosition;
        stamps.offsetMin = std::min(stamps.offsetMin, offset);
        stamps.offsetMax = std::max(stamps.offsetMax, offset);
        const int64_t ns = anchorNanos + offset * 1'000'000'000LL / sampleRate;
        return ns > 0 ? static_cast<uint64_t>(ns) : 0;
    }
    void countStampFrames(int32_t frames) {
        stampFrames += frames;
        if (stampFrames < sampleRate * 5) return;
        windowExact.store(stamps.exact, std::memory_order_relaxed);
        windowExtrapolated.store(stamps.extrapolated, std::memory_order_relaxed);
        windowUnknown.store(stamps.unknown, std::memory_order_relaxed);
        windowOffsetMin.store(stamps.offsetMin == INT64_MAX ? 0 : stamps.offsetMin, std::memory_order_relaxed);
        windowOffsetMax.store(stamps.offsetMax == INT64_MIN ? 0 : stamps.offsetMax, std::memory_order_relaxed);
        stampWindows.fetch_add(1, std::memory_order_release);
        stamps = TimestampWindow{};
        stampFrames = 0;
        // Output streams have no other periodic worker wake.
        if (output) signal();
    }
    static aaudio_data_callback_result_t data(AAudioStream* stream, void* opaque, void* buffer, int32_t frames) {
        auto* self = static_cast<Audio*>(opaque);
        auto* samples = static_cast<float*>(buffer);
        if (self->stopping.load(std::memory_order_acquire)) {
            if (self->output) std::fill_n(samples, frames * self->channels, 0.0f);
            return AAUDIO_CALLBACK_RESULT_STOP;
        }
        self->callbacks.fetch_add(1, std::memory_order_relaxed);
        self->framesSeen.fetch_add(frames, std::memory_order_relaxed);
        // Aggregate signal presence and level only; retain no PCM or speech-derived data in diagnostics.
        if (self->input) self->measure(samples, frames);
        // A direction has one serialized callback writer.
        self->maxBatch.store(std::max(self->maxBatch.load(std::memory_order_relaxed), frames), std::memory_order_relaxed);
        // AAudio may choose a larger callback. Keep each ABI call within its 4096-frame bound.
        const uint64_t host = self->firstFrameNs(stream);
        self->countStampFrames(frames);
        for (int offset = 0; offset < frames; offset += 4096) {
            auto count = std::min(4096, frames - offset);
            auto* part = samples + offset * self->channels;
            const uint64_t partHost = host == 0 ? 0 : host +
                static_cast<uint64_t>(offset) * 1'000'000'000ULL / static_cast<uint64_t>(self->sampleRate);
            if (self->output) oink_audio_output_pull_at(self->output, part, count, partHost);
            if (self->input) {
                const uint32_t queued = oink_audio_input_push(self->input, part, count, partHost);
                if (queued > 0) self->signal();
                // Frames the bounded handoff refused because the worker fell behind
                // (the core marks the gap as a discontinuity).
                if (queued < static_cast<uint32_t>(count))
                    self->refusedFrames.fetch_add(static_cast<uint32_t>(count) - queued, std::memory_order_relaxed);
            }
        }
        return AAUDIO_CALLBACK_RESULT_CONTINUE;
    }
    void measure(const float* samples, int32_t frames) {
        bool nonzero = false;
        for (int32_t i = 0; i < frames * channels; ++i) {
            const float sample = samples[i];
            nonzero |= sample != 0.0f;
            levelPeak = std::max(levelPeak, std::fabs(sample));
            levelSquares += static_cast<double>(sample) * sample;
        }
        if (nonzero) nonzeroInputCallbacks.fetch_add(1, std::memory_order_relaxed);
        levelFrames += frames;
        if (levelFrames >= sampleRate * 5) {
            windowPeak.store(levelPeak, std::memory_order_relaxed);
            windowMeanSquare.store(static_cast<float>(levelSquares / (static_cast<double>(levelFrames) * channels)),
                std::memory_order_relaxed);
            levelWindows.fetch_add(1, std::memory_order_release);
            levelFrames = 0;
            levelPeak = 0.0f;
            levelSquares = 0.0;
        }
    }
    // Worker only. One line per finished window: the capture level as the
    // platform delivers it, before any Oink gain, AEC or codec.
    void logLevels() {
        const uint64_t windows = levelWindows.load(std::memory_order_acquire);
        if (windows == loggedWindows) return;
        loggedWindows = windows;
        __android_log_print(ANDROID_LOG_INFO, "OinkAudio",
            "input level window=%llu seconds=5 peak_dbfs=%.1f rms_dbfs=%.1f refused_frames=%llu generation=%llu",
            static_cast<unsigned long long>(windows), 20.0 * std::log10(windowPeak.load(std::memory_order_relaxed)),
            10.0 * std::log10(windowMeanSquare.load(std::memory_order_relaxed)),
            static_cast<unsigned long long>(refusedFrames.load(std::memory_order_relaxed)),
            static_cast<unsigned long long>(generation));
    }
    // Worker only. One line per finished 5 s window and direction: where the
    // callback timestamps came from (AEC alignment needs them).
    void logTimestamps() {
        const uint64_t windows = stampWindows.load(std::memory_order_acquire);
        if (windows == loggedStampWindows) return;
        loggedStampWindows = windows;
        __android_log_print(ANDROID_LOG_INFO, "OinkAudio",
            "timestamps input=%d window=%llu seconds=5 exact=%llu extrapolated=%llu unknown=%llu "
            "offset_min_frames=%lld offset_max_frames=%lld generation=%llu",
            input != nullptr, static_cast<unsigned long long>(windows),
            static_cast<unsigned long long>(windowExact.load(std::memory_order_relaxed)),
            static_cast<unsigned long long>(windowExtrapolated.load(std::memory_order_relaxed)),
            static_cast<unsigned long long>(windowUnknown.load(std::memory_order_relaxed)),
            static_cast<long long>(windowOffsetMin.load(std::memory_order_relaxed)),
            static_cast<long long>(windowOffsetMax.load(std::memory_order_relaxed)),
            static_cast<unsigned long long>(generation));
    }
    static void failed(AAudioStream*, void* opaque, aaudio_result_t error) {
        auto* self = static_cast<Audio*>(opaque);
        self->error.store(error, std::memory_order_release);
        self->signal();
    }
    void run() {
        if (input && setpriority(PRIO_PROCESS, 0, -16) != 0)
            __android_log_print(ANDROID_LOG_WARN, "OinkAudio", "capture worker priority unavailable errno=%d", errno);
        JNIEnv* env = nullptr;
        vm->AttachCurrentThread(&env, nullptr);
        pollfd fd{event, POLLIN, 0};
        while (!stopping.load(std::memory_order_acquire)) {
            if (poll(&fd, 1, -1) < 0) { if (errno == EINTR) continue; break; }
            uint64_t count;
            auto ignored = read(event, &count, sizeof(count));
            (void)ignored;
            if (stopping.load(std::memory_order_acquire)) break;
            int result = error.exchange(AAUDIO_OK);
            if (result != AAUDIO_OK && env) {
                // Ordinary worker only. The Kotlin callback posts onto the owner control queue.
                env->CallVoidMethod(listener, onError, static_cast<jlong>(generation), result);
                if (env->ExceptionCheck()) env->ExceptionClear();
            }
            if (input) {
                oink_audio_input_process(input);
                logLevels();
            }
            logTimestamps();
        }
        if (env) vm->DetachCurrentThread();
    }
    void close(JNIEnv* env) {
        // Read before revoke: a revoked handoff refuses the last callbacks' frames too.
        const uint64_t refused = refusedFrames.load(std::memory_order_relaxed);
        if (input) oink_audio_input_revoke(input);
        if (output) oink_audio_output_revoke(output);
        stopping.store(true, std::memory_order_release);
        // Only the control thread uses stream lifecycle APIs. close waits for native callbacks.
        if (stream) {
            __android_log_print(ANDROID_LOG_INFO, "OinkAudio", "stop input=%d xruns=%d device=%d buffer=%d",
                input != nullptr, AAudioStream_getXRunCount(stream), AAudioStream_getDeviceId(stream),
                AAudioStream_getBufferSizeInFrames(stream));
            AAudioStream_requestStop(stream);
            AAudioStream_close(stream);
            stream = nullptr;
        }
        if (worker.joinable()) { signal(); worker.join(); }
        __android_log_print(ANDROID_LOG_INFO, "OinkAudio", "closed input=%d callbacks=%llu frames=%llu nonzero_input_callbacks=%llu refused_frames=%llu max_batch=%d generation=%llu",
            input != nullptr, static_cast<unsigned long long>(callbacks.load()),
            static_cast<unsigned long long>(framesSeen.load()), static_cast<unsigned long long>(nonzeroInputCallbacks.load()),
            static_cast<unsigned long long>(refused), maxBatch.load(), static_cast<unsigned long long>(generation));
        if (input) oink_audio_input_destroy(input);
        if (output) oink_audio_output_destroy(output);
        if (event >= 0) ::close(event);
        if (listener) env->DeleteGlobalRef(listener);
    }
};
Audio* audio(jlong handle) { return reinterpret_cast<Audio*>(handle); }
}
extern "C" JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM* value, void*) { vm = value; return JNI_VERSION_1_6; }
extern "C" JNIEXPORT jlong JNICALL
Java_com_hachiwii_oink_audio_NativeAudio_open(JNIEnv* env, jobject listener, jboolean input, jlong generation, jboolean exclusive) {
    auto* self = new Audio();
    self->generation = static_cast<uint64_t>(generation);
    self->event = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    self->listener = env->NewGlobalRef(listener);
    self->onError = env->GetMethodID(env->GetObjectClass(listener), "onNativeError", "(JI)V");
    AAudioStreamBuilder* builder = nullptr;
    aaudio_result_t result = AAudio_createStreamBuilder(&builder);
    if (result == AAUDIO_OK && self->event >= 0) {
        AAudioStreamBuilder_setDirection(builder, input ? AAUDIO_DIRECTION_INPUT : AAUDIO_DIRECTION_OUTPUT);
        AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_FLOAT);
        AAudioStreamBuilder_setSampleRate(builder, 48000);
        AAudioStreamBuilder_setChannelCount(builder, input ? 1 : 2);
        AAudioStreamBuilder_setPerformanceMode(builder, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
        AAudioStreamBuilder_setUsage(builder, AAUDIO_USAGE_MEDIA);
        // GENERIC is Android's ordinary recording path: the device's normal
        // microphone gain, no communication-mode voice processing. That is
        // what the PCs (default role, no voice processing) and iOS
        // (playAndRecord, mode default) capture, and Case A AEC runs on it there.
        // Not UNPROCESSED: Android specifies it with no AGC at a sensitivity of
        // -36 dBFS for a 94 dB SPL tone (CDD 5.11), so speech arrives near
        // -60 dBFS (the NDK warns "the recorded volume may be very low"), and
        // on the Phone it has no FAST capture profile, so AAudio fell back
        // to 20 ms bursts (performance=10 burst=960). Not VOICE_COMMUNICATION:
        // it adds the platform's AEC/NS/AGC, which Oink does not stack on its
        // own Case A. Not VOICE_RECOGNITION: also AGC-free, specified at
        // -22 dBFS for 90 dB SPL, still well under a normal recording level.
        AAudioStreamBuilder_setInputPreset(builder, AAUDIO_INPUT_PRESET_GENERIC);
        // A client session ID lets AudioRecordingCallback identify system-silenced capture.
        // Android documents a possible latency cost, so actual performance/burst are logged.
        AAudioStreamBuilder_setSessionId(builder, AAUDIO_SESSION_ID_ALLOCATE);
        AAudioStreamBuilder_setDataCallback(builder, Audio::data, self);
        AAudioStreamBuilder_setErrorCallback(builder, Audio::failed, self);
        AAudioStreamBuilder_setFramesPerDataCallback(builder, 240);
        AAudioStreamBuilder_setSharingMode(builder, exclusive ? AAUDIO_SHARING_MODE_EXCLUSIVE : AAUDIO_SHARING_MODE_SHARED);
        result = AAudioStreamBuilder_openStream(builder, &self->stream);
        if (result != AAUDIO_OK) {
            AAudioStreamBuilder_setSharingMode(builder, AAUDIO_SHARING_MODE_SHARED);
            result = AAudioStreamBuilder_openStream(builder, &self->stream);
        }
    }
    if (builder) AAudioStreamBuilder_delete(builder);
    if (result == AAUDIO_OK && self->stream) {
        if (!input) AAudioStream_setBufferSizeInFrames(self->stream, 2 * AAudioStream_getFramesPerBurst(self->stream));
        int rate = AAudioStream_getSampleRate(self->stream);
        self->sampleRate = rate > 0 ? rate : 48000;
        self->channels = AAudioStream_getChannelCount(self->stream);
        if (AAudioStream_getFormat(self->stream) == AAUDIO_FORMAT_PCM_FLOAT) {
            if (input) self->input = oink_audio_input_create(rate, self->channels, generation);
            else self->output = oink_audio_output_create(rate, self->channels, generation);
        }
        __android_log_print(ANDROID_LOG_INFO, "OinkAudio", "open input=%d rate=%d channels=%d preset=%d sharing=%d performance=%d burst=%d callback=%d buffer=%d session=%d generation=%lld",
            input, rate, self->channels, input ? AAudioStream_getInputPreset(self->stream) : 0,
            AAudioStream_getSharingMode(self->stream),
            AAudioStream_getPerformanceMode(self->stream), AAudioStream_getFramesPerBurst(self->stream),
            AAudioStream_getFramesPerDataCallback(self->stream), AAudioStream_getBufferSizeInFrames(self->stream),
            AAudioStream_getSessionId(self->stream), static_cast<long long>(generation));
    }
    if (!self->input && !self->output) {
        __android_log_print(ANDROID_LOG_ERROR, "OinkAudio", "open failed: %d", result);
        self->close(env); delete self; return 0;
    }
    try {
        self->worker = std::thread([self] { self->run(); });
    } catch (const std::system_error& error) {
        __android_log_print(ANDROID_LOG_ERROR, "OinkAudio", "worker start failed: %s", error.what());
        self->close(env);
        delete self;
        return 0;
    }
    return reinterpret_cast<jlong>(self);
}
extern "C" JNIEXPORT jlong JNICALL
Java_com_hachiwii_oink_audio_NativeAudio_address(JNIEnv*, jobject, jlong handle) {
    auto* self = audio(handle);
    return self->input ? reinterpret_cast<jlong>(self->input) : reinterpret_cast<jlong>(self->output);
}
extern "C" JNIEXPORT jint JNICALL
Java_com_hachiwii_oink_audio_NativeAudio_start(JNIEnv*, jobject, jlong handle) {
    auto* stream = audio(handle)->stream;
    auto result = AAudioStream_requestStart(stream);
    if (result != AAUDIO_OK) return result;
    auto state = AAudioStream_getState(stream);
    while (state == AAUDIO_STREAM_STATE_STARTING || state == AAUDIO_STREAM_STATE_OPEN) {
        // AAudio lifecycle operations are asynchronous. Never publish Ready from requestStart alone.
        result = AAudioStream_waitForStateChange(stream, state, &state, 1000000000);
        if (result != AAUDIO_OK) return result;
    }
    return state == AAUDIO_STREAM_STATE_STARTED ? AAUDIO_OK : AAUDIO_ERROR_INVALID_STATE;
}
extern "C" JNIEXPORT jint JNICALL
Java_com_hachiwii_oink_audio_NativeAudio_sessionId(JNIEnv*, jobject, jlong handle) {
    return AAudioStream_getSessionId(audio(handle)->stream);
}
extern "C" JNIEXPORT jint JNICALL
Java_com_hachiwii_oink_audio_NativeAudio_deviceId(JNIEnv*, jobject, jlong handle) {
    return AAudioStream_getDeviceId(audio(handle)->stream);
}
extern "C" JNIEXPORT void JNICALL
Java_com_hachiwii_oink_audio_NativeAudio_close(JNIEnv* env, jobject, jlong handle) {
    auto* self = audio(handle); self->close(env); delete self;
}
