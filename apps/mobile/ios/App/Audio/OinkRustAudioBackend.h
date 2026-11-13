#import "OinkAudioIO.h"

NS_ASSUME_NONNULL_BEGIN
// Own this object for the entire MobileAudioSession lifetime. Handles are exposed
// only on the control thread for core attach/detach; never call RT functions here.
@interface OinkRustAudioBackend : NSObject
@property(nonatomic, readonly) OinkAudioBackend callbacks;
@property(nonatomic, readonly) uint64_t outputHandleAddress;
@property(nonatomic, readonly) uint64_t inputHandleAddress;
// Called after create and before engine start, then after callback/worker quiescence and destroy. The
// core integration must synchronously attach and acknowledgeStopped, respectively.
@property(nonatomic, copy, nullable) BOOL (^attachHandles)(uint64_t generation, uint64_t output, uint64_t input);
@property(nonatomic, copy, nullable) void (^detachHandles)(uint64_t output, uint64_t input);
@end
NS_ASSUME_NONNULL_END
