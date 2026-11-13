#ifndef OINK_MOBILE_AUDIO_H
#define OINK_MOBILE_AUDIO_H
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif
/* Control thread owns handles. Each direction has ONE serialized callback.
 * Float PCM is interleaved, 1/2 channels, 8000..192000 Hz, <=4096 frames.
 * generation is supplied by MobileMediaSession. host_ns is hardware first-frame
 * monotonic nanoseconds, 0 means unknown (never substitute dequeue time).
 * Stop native I/O and join all callbacks/worker before destroy. Never destroy
 * on an audio callback. Recreate on route/generation change.
 * pull/push do not allocate/free, log, enter foreign runtimes or take locks.
 */
typedef struct OinkAudioOutput OinkAudioOutput;
typedef struct OinkAudioInput OinkAudioInput;
typedef void (*OinkAudioRenderCallback)(void *context, float *interleaved,
    uint32_t frames, uint64_t host_ns);
typedef void (*OinkAudioCaptureCallback)(void *context, const float *interleaved,
    uint32_t frames, uint64_t host_ns);
/* Core prepares each negotiated source before publication to render. */
OinkAudioOutput *oink_audio_output_create(uint32_t rate, uint16_t channels, uint64_t generation);
OinkAudioInput *oink_audio_input_create(uint32_t rate, uint16_t channels, uint64_t generation);
/* Control-thread callback ownership. The callbacks are invoked only after the
 * native owner has entered the Rust handle; they must not retain the buffers. */
void oink_audio_output_set_renderer(OinkAudioOutput *, void *owner, OinkAudioRenderCallback);
void oink_audio_input_set_capture(OinkAudioInput *, OinkAudioCaptureCallback, void *context);
/* Device-local Case A policy. AEC is enabled by default and never enters v2 wire. */
void oink_audio_input_set_aec(OinkAudioInput *, uint8_t enabled, uint8_t local_mic, uint8_t local_exit);
/* RT callbacks. pull initializes the entire buffer, including on stale epoch. */
uint8_t oink_audio_output_pull(OinkAudioOutput *, float *, uint32_t frames);
/* Timestamped variant used by Android AAudio and iOS AVAudioEngine. The
 * compatibility wrapper above passes host_ns=0 for older adapters. */
uint8_t oink_audio_output_pull_at(OinkAudioOutput *, float *, uint32_t frames, uint64_t host_ns);
uint32_t oink_audio_input_push(OinkAudioInput *, const float *, uint32_t frames, uint64_t host_ns);
/* Native worker: signal when push returns >0; do not call from input callback.
 * No periodic timer. Worker processes bounded backlog, drops samples >20ms old.
 * Core transport owns demand; native hardware remains running without demand. */
void oink_audio_input_process(OinkAudioInput *);
/* Revoke immediately, then stop native I/O, join callbacks, destroy, ack stop. */
void oink_audio_output_revoke(OinkAudioOutput *);
void oink_audio_input_revoke(OinkAudioInput *);
void oink_audio_output_destroy(OinkAudioOutput *);
void oink_audio_input_destroy(OinkAudioInput *);
/* Output capture (AudioPolicy / ReplayKit), resource bit 4 (Forward).
 * Serialized non-RT worker; format fixed per handle, interleaved 1/2 channels,
 * 8000..192000 Hz, <=4096 frames. host_ns is first-frame CLOCK_MONOTONIC /
 * host time, 0 unknown. Mark dropped blocks/timestamp resets discontinuous.
 * claim_start -> create -> attach_audio -> capture starts -> acknowledge_started.
 * On stop: revoke, stop/join capture workers, destroy, acknowledge_stopped.
 * A stale generation cannot restart the core. Return is accepted input frames.
 * Capture busy: report_failure(Forward,generation,Unavailable); helper absent:
 * SystemBlocked; pipe EOF: Interrupted. Helper never owns a network session.
 */
typedef struct OinkAudioForward OinkAudioForward;
OinkAudioForward *oink_audio_forward_create(uint32_t rate, uint16_t channels, uint64_t generation);
uint32_t oink_audio_forward_push(OinkAudioForward *, const float *, uint32_t frames, uint64_t host_ns, uint8_t discontinuity);
uint32_t oink_audio_forward_push_i16(OinkAudioForward *, const int16_t *, uint32_t frames, uint64_t host_ns, uint8_t discontinuity);
void oink_audio_forward_revoke(OinkAudioForward *);
void oink_audio_forward_destroy(OinkAudioForward *);
#ifdef __cplusplus
}
#endif
#endif
