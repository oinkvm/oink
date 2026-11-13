#import <Foundation/Foundation.h>
#import <CoreMedia/CoreMedia.h>

NS_ASSUME_NONNULL_BEGIN
// Bounded, shared PCM packets; no audio or video is persisted outside this mmap.
typedef void (^OinkForwardPCM)(const float *pcm, uint32_t frames, uint32_t rate,
    uint16_t channels, uint64_t hostNS, BOOL discontinuity);
@interface OinkBroadcastPipe : NSObject
- (nullable instancetype)initWithRoot:(NSString *)root;
@property(nonatomic, readonly) BOOL broadcasting;
@property(nonatomic) BOOL paused;
@property(nonatomic) BOOL stopRequested;
@property(nonatomic, readonly) BOOL appRequested;
- (BOOL)claimBroadcast;
- (void)endBroadcast;
- (BOOL)claimApp;
- (void)releaseApp;
- (void)writePCM:(const float *)pcm frames:(uint32_t)frames rate:(uint32_t)rate
    channels:(uint16_t)channels hostNS:(uint64_t)hostNS discontinuity:(BOOL)discontinuity;
- (void)drain:(OinkForwardPCM)consume;
@end
@interface OinkReplayPCM : NSObject
// Called synchronously on ReplayKit's serialized audio worker. Video never enters here.
- (BOOL)convert:(CMSampleBufferRef)buffer consume:(OinkForwardPCM)consume;
@end
// Jetsam accounts phys_footprint, not maxRSS (shared framework pages inflate RSS).
FOUNDATION_EXPORT uint64_t OinkBroadcastPhysicalFootprint(void);
NS_ASSUME_NONNULL_END
