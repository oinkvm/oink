#import <Foundation/Foundation.h>

NS_ASSUME_NONNULL_BEGIN
// Includes the legacy suspension marker for delayed interruption notifications.
FOUNDATION_EXPORT BOOL OinkAudioInterruptionWasSuspended(NSNotification *notification);
// All control callbacks run on the main thread. PCM callbacks run on native audio
// threads: no allocation, locking, logging, Swift objects, or network operations.
// Handles are borrowed until stop has closed admission and drained both callbacks.
typedef struct {
    void * _Nullable owner;
    bool (* _Nullable prepare)(void * _Nullable owner, double outputRate,
        double inputRate, uint32_t inputChannels, uint64_t epoch,
        void * _Nullable * _Nonnull output, void * _Nullable * _Nonnull input);
    void (* _Nullable render)(void * _Nullable output, float * _Nonnull interleaved,
        uint32_t frames, uint64_t hostTimeNs);
    void (* _Nullable capture)(void * _Nullable input, const float * _Nonnull interleaved,
        uint32_t frames, uint64_t hostTimeNs);
    void (* _Nullable revoke)(void * _Nullable output, void * _Nullable input);
    void (* _Nullable releaseHandles)(void * _Nullable owner, void * _Nullable output,
        void * _Nullable input);
} OinkAudioBackend;

@interface OinkAudioIO : NSObject
- (instancetype)initWithBackend:(OinkAudioBackend)backend;
// Session must already be active. Output is stereo at actual hardware rate;
// input is mono/stereo at inputNode's actual rate. A new epoch on every start.
- (BOOL)startWithPlayback:(BOOL)playback microphone:(BOOL)microphone
                   epoch:(uint64_t)epoch error:(NSError **)error;
- (void)stop;
- (BOOL)ownsEngine:(id)engine;
@property(nonatomic, readonly) uint64_t renderedFrames;
@property(nonatomic, readonly) uint64_t capturedFrames;
@property(nonatomic, readonly) uint32_t maximumOutputBatchFrames;
@property(nonatomic, readonly) uint32_t maximumInputBatchFrames;
@property(nonatomic, readonly) double outputRate;
@property(nonatomic, readonly) double inputRate;
@end
NS_ASSUME_NONNULL_END
