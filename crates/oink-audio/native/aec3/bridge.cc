#include <algorithm>
#include <cmath>
#include "modules/audio_processing/aec3/echo_canceller3.h"

namespace {
webrtc::EchoCanceller3Config config() {
  auto c = webrtc::EchoCanceller3::CreateDefaultMultichannelConfig();
  // Fixed stereo reference: never downmix anti-phase channels, and never
  // allocate a new filter in ProcessCapture when content switches mono/stereo.
  c.multi_channel.detect_stereo_content = false;
  // AEC3's own echo-path delay estimator (render delay controller with its
  // matched filters) runs as in upstream WebRTC: no external delay, upstream
  // headroom, filter count and selection thresholds. It finds whatever delay
  // remains after Oink's timestamp alignment (output/input latency, acoustic
  // path); `metrics().delay_ms` reports it.
  c.delay.use_external_delay_estimator = false;
  // Timestamp-aligned local render may have zero acoustic bulk delay. Starting
  // five blocks behind prevents acquisition of that path in the pinned engine.
  // Only the seed lag is zero; the estimator moves it from there.
  c.delay.default_delay = 0;
  // Suppressor, echo-path strength and comfort noise stay at AEC3's defaults:
  // before the linear filter has converged the suppressor assumes a strong
  // echo path wherever the reference is active, which is what breaks an
  // acoustic loop early. It needs the delay controller's estimate for that:
  // until the controller locks (about the first 100 ms of render activity)
  // the echo estimate uses the unconverged filter's delay and can miss the
  // echo (see the closed-loop test in tests/offline.rs).
  c.filter.export_linear_aec_output = true;
  // 256 ms of modeled decay for a physical room. Upstream's 52 ms refined
  // filter leaves most of a measured reverberant tail to the nonlinear stage.
  // Fix both initial/final sizes so adaptation never changes storage lifetime.
  c.filter.refined.length_blocks = 64;
  c.filter.refined_initial.length_blocks = 64;
  c.filter.coarse.length_blocks = 32;
  c.filter.coarse_initial.length_blocks = 32;
  return c;
}
struct Engine {
  float delayed_capture_low[64] = {};
  unsigned capture_low_index = 0;
  double capture_low_power = 0, linear_power = 0;
  webrtc::AudioBuffer render{48000, 2, 48000, 2, 48000, 2};
  webrtc::AudioBuffer capture{48000, 1, 48000, 1, 48000, 1};
  webrtc::AudioBuffer linear{16000, 1, 16000, 1, 16000, 1};
  webrtc::EchoCanceller3 aec{config(), config(), 48000, 2, 1};
};
}
extern "C" {
void* oink_aec3_create() { try { return new Engine; } catch (...) { return nullptr; } }
void oink_aec3_destroy(void* p) { delete static_cast<Engine*>(p); }
void oink_aec3_process(void* p, const float* left, const float* right,
                       const float* mic, float* output, float* linear,
                       int level_change) {
  auto& e = *static_cast<Engine*>(p);
  for (int i=0; i<480; ++i) {
    e.render.channels()[0][i] = left[i]*32768.f;
    e.render.channels()[1][i] = right[i]*32768.f;
    e.capture.channels()[0][i] = mic[i]*32768.f;
  }
  e.render.SplitIntoFrequencyBands();
  e.aec.AnalyzeRender(&e.render);
  e.aec.AnalyzeCapture(&e.capture);
  e.capture.SplitIntoFrequencyBands();
  e.capture_low_power = 0;
  for (int i=0; i<160; ++i) {
    // The linear output BlockFramer adds exactly 64 low-band samples. Compare
    // powers at the same sampling rate/time, never 16 kHz output with 48 kHz PCM.
    const float sample=e.delayed_capture_low[e.capture_low_index];
    e.capture_low_power += double(sample)*sample;
    e.delayed_capture_low[e.capture_low_index]=e.capture.split_bands(0)[0][i]/32768.f;
    e.capture_low_index=(e.capture_low_index+1)%64;
  }
  e.aec.ProcessCapture(&e.capture, &e.linear, level_change != 0);
  e.capture.MergeFrequencyBands();
  for (int i=0; i<480; ++i) output[i]=e.capture.channels()[0][i]/32768.f;
  e.linear_power = 0;
  for (int i=0; i<160; ++i) {
    linear[i]=e.linear.channels()[0][i]/32768.f;
    e.linear_power += double(linear[i])*linear[i];
  }
}
void oink_aec3_linear_powers(void* p,double* capture,double* linear) {
  auto& e=*static_cast<Engine*>(p);*capture=e.capture_low_power;*linear=e.linear_power;
}
void oink_aec3_metrics(void* p, double* erl, double* erle, int* delay) {
  auto metrics = static_cast<Engine*>(p)->aec.GetMetrics();
  *erl=metrics.echo_return_loss;
  *erle=metrics.echo_return_loss_enhancement;
  *delay=metrics.delay_ms;
}
}
