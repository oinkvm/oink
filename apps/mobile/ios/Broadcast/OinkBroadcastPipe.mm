#import "OinkBroadcastPipe.h"
#import <AudioToolbox/AudioToolbox.h>
#import <mach/mach.h>
#import <sys/file.h>
#import <sys/mman.h>
#import <sys/stat.h>
#import <fcntl.h>
#import <unistd.h>
#include <atomic>
#include <algorithm>
#include <new>
#include <vector>
#include <cmath>
#include <cstring>

namespace {
constexpr uint32_t slots = 24, framesPerSlot = 1024;
struct Packet {
    std::atomic<uint32_t> ready;
    uint32_t frames, rate, epoch;
    uint16_t channels;
    bool discontinuity;
    uint64_t host, enqueuedHost;
    float pcm[framesPerSlot * 2];
};
struct Ring {
    std::atomic<uint32_t> active, paused, stop, epoch;
    // Each index has one writer. Release/acquire of ready protects the entire packet.
    std::atomic<uint32_t> write, read;
    Packet packets[slots];
};
static_assert(std::atomic<uint32_t>::is_always_lock_free);
int openLock(NSString *path) { return open(path.fileSystemRepresentation, O_CREAT | O_RDWR, 0600); }
bool heldByOther(int fd) {
    if (flock(fd, LOCK_EX | LOCK_NB) == 0) { flock(fd, LOCK_UN); return false; }
    return errno == EWOULDBLOCK;
}
}
@implementation OinkBroadcastPipe {
    Ring *_ring;
    int _fd, _broadcastFD, _appFD, _guardFD;
    bool _ownsBroadcast, _ownsApp, _dropped, _readDropped;
}
- (instancetype)initWithRoot:(NSString *)root {
    if (!(self = [super init])) return nil;
    _fd = _broadcastFD = _appFD = _guardFD = -1;
    NSString *ringPath = [root stringByAppendingPathComponent:@"broadcast-pcm.ring"];
    _fd = openLock(ringPath);
    if (_fd < 0) return nil;
    if (![[NSURL fileURLWithPath:ringPath] setResourceValue:@YES forKey:NSURLIsExcludedFromBackupKey error:nullptr]) return nil;
    // Only mapping initialization uses a syscall lock. PCM never takes it.
    if (flock(_fd, LOCK_EX) != 0) return nil;
    struct stat st = {};
    if (fstat(_fd, &st) != 0) { flock(_fd, LOCK_UN); return nil; }
    bool fresh = st.st_size != sizeof(Ring);
    if (fresh && ftruncate(_fd, sizeof(Ring)) != 0) { flock(_fd, LOCK_UN); return nil; }
    _ring = (Ring *)mmap(nullptr, sizeof(Ring), PROT_READ | PROT_WRITE, MAP_SHARED, _fd, 0);
    if (_ring == MAP_FAILED) { _ring = nullptr; flock(_fd, LOCK_UN); return nil; }
    if (fresh) new (_ring) Ring{};
    flock(_fd, LOCK_UN);
    _broadcastFD = openLock([root stringByAppendingPathComponent:@"broadcast-running.lock"]);
    _appFD = openLock([root stringByAppendingPathComponent:@"broadcast-app.lock"]);
    _guardFD = openLock([root stringByAppendingPathComponent:@"broadcast-control.lock"]);
    if (_broadcastFD < 0 || _appFD < 0 || _guardFD < 0) return nil;
    return self;
}
// BSD flock has no read-only ownership query. A probe briefly takes the lock
// when it is free; serialize probes and claims with a separate short guard so
// that a probe can never make a legitimate claim spuriously return OwnerBusy.
// This control lock never covers PCM, network shutdown or asynchronous work.
- (BOOL)broadcasting {
    if (flock(_guardFD, LOCK_EX) != 0) return NO;
    BOOL result = _ring->active.load() && (_ownsBroadcast || heldByOther(_broadcastFD));
    flock(_guardFD, LOCK_UN);
    return result;
}
- (BOOL)paused { return _ring->paused.load(); }
- (void)setPaused:(BOOL)value { _ring->paused.store(value); }
- (BOOL)stopRequested { return _ring->stop.load(); }
- (void)setStopRequested:(BOOL)value { _ring->stop.store(value); }
- (BOOL)appRequested {
    if (flock(_guardFD, LOCK_EX) != 0) return NO;
    BOOL result = _ownsApp || heldByOther(_appFD);
    flock(_guardFD, LOCK_UN);
    return result;
}
- (BOOL)claimBroadcast {
    if (flock(_guardFD, LOCK_EX) != 0) return NO;
    if (_ownsBroadcast || flock(_broadcastFD, LOCK_EX | LOCK_NB) != 0) {
        flock(_guardFD, LOCK_UN); return NO;
    }
    _ownsBroadcast = true;
    // Packet epochs keep an in-flight consumer from replaying old broadcasts.
    _ring->paused.store(0); _ring->stop.store(0);
    _ring->epoch.fetch_add(1);
    _ring->active.store(1);
    _dropped = true;
    flock(_guardFD, LOCK_UN);
    return YES;
}
- (void)endBroadcast {
    if (!_ownsBroadcast) return;
    if (flock(_guardFD, LOCK_EX) != 0) return;
    _ring->active.store(0);
    flock(_broadcastFD, LOCK_UN); _ownsBroadcast = false;
    flock(_guardFD, LOCK_UN);
}
- (BOOL)claimApp {
    if (flock(_guardFD, LOCK_EX) != 0) return NO;
    if (!_ownsApp) _ownsApp = flock(_appFD, LOCK_EX | LOCK_NB) == 0;
    BOOL result = _ownsApp;
    flock(_guardFD, LOCK_UN);
    return result;
}
- (void)releaseApp {
    if (!_ownsApp) return;
    if (flock(_guardFD, LOCK_EX) != 0) return;
    flock(_appFD, LOCK_UN); _ownsApp = false;
    flock(_guardFD, LOCK_UN);
}
- (void)writePCM:(const float *)pcm frames:(uint32_t)frames rate:(uint32_t)rate
    channels:(uint16_t)channels hostNS:(uint64_t)hostNS discontinuity:(BOOL)discontinuity {
    if (!frames || frames > framesPerSlot || channels < 1 || channels > 2) return;
    auto index = _ring->write.load();
    auto &packet = _ring->packets[index % slots];
    if (packet.ready.load(std::memory_order_acquire) != 0) { _dropped = true; return; }
    packet.epoch = _ring->epoch.load();
    packet.frames = frames; packet.rate = rate; packet.channels = channels; packet.host = hostNS;
    // Freshness is separate from the source timestamp: unknown hardware time
    // stays 0 when passed to Rust, but old unknown-time packets still expire.
    packet.enqueuedHost = (uint64_t)CMTimeConvertScale(CMClockGetTime(CMClockGetHostTimeClock()),
        1000000000, kCMTimeRoundingMethod_Default).value;
    packet.discontinuity = discontinuity || _dropped;
    memcpy(packet.pcm, pcm, frames * channels * sizeof(float));
    packet.ready.store(1, std::memory_order_release);
    _ring->write.store((index + 1) % slots);
    _dropped = false;
}
- (void)drain:(OinkForwardPCM)consume {
    for (uint32_t n = 0; n < slots; ++n) {
        auto index = _ring->read.load();
        auto &packet = _ring->packets[index % slots];
        if (!packet.ready.load(std::memory_order_acquire)) break;
        // ReplayKit PTS is host-clock time. Discard old handover backlog rather
        // than play it late. Unknown timestamps remain unknown throughout.
        uint64_t now = (uint64_t)CMTimeConvertScale(CMClockGetTime(CMClockGetHostTimeClock()),
            1000000000, kCMTimeRoundingMethod_Default).value;
        if (packet.epoch == _ring->epoch.load() &&
            (packet.enqueuedHost > now || now - packet.enqueuedHost <= 500000000ULL)) {
            consume(packet.pcm, packet.frames, packet.rate, packet.channels, packet.host, packet.discontinuity || _readDropped);
            _readDropped = false;
        } else { _readDropped = true; }
        packet.ready.store(0, std::memory_order_release);
        _ring->read.store((index + 1) % slots);
    }
}
- (void)dealloc {
    [self endBroadcast]; [self releaseApp];
    if (_ring) munmap(_ring, sizeof(Ring));
    for (int fd : {_fd, _broadcastFD, _appFD, _guardFD}) if (fd >= 0) close(fd);
}
@end

