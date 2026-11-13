#import "AudioSystemStub.h"
#import <AVFAudio/AVFAudio.h>
#import <UIKit/UIKit.h>
#import <objc/runtime.h>
#include <unordered_map>
#include <vector>

namespace {
struct Replacement { Method method; IMP original; };
std::vector<Replacement> replacements;
NSUInteger activations, deactivations, starts, categoryOptions;
NSInteger failNextActivation;
BOOL running;
NSUInteger permissionState;
void (^permissionReply)(BOOL);

void replace(Class cls, SEL selector, IMP implementation) {
    Method method = class_getInstanceMethod(cls, selector);
    NSCAssert(method, @"Missing test selector %@", NSStringFromSelector(selector));
    replacements.push_back({method, method_setImplementation(method, implementation)});
}
BOOL setActive(id, SEL, BOOL active, NSError **error) {
    if (active && failNextActivation) {
        if (error) *error = [NSError errorWithDomain:NSOSStatusErrorDomain code:failNextActivation userInfo:nil];
        failNextActivation = 0;
        return NO;
    }
    if (active) ++activations;
    else ++deactivations;
    return YES;
}
BOOL setActiveOptions(id object, SEL selector, BOOL active, AVAudioSessionSetActiveOptions, NSError **error) {
    return setActive(object, selector, active, error);
}
BOOL setCategory(id, SEL, NSString *, NSString *, AVAudioSessionCategoryOptions options, NSError **) {
    categoryOptions = options;
    return YES;
}
BOOL setPreference(id, SEL, double, NSError **) { return YES; }
NSUInteger recordPermission(id, SEL) {
    return permissionState;
}
void requestPermission(id, SEL, void (^reply)(BOOL)) { permissionReply = [reply copy]; }
UIApplicationState applicationState(id, SEL) { return UIApplicationStateActive; }
// The engine stays stubbed, but an IO bound to a real Rust backend still runs
// its handle lifecycle: the core admits a running direction only after the
// native handle is attached, so skipping prepare would fake an unadmitted run.
struct Handles { OinkAudioBackend backend; void *output = nullptr, *input = nullptr; };
std::unordered_map<const void *, Handles> handles;
OinkAudioBackend backendOf(id io) {
    Ivar ivar = class_getInstanceVariable(OinkAudioIO.class, "_backend");
    NSCAssert(ivar, @"Missing OinkAudioIO backend");
    return *reinterpret_cast<OinkAudioBackend *>(reinterpret_cast<uint8_t *>((__bridge void *)io) + ivar_getOffset(ivar));
}
void stopIO(id io, SEL) {
    running = NO;
    auto found = handles.find((__bridge const void *)io);
    if (found == handles.end()) return;
    Handles entry = found->second;
    handles.erase(found);
    if (entry.backend.revoke) entry.backend.revoke(entry.output, entry.input);
    if (entry.backend.releaseHandles) entry.backend.releaseHandles(entry.backend.owner, entry.output, entry.input);
}
BOOL startIO(id io, SEL selector, BOOL playback, BOOL microphone, uint64_t epoch, NSError **error) {
    stopIO(io, selector);
    Handles entry{backendOf(io)};
    if (entry.backend.prepare) {
        const bool prepared = entry.backend.prepare(entry.backend.owner, playback ? 48000 : 0,
            microphone ? 48000 : 0, microphone ? 1 : 0, epoch, &entry.output, &entry.input);
        if (entry.output || entry.input) handles[(__bridge const void *)io] = entry;
        if (!prepared) {
            // Like OinkAudioIO, a failed prepare releases what it created.
            stopIO(io, selector);
            if (error) *error = [NSError errorWithDomain:@"AudioSystemStub" code:1
                userInfo:@{NSLocalizedDescriptionKey: @"Audio media core is not prepared"}];
            return NO;
        }
    }
    ++starts; running = YES; return YES;
}
}

@implementation AudioSystemStub
+ (void)postLegacySuspensionInterruption {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    [NSNotificationCenter.defaultCenter postNotificationName:AVAudioSessionInterruptionNotification object:nil
        userInfo:@{AVAudioSessionInterruptionTypeKey: @(AVAudioSessionInterruptionTypeBegan),
                   AVAudioSessionInterruptionWasSuspendedKey: @YES}];
#pragma clang diagnostic pop
}
+ (void)postSuspensionReasonInterruption {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    [NSNotificationCenter.defaultCenter postNotificationName:AVAudioSessionInterruptionNotification object:nil
        userInfo:@{AVAudioSessionInterruptionTypeKey: @(AVAudioSessionInterruptionTypeBegan),
                   AVAudioSessionInterruptionReasonKey: @(AVAudioSessionInterruptionReasonAppWasSuspended)}];
#pragma clang diagnostic pop
}
+ (void)install {
    NSAssert(replacements.empty(), @"Test stubs already installed");
    activations = deactivations = starts = categoryOptions = 0;
    failNextActivation = 0;
    running = NO;
    handles.clear();
    permissionState = AVAudioApplicationRecordPermissionGranted;
    replace(AVAudioSession.class, @selector(setActive:error:), (IMP)setActive);
    replace(AVAudioSession.class, @selector(setActive:withOptions:error:), (IMP)setActiveOptions);
    replace(AVAudioSession.class, @selector(setCategory:mode:options:error:), (IMP)setCategory);
    replace(AVAudioSession.class, @selector(setPreferredSampleRate:error:), (IMP)setPreference);
    replace(AVAudioSession.class, @selector(setPreferredIOBufferDuration:error:), (IMP)setPreference);
    replace(AVAudioApplication.class, @selector(recordPermission), (IMP)recordPermission);
    replace(object_getClass(AVAudioApplication.class), @selector(requestRecordPermissionWithCompletionHandler:), (IMP)requestPermission);
    replace(UIApplication.class, @selector(applicationState), (IMP)applicationState);
    replace(OinkAudioIO.class, @selector(startWithPlayback:microphone:epoch:error:), (IMP)startIO);
    replace(OinkAudioIO.class, @selector(stop), (IMP)stopIO);
}
+ (void)uninstall {
    for (auto entry = replacements.rbegin(); entry != replacements.rend(); ++entry)
        method_setImplementation(entry->method, entry->original);
    replacements.clear();
    permissionReply = nil;
}
+ (void)failNextActivationWithCode:(NSInteger)code { failNextActivation = code; }
+ (NSUInteger)activationCount { return activations; }
+ (NSUInteger)deactivationCount { return deactivations; }
+ (NSUInteger)startCount { return starts; }
+ (NSUInteger)lastCategoryOptions { return categoryOptions; }
+ (BOOL)isRunning { return running; }
+ (void)setPermissionGranted:(BOOL)granted {
    permissionState = granted ? AVAudioApplicationRecordPermissionGranted : AVAudioApplicationRecordPermissionUndetermined;
}
+ (void)setPermissionDenied { permissionState = AVAudioApplicationRecordPermissionDenied; }
+ (BOOL)hasPermissionRequest { return permissionReply != nil; }
+ (void)resolvePermission:(BOOL)granted {
    permissionState = granted ? AVAudioApplicationRecordPermissionGranted : AVAudioApplicationRecordPermissionDenied;
    void (^reply)(BOOL) = permissionReply;
    permissionReply = nil;
    NSAssert(reply, @"No pending permission request");
    reply(granted);
}
@end
