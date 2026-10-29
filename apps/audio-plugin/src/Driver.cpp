// SPDX-License-Identifier: MIT
#include "Loopback.hpp"
#include <aspl/Driver.hpp>
#include <mach/mach_time.h>
#include <cmath>

namespace {
constexpr UInt32 Rate = 48000;
constexpr UInt32 Period = 256;
struct State {
    Loopback audio;
    std::atomic<bool> writing{false};
    const uint64_t epoch = mach_absolute_time();
    mach_timebase_info_data_t timebase{};
    State() { mach_timebase_info(&timebase); }
};
class Handler final : public aspl::IORequestHandler, public aspl::ControlRequestHandler {
public:
    Handler(std::shared_ptr<State> state, bool input) : state_(state), input_(input) {}
    OSStatus OnStartIO() override {
        if (!input_) { state_->audio.clear(); state_->writing.store(true); }
        return noErr;
    }
    void OnStopIO() override {
        if (!input_) { state_->writing.store(false); state_->audio.clear(); }
    }
    void OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>&,
        Float64, Float64 time, const void* bytes, UInt32 count) override {
        if (input_ || time < 0 || !std::isfinite(time)) return;
        const auto* samples = static_cast<const float*>(bytes);
        for (UInt32 i = 0; i < count / sizeof(float); ++i)
            state_->audio.write(static_cast<uint64_t>(time) + i, samples[i]);
    }
    void OnReadClientInput(const std::shared_ptr<aspl::Client>&,
        const std::shared_ptr<aspl::Stream>&, Float64, Float64 time,
        void* bytes, UInt32 count) override {
        std::memset(bytes, 0, count);
        if (!input_ || !state_->writing.load() || time < Loopback::Delay || !std::isfinite(time)) return;
        auto* samples = static_cast<float*>(bytes);
        for (UInt32 i = 0; i < count / sizeof(float); ++i)
            samples[i] = state_->audio.read(static_cast<uint64_t>(time) + i - Loopback::Delay);
    }
private:
    std::shared_ptr<State> state_;
    bool input_;
};
class Device final : public aspl::Device {
public:
    Device(std::shared_ptr<aspl::Context> context, const aspl::DeviceParameters& params,
           std::shared_ptr<State> state, bool hidden)
        : aspl::Device(context, params), state_(state), hidden_(hidden) {
        // On macOS 14.7.8 the installed c17bd88 driver returns fwvn via its
        // driver interface, but HAL clients see HasProperty=false / 'who?'.
        // Implementing the standard informational firmware getter alone does
        // not establish a client-visible identity. AudioServerPlugIn.h defines
        // CustomPropertyInfoList specifically for host/client marshalling;
        // libASPL registers AcID there as CFString with no qualifier or setter.
        // Keep UID/model/name stable rather than encoding builds in routing IDs
        // or user-facing names. No audio/clock/permission behavior is changed.
        // 26.6.2 had no installed device to compare; new HAL exposure on both
        // OS versions awaits user installation (offline interface tests only).
        RegisterCustomProperty('AcID', []() -> CFStringRef {
            return CFStringCreateWithCString(nullptr,
                "v1:" OINK_PLUGIN_VERSION ":" OINK_PLUGIN_GIT_SHA,
                kCFStringEncodingUTF8);
        });
    }
    bool GetIsHidden() const override { return hidden_; }
    OSStatus GetPropertyData(AudioObjectID objectID, pid_t clientPID,
        const AudioObjectPropertyAddress* address, UInt32 qualifierDataSize,
        const void* qualifierData, UInt32 inDataSize, UInt32* outDataSize,
        void* outData) const override {
        const auto status = aspl::Device::GetPropertyData(objectID, clientPID, address,
            qualifierDataSize, qualifierData, inDataSize, outDataSize, outData);
        // libASPL's CanBeDefault flag is not scoped. HAL's dflt property is:
        // advertising this input-only device as a default output candidate is
        // invalid even though it has no output streams. Keep inherited size,
        // object validation and the hidden feed's false result intact.
        if (status == noErr && address->mSelector == kAudioDevicePropertyDeviceCanBeDefaultDevice
            && address->mScope != kAudioObjectPropertyScopeInput && outData) {
            *static_cast<UInt32*>(outData) = 0;
        }
        return status;
    }
protected:
    // Both devices MUST use one clock epoch, even if started minutes apart.
    OSStatus GetZeroTimeStampImpl(UInt32, Float64* sample, UInt64* host, UInt64* seed) override {
        const double ticksPerFrame = 1e9 * state_->timebase.denom / state_->timebase.numer / Rate;
        const auto periods = static_cast<uint64_t>((mach_absolute_time() - state_->epoch) / ticksPerFrame / Period);
        *sample = static_cast<Float64>(periods * Period);
        *host = state_->epoch + static_cast<uint64_t>(*sample * ticksPerFrame);
        *seed = 1;
        return noErr;
    }
private:
    std::shared_ptr<State> state_;
    bool hidden_;
};
std::shared_ptr<aspl::Driver> createDriver() {
    auto context = std::make_shared<aspl::Context>(std::make_shared<aspl::Tracer>(aspl::Tracer::Mode::Noop));
    auto plugin = std::make_shared<aspl::Plugin>(context);
    auto state = std::make_shared<State>();
    for (bool input : {true, false}) {
        aspl::DeviceParameters params;
        params.Name = input ? "Oink Microphone" : "Oink Microphone Feed";
        params.Manufacturer = "Oink";
        params.DeviceUID = input ? "com.hachiwii.oink.microphone.input.v1" : "com.hachiwii.oink.microphone.feed.v1";
        params.ModelUID = "com.hachiwii.oink.microphone.v1";
        params.FirmwareVersion = "v1:" OINK_PLUGIN_VERSION ":" OINK_PLUGIN_GIT_SHA;
        params.SampleRate = Rate;
        params.ChannelCount = 1;
        params.ZeroTimeStampPeriod = Period;
        params.ClockDomain = 0x41637273;
        params.Latency = input ? Loopback::Delay : 0;
        // System Settings and many voice apps only list input devices that can be
        // the default, so the microphone must allow it; the hidden feed must not.
        params.CanBeDefault = input;
        params.CanBeDefaultForSystemSounds = false;
        auto device = std::make_shared<Device>(context, params, state, !input);
        aspl::StreamParameters stream;
        stream.Direction = input ? aspl::Direction::Input : aspl::Direction::Output;
        stream.Format = {Rate, kAudioFormatLinearPCM,
            kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagsNativeEndian,
            sizeof(float), 1, sizeof(float), 1, 32, 0};
        device->AddStreamAsync(stream);
        auto handler = std::make_shared<Handler>(state, input);
        device->SetControlHandler(handler);
        device->SetIOHandler(handler);
        plugin->AddDevice(device);
    }
    return std::make_shared<aspl::Driver>(context, plugin);
}
}
extern "C" __attribute__((visibility("default")))
void* OinkMicrophoneFactory(CFAllocatorRef, CFUUIDRef type) {
    if (!type || !CFEqual(type, kAudioServerPlugInTypeUUID)) return nullptr;
    static auto driver = createDriver();
    return driver->GetReference();
}