@implementation OinkReplayPCM {
    float _pcm[framesPerSlot * 2];
    std::vector<uint8_t> _bufferList;
}
- (BOOL)convert:(CMSampleBufferRef)buffer consume:(OinkForwardPCM)consume {
    if (CMSampleBufferGetNumSamples(buffer) == 0) return YES;
    auto description = CMSampleBufferGetFormatDescription(buffer);
    auto asbd = description ? CMAudioFormatDescriptionGetStreamBasicDescription(description) : nullptr;
    if (!asbd || asbd->mFormatID != kAudioFormatLinearPCM || asbd->mChannelsPerFrame < 1 ||
        asbd->mChannelsPerFrame > 2 || asbd->mSampleRate < 8000 || asbd->mSampleRate > 192000) return NO;
    bool floating = asbd->mFormatFlags & kAudioFormatFlagIsFloat;
    bool big = asbd->mFormatFlags & kAudioFormatFlagIsBigEndian;
    bool planar = asbd->mFormatFlags & kAudioFormatFlagIsNonInterleaved;
    unsigned bits = asbd->mBitsPerChannel, bytes = (bits + 7) / 8, channels = asbd->mChannelsPerFrame;
    if ((floating && bits != 32 && bits != 64) || (!floating &&
        (!(asbd->mFormatFlags & kAudioFormatFlagIsSignedInteger) || (bits != 16 && bits != 24 && bits != 32)))) return NO;
    // Packed samples only: reject an unsupported layout instead of reinterpreting it.
    if (asbd->mBytesPerFrame != bytes * (planar ? 1 : channels)) return NO;
    // iOS 27 simulator rejects interleaved PCM with ArrayTooSmall when given
    // a fixed two-buffer list and a null size output. Use CoreMedia's queried
    // size, including on the fill call, rather than assuming a layout. This
    // preserves samples/timestamps; retain the allocation across callbacks.
    size_t needed = 0;
    OSStatus status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(buffer, &needed,
        nullptr, 0, kCFAllocatorDefault, kCFAllocatorDefault, 0, nullptr);
    if ((status != noErr && status != kCMSampleBufferError_ArrayTooSmall) || needed < sizeof(AudioBufferList)) return NO;
    _bufferList.resize(needed);
    auto listPointer = reinterpret_cast<AudioBufferList *>(_bufferList.data());
    CMBlockBufferRef retained = nullptr;
    status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(buffer, &needed,
        listPointer, _bufferList.size(), kCFAllocatorDefault, kCFAllocatorDefault, 0, &retained);
    if (status != noErr) {
        NSLog(@"Oink PCM buffer-list error=%d flags=%u bits=%u channels=%u bytes/frame=%u frames=%lld",
            (int)status, (unsigned)asbd->mFormatFlags, bits, channels, (unsigned)asbd->mBytesPerFrame,
            (long long)CMSampleBufferGetNumSamples(buffer));
        if (retained) CFRelease(retained);
        return NO;
    }
    auto &list = *listPointer;
    bool valid = list.mNumberBuffers == (planar ? channels : 1);
    uint32_t frames = (uint32_t)CMSampleBufferGetNumSamples(buffer);
    for (unsigned b = 0; valid && b < list.mNumberBuffers; ++b)
        valid = list.mBuffers[b].mNumberChannels == (planar ? 1 : channels) &&
            list.mBuffers[b].mData && list.mBuffers[b].mDataByteSize >= uint64_t(frames) * asbd->mBytesPerFrame;
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(buffer);
    double seconds = CMTimeGetSeconds(pts);
    uint64_t host = CMTIME_IS_NUMERIC(pts) && std::isfinite(seconds) && seconds > 0 ? uint64_t(seconds * 1e9) : 0;
    for (uint32_t base = 0; valid && base < frames; base += framesPerSlot) {
        uint32_t count = std::min(framesPerSlot, frames - base);
        for (uint32_t f = 0; f < count; ++f) for (unsigned c = 0; c < channels; ++c) {
            auto p = (const uint8_t *)list.mBuffers[planar ? c : 0].mData +
                (base + f) * asbd->mBytesPerFrame + (planar ? 0 : c * bytes);
            uint64_t raw = 0;
            for (unsigned j = 0; j < bytes; ++j) raw |= uint64_t(p[j]) << (8 * (big ? bytes - j - 1 : j));
            double sample;
            if (floating && bits == 32) { uint32_t v = (uint32_t)raw; float s; memcpy(&s, &v, 4); sample = s; }
            else if (floating) { memcpy(&sample, &raw, 8); }
            else { int64_t value = (int64_t)(raw << (64 - bits)) >> (64 - bits); sample = double(value) / double(uint64_t(1) << (bits - 1)); }
            _pcm[f * channels + c] = std::isfinite(sample) ? float(sample) : 0;
        }
        consume(_pcm, count, (uint32_t)asbd->mSampleRate, channels,
            host ? host + uint64_t(base) * 1000000000ULL / uint32_t(asbd->mSampleRate) : 0, NO);
    }
    if (!valid) NSLog(@"Oink PCM layout error: flags=%u channels=%u buffers=%u first/channels=%u first/bytes=%u frames=%u bytes/frame=%u",
        (unsigned)asbd->mFormatFlags, channels, (unsigned)list.mNumberBuffers,
        list.mNumberBuffers ? (unsigned)list.mBuffers[0].mNumberChannels : 0,
        list.mNumberBuffers ? (unsigned)list.mBuffers[0].mDataByteSize : 0, frames, (unsigned)asbd->mBytesPerFrame);
    if (retained) CFRelease(retained);
    return valid;
}
@end
uint64_t OinkBroadcastPhysicalFootprint(void) {
    task_vm_info_data_t info = {};
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    return task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&info, &count) == KERN_SUCCESS ? info.phys_footprint : 0;
}
