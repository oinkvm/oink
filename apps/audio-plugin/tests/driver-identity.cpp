// Loads the built driver locally, never installs it or calls HAL/StartIO.
#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <cassert>
#include <cstdio>
#include <cstring>
#include <dlfcn.h>
#include <mach/mach_time.h>
#include <cmath>
#include <string>
#include <unistd.h>
#include <vector>

int main(int argc, char** argv) {
    assert(argc == 4);
    void* library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) { std::fprintf(stderr, "%s\n", dlerror()); return 1; }
    auto factory = reinterpret_cast<void* (*)(CFAllocatorRef, CFUUIDRef)>(
        dlsym(library, "OinkMicrophoneFactory"));
    assert(factory);
    auto driver = static_cast<AudioServerPlugInDriverRef>(factory(nullptr, kAudioServerPlugInTypeUUID));
    assert(driver && *driver);
    AudioObjectPropertyAddress address = {kAudioPlugInPropertyDeviceList,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioObjectID devices[2] = {};
    UInt32 size = 0;
    assert((*driver)->GetPropertyData(driver, kAudioObjectPlugInObject, getpid(), &address,
        0, nullptr, sizeof(devices), &size, devices) == noErr);
    assert(size == sizeof(devices));
    auto stringProperty = [&](AudioObjectID object, AudioObjectPropertySelector selector) {
        AudioObjectPropertyAddress property = {selector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        CFStringRef value = nullptr;
        UInt32 bytes = 0;
        assert((*driver)->GetPropertyData(driver, object, getpid(), &property, 0, nullptr,
            sizeof(value), &bytes, &value) == noErr);
        assert(bytes == sizeof(value) && value);
        char text[256] = {};
        assert(CFStringGetCString(value, text, sizeof(text), kCFStringEncodingUTF8));
        CFRelease(value);
        return std::string(text);
    };
    auto integerProperty = [&](AudioObjectID object, AudioObjectPropertySelector selector, AudioObjectPropertyScope scope) {
        AudioObjectPropertyAddress property = {selector, scope, kAudioObjectPropertyElementMain};
        UInt32 value = 0, bytes = 0;
        assert((*driver)->GetPropertyData(driver, object, getpid(), &property, 0, nullptr,
            sizeof(value), &bytes, &value) == noErr);
        assert(bytes == sizeof(value));
        return value;
    };
    unsigned inputs = 0, feeds = 0;
    for (auto device : devices) {
        const auto uid = stringProperty(device, kAudioDevicePropertyDeviceUID);
        const bool input = uid == "com.hachiwii.oink.microphone.input.v1";
        assert(input || uid == "com.hachiwii.oink.microphone.feed.v1");
        input ? ++inputs : ++feeds;
        assert(stringProperty(device, kAudioDevicePropertyModelUID) == "com.hachiwii.oink.microphone.v1");
        assert(stringProperty(device, kAudioObjectPropertyName) == (input ? "Oink Microphone" : "Oink Microphone Feed"));
        assert(stringProperty(device, kAudioObjectPropertyManufacturer) == "Oink");
        assert(integerProperty(device, kAudioDevicePropertyIsHidden, kAudioObjectPropertyScopeGlobal) == !input);
        const auto scope = input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput;
        // HAL asks eligibility separately for input and output. An input-only
        // device must never advertise itself as a default output candidate.
        for (auto direction : {kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput}) {
            const auto eligible = integerProperty(device, kAudioDevicePropertyDeviceCanBeDefaultDevice, direction);
            std::fprintf(stderr, "device %u default %s: %u\n", device,
                direction == kAudioObjectPropertyScopeInput ? "input" : "output", eligible);
            assert(eligible == (input && direction == kAudioObjectPropertyScopeInput));
            assert(integerProperty(device, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, direction) == 0);
            AudioObjectPropertyAddress list = {kAudioDevicePropertyStreams, direction, kAudioObjectPropertyElementMain};
            UInt32 streamBytes = 0;
            assert((*driver)->GetPropertyDataSize(driver, device, getpid(), &list, 0, nullptr, &streamBytes) == noErr);
            assert(streamBytes == ((direction == scope) ? sizeof(AudioObjectID) : 0));
        }
        // Test inherited ABI validation too; our scope override must not write
        // a UInt32 into a short buffer or change read-only property behavior.
        AudioObjectPropertyAddress eligibility = {kAudioDevicePropertyDeviceCanBeDefaultDevice,
            kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
        UInt32 shortBuffer = 0xabcdef01, returnedSize = 0;
        assert((*driver)->GetPropertyData(driver, device, getpid(), &eligibility, 0, nullptr,
            1, &returnedSize, &shortBuffer) == kAudioHardwareBadPropertySizeError);
        assert(shortBuffer == 0xabcdef01);
        Boolean settable = true;
        assert((*driver)->IsPropertySettable(driver, device, getpid(), &eligibility, &settable) == noErr);
        assert(!settable);
        assert(integerProperty(device, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, scope) == 0);
        AudioObjectPropertyAddress streams = {kAudioDevicePropertyStreams, scope, kAudioObjectPropertyElementMain};
        AudioObjectID stream = 0;
        UInt32 bytes = 0;
        assert((*driver)->GetPropertyData(driver, device, getpid(), &streams, 0, nullptr,
            sizeof(stream), &bytes, &stream) == noErr);
        assert(bytes == sizeof(stream));
        AudioObjectPropertyAddress formatProperty = {kAudioStreamPropertyVirtualFormat,
            kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        AudioStreamBasicDescription format = {};
        assert((*driver)->GetPropertyData(driver, stream, getpid(), &formatProperty, 0, nullptr,
            sizeof(format), &bytes, &format) == noErr);
        assert(format.mSampleRate == 48000 && format.mChannelsPerFrame == 1);
        assert(format.mFormatID == kAudioFormatLinearPCM && format.mBitsPerChannel == 32);
        assert(format.mFormatFlags == (kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagsNativeEndian));
    }
    assert(inputs == 1 && feeds == 1);
    // Exercise the real clock callback locally without starting HAL or device IO.
    // A zero/stalled timestamp or a future host time can break HAL scheduling.
    mach_timebase_info_data_t timebase{};
    assert(mach_timebase_info(&timebase) == KERN_SUCCESS);
    const double ticksPerFrame = 1e9 * timebase.denom / timebase.numer / 48000;
    Float64 previousSample[2] = {};
    UInt64 previousHost[2] = {};
    for (unsigned iteration = 0; iteration < 4; ++iteration) {
        for (unsigned index = 0; index < 2; ++index) {
            const auto device = devices[index];
            assert(integerProperty(device, kAudioDevicePropertyZeroTimeStampPeriod,
                kAudioObjectPropertyScopeGlobal) == 256);
            Float64 sample = 0;
            UInt64 host = 0, seed = 0;
            assert((*driver)->GetZeroTimeStamp(driver, device, 0, &sample, &host, &seed) == noErr);
            assert(std::isfinite(sample) && sample >= 0 && std::fmod(sample, 256) == 0);
            assert(host <= mach_absolute_time() && seed == 1);
            if (iteration) {
                assert(sample > previousSample[index] && host > previousHost[index]);
                const auto expectedTicks = (sample - previousSample[index]) * ticksPerFrame;
                assert(std::abs(double(host - previousHost[index]) - expectedTicks) <= 2);
            }
            previousSample[index] = sample;
            previousHost[index] = host;
        }
        usleep(20000);
    }
    const std::string expected = std::string("v1:") + argv[2] + ":" + argv[3];
    for (auto device : devices) {
        address.mSelector = kAudioObjectPropertyCustomPropertyInfoList;
        assert((*driver)->GetPropertyDataSize(driver, device, getpid(), &address, 0, nullptr, &size) == noErr);
        assert(size % sizeof(AudioServerPlugInCustomPropertyInfo) == 0);
        std::vector<AudioServerPlugInCustomPropertyInfo> properties(size / sizeof(AudioServerPlugInCustomPropertyInfo));
        assert((*driver)->GetPropertyData(driver, device, getpid(), &address, 0, nullptr,
            size, &size, properties.data()) == noErr);
        bool declared = false;
        for (const auto& property : properties) {
            if (property.mSelector != 'AcID') continue;
            assert(property.mPropertyDataType == kAudioServerPlugInCustomPropertyDataTypeCFString);
            assert(property.mQualifierDataType == kAudioServerPlugInCustomPropertyDataTypeNone);
            declared = true;
        }
        assert(declared);
        address.mSelector = 'AcID';
        assert((*driver)->HasProperty(driver, device, getpid(), &address));
        Boolean settable = true;
        assert((*driver)->IsPropertySettable(driver, device, getpid(), &address, &settable) == noErr);
        assert(!settable);
        assert((*driver)->GetPropertyDataSize(driver, device, getpid(), &address, 0, nullptr, &size) == noErr);
        assert(size == sizeof(CFStringRef));
        CFStringRef value = nullptr;
        assert((*driver)->GetPropertyData(driver, device, getpid(), &address, 0, nullptr,
            sizeof(value), &size, &value) == noErr);
        assert(value && CFGetTypeID(value) == CFStringGetTypeID());
        char text[256] = {};
        assert(CFStringGetCString(value, text, sizeof(text), kCFStringEncodingUTF8));
        assert(expected == text);
        CFRelease(value);
        std::printf("device %u: read-only CFString AcID declared and build identity matches\n", device);
    }
    // Driver owns static objects; unloading while those still exist is unsafe.
    // Process exit releases the module. No IO was started.
}
