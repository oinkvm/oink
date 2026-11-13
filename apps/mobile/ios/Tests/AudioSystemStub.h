#import <Foundation/Foundation.h>
#import "../App/Audio/OinkAudioIO.h"

// Test-bundle-only substitutions. No hardware session activation or audio I/O.
@interface AudioSystemStub : NSObject
+ (void)install;
+ (void)postLegacySuspensionInterruption;
+ (void)postSuspensionReasonInterruption;
+ (void)uninstall;
+ (NSUInteger)activationCount;
+ (NSUInteger)deactivationCount;
+ (NSUInteger)startCount;
+ (NSUInteger)lastCategoryOptions;
+ (BOOL)isRunning;
+ (void)setPermissionGranted:(BOOL)granted;
+ (void)setPermissionDenied;
+ (BOOL)hasPermissionRequest;
+ (void)resolvePermission:(BOOL)granted;
// The next activation fails with this AVAudioSession error code (another app's audio holds the session).
+ (void)failNextActivationWithCode:(NSInteger)code;
@end
