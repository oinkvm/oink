// Synthetic bridge unit tests: never creates a device, tap, or IOProc.
#include "../native/macos.m"
#include <assert.h>
static OinkCapture *make_capture(void) {
    OinkCapture *capture = calloc(1, sizeof(*capture));
    assert(capture);
    capture->sample_rate = 48000;
    capture->low_latency = true;
    capture->info.buffer_frames = 128;
    mach_timebase_info(&capture->timebase);
    return capture;
}
static void audio_is_independent_of_diagnostic_drain(void) {
    OinkCapture *drained = make_capture(), *full = make_capture();
    float input[256], a[512], b[512];
    for (size_t i = 0; i < 256; i++) input[i] = (float)i / 256;
    AudioBufferList buffers = {.mNumberBuffers = 1, .mBuffers = {{2, sizeof(input), input}}};
    AudioTimeStamp stamp = {.mHostTime = mach_absolute_time(), .mFlags = kAudioTimeStampHostTimeValid};
    OinkCaptureEvent events[EVENT_CAPACITY];
    uint64_t lost, trimmed, audio_dropped;
    for (size_t i = 0; i < 3000; i++) {
        capture_io(0, NULL, &buffers, &stamp, NULL, NULL, drained);
        capture_io(0, NULL, &buffers, &stamp, NULL, NULL, full);
        assert(oink_audio_capture_events(drained, events, EVENT_CAPACITY, &lost, &trimmed, &audio_dropped) == 1);
        assert(lost == 0 && events[0].frames == 128 && events[0].sequence == i * 128);
        if (i % 3 == 2) {
            uint64_t ah, ac, as, bh, bc, bs;
            size_t an = oink_audio_capture_read(drained, a, 256, &ah, &ac, &as);
            size_t bn = oink_audio_capture_read(full, b, 256, &bh, &bc, &bs);
            assert(an == bn && ah == bh && as == bs && ac != 0 && bc != 0);
            assert(memcmp(a, b, an * 2 * sizeof(float)) == 0);
        }
    }
    assert(oink_audio_capture_events(full, events, EVENT_CAPACITY, &lost, &trimmed, &audio_dropped) == EVENT_CAPACITY);
    assert(lost == 3000 - EVENT_CAPACITY && trimmed == drained->trimmed_frames && trimmed > 0);
    for (size_t i = 0; i < EVENT_CAPACITY; i++) assert(events[i].sequence == i * 128);
    capture_io(0, NULL, NULL, NULL, NULL, NULL, drained);
    assert(oink_audio_capture_events(drained, events, 1, &lost, &trimmed, &audio_dropped) == 1);
    assert(events[0].frames == 0 && events[0].host_ns == 0);
    free(drained); free(full);
}
static _Atomic bool done;
static void *produce(void *raw) {
    OinkCapture *capture = raw;
    for (uint64_t i = 1; i <= 100000; i++) capture_event(capture, i * 1000, i * 1000 - 1, i, 128, 0, 0);
    atomic_store_explicit(&done, true, memory_order_release);
    return NULL;
}
static void concurrent_ring(void) {
    OinkCapture *capture = make_capture();
    pthread_t thread;
    assert(pthread_create(&thread, NULL, produce, capture) == 0);
    uint64_t seen = 0, previous = 0, lost = 0, trimmed = 0, audio_dropped = 0;
    for (;;) {
        bool finished = atomic_load_explicit(&done, memory_order_acquire);
        OinkCaptureEvent events[17];
        size_t count = oink_audio_capture_events(capture, events, 17, &lost, &trimmed, &audio_dropped);
        for (size_t i = 0; i < count; i++) {
            assert(events[i].sequence > previous);
            assert(events[i].callback_ns == events[i].sequence * 1000);
            assert(events[i].host_ns + 1 == events[i].callback_ns && events[i].frames == 128);
            previous = events[i].sequence;
        }
        seen += count;
        if (finished && count == 0) break;
    }
    pthread_join(thread, NULL);
    assert(seen + lost == 100000);
    free(capture);
}
static void silence_keeps_the_first_nonzero_buffer(void) {
    OinkCapture *capture = make_capture();
    assert(semaphore_create(mach_task_self(), &capture->wake, SYNC_POLICY_FIFO, 0) == KERN_SUCCESS);
    float input[256] = {0}, output[256];
    AudioBufferList buffers = {.mNumberBuffers = 1, .mBuffers = {{2, sizeof(input), input}}};
    uint64_t host, callback, sequence;
    for (unsigned i = 0; i < 2000; ++i) {
        capture_io(0, NULL, &buffers, NULL, NULL, NULL, capture);
        if (atomic_load(&capture->wake_pending)) oink_audio_capture_wait(capture);
        oink_audio_capture_read(capture, output, 128, &host, &callback, &sequence);
    }
    capture_io(0, NULL, &buffers, NULL, NULL, NULL, capture);
    assert(!atomic_load(&capture->wake_pending));
    assert(oink_audio_capture_read(capture, output, 128, &host, &callback, &sequence) == 0);
    // Even a subnormal in the last channel must wake and keep the entire onset.
    input[255] = 0x1p-140f;
    capture_io(0, NULL, &buffers, NULL, NULL, NULL, capture);
    assert(atomic_load(&capture->wake_pending));
    oink_audio_capture_wait(capture);
    assert(oink_audio_capture_read(capture, output, 128, &host, &callback, &sequence) == 128);
    assert(sequence == 2001 * 128 && memcmp(input, output, sizeof(input)) == 0);
    input[255] = 0;
    capture_io(0, NULL, &buffers, NULL, NULL, NULL, capture);
    assert(atomic_load(&capture->wake_pending)); // preserve the tail of a live burst
    capture_free(capture);
}
int main(void) {
    audio_is_independent_of_diagnostic_drain();
    concurrent_ring();
    silence_keeps_the_first_nonzero_buffer();
    return 0;
}
