# Oink local AEC3

Pinned AEC3-only WebRTC audio-processing subset with a fixed-buffer C ABI.
The `tonarino/webrtc-audio-processing` 2.1.0 wrapper was evaluated: its processing
entry point allocates output vectors, and its bundled build requires Meson/Ninja.
This subset builds with the repository's existing `cc` toolchain on Apple Silicon
and Windows MSVC without installing additional tools. No NS or AGC is linked.

`vendor-manifest.json` pins every source/header/notice and both upstream versions.
`sources.txt`, `avx2-sources.txt`, and `build.rs` define the actual native closure.
The only source patch retains comfort-noise initialization storage until ordinary
thread destruction; otherwise WebRTC frees it during a processing block. Private
namespaces and QMF/fatal symbols avoid collisions. Stereo is fixed to preserve
anti-phase reference channels and avoid dynamic channel/filter allocations.
The refined/coarse filters are 256/128 ms, with zero initial bulk delay; residual
echo suppression and the required 48 kHz split filterbank remain enabled.

## Local pipeline

Only the actual shared-microphone sender that also owns the physical forwarded
speaker exit, with its local switch enabled and live capture demand, runs AEC.
Virtual microphone playback never supplies a reference. All receivers use the
existing single encoder; there is no protocol change or PCM diagnostic recording.

HAL copies final float output and inputDataTime host/sample positions. WASAPI
copies final clipped/quantized values only after successful ReleaseBuffer; capture
uses native GetBuffer device/QPC positions and flags. Render IAudioClock pairs
estimate rate; padding supplies a conservative presentation estimate, not a DAC
measurement. Unknown timestamps, discontinuities and unstable clocks bypass AEC.

Native callbacks publish fixed SPSC packets (capture <=20 ms, render <=40 ms).
The device workgroup/MMCSS executor resamples mono capture to 48 kHz; only the
stereo reference receives drift ASRC. AEC consumes 10 ms blocks, the existing
encoder still emits its negotiated packet duration. Ordinary owners create and
retire engines via atomic handoff. No processing-path heap operations, mutexes,
logging, device queries or blocking queue writes. The scheduling wait is outside
the work unit. A 20 ms capture-age limit or 2 ms AEC work overrun bypasses raw audio
and requests a fresh epoch. State changes use a 10 ms mix ramp; missing reference
uses a boundary splice without replaying old speech or fabricating reference PCM.

The measured near-only peak delay of this engine is 430 samples at 48 kHz
(8.96 ms), plus up to 10 ms block accumulation and native SRC lookahead (32 native
frames). Actual device/transport latency and scheduling require live measurement.
Raw bypass is delay-matched while eligible; switching Off removes that delay.

Diagnostics are fixed-size once-per-second numeric summaries, split into two
rows within the existing 24-field limit, with up to 16 state transitions. `State`, `Reason`, timestamp quality, validity, clock drift, alignment,
qualified engine ERLE, CPU, work histogram, resets and bypass are recorded. ERLE
is withheld for silence, invalid reference, clipping and insufficient aligned
linear reduction; it remains an estimate, not a double-talk ground truth measure.
The UI contains only the default-on, locally persisted switch.

## Reproduction and checks

- `python3 crates/oink-audio/native/aec3/verify-vendor.py` checks the entire closure.
  Optional `--webrtc <pinned-checkout> --abseil <20240722.0-extraction>` compares
  every file to upstream plus `realtime-storage.patch`.
- `cargo test -p oink-aec3 -p oink-audio` runs deterministic synthetic quality,
  clock drift, lifecycle, native boundary mocks and allocation audits without
  opening devices. Use the repository's Mac cargo lane / a Windows test machine 8-job cap.
- `cargo run --release -p oink-aec3 --example offline` prints numerical/CPU results.
  `--rir <mono-48k-float32.wav>` adds a recorded room-response convolution.
- Explicit fixture tests: set `OINK_AEC_RIR` and run the ignored
  `recorded_room_response` and `recorded_room_end_to_end_activation_and_near_end_preservation`.
  Numerical adapter fixtures use a virtual work budget so host test-runner
  preemption does not reset filters; a separate injected-exhaustion test checks
  product overload behavior. CPU figures must come from actual timed runs.
- The fixture used in evaluation is the REW Position 1 measurement from
  `Cieslar-Simon/STI` commit `1b4a0c33740c52d62b81aad7b667b1f85c522fec`,
  `control_measurements/Position1/IR_REW_position_1.wav`, GPL-3.0. SHA-256:
  `cb91f8747eefe38c5f6e3e79a0fd40ec9e899fa5d4ce01bbee736398dba73288`.
  It is external evidence, not bundled. The harness removes measurement pre-delay
  before peak-minus-96 samples, retains the full tail and normalizes L2 gain.
- Native allocator/lock interposers and live acoustic probe/plan are retained in
  evidence `oink-aec-impl/`. macOS native+Rust audit uses
  `OINK_AEC_NATIVE_AUDIT=1`, `DYLD_INSERT_LIBRARIES=<audit dylib>`, one exact test
  thread. Windows uses own-process allocator/lock IAT instrumentation only.
- `node build-support/generate-licenses.mjs` updates attribution; `--check` verifies
  packaging outputs and the pinned source manifest. See LICENSE/PATENTS/NOTICE.

Live acoustic acceptance, device restart/sleep behavior, OS enhancement interaction,
STOI and listening checks are pending explicit authorization. Offline RIR results
are not evidence of microphones/speakers on the user's machines.
