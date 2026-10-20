// Main-thread backend-owned Sparkle driver. No windows, WebViews or install-on-quit
// policy are delegated to Sparkle's standard UI driver.
#import <AppKit/AppKit.h>
#import <Sparkle/Sparkle.h>
#include <stdint.h>
typedef void (*UpdateEvent)(uint32_t,uint64_t,uint64_t,uint32_t,const char *,const char *);
@interface OinkUpdateDriver : NSObject <SPUUserDriver, SPUUpdaterDelegate>
@property(nonatomic, strong) SPUUpdater *updater;
@property UpdateEvent callback;
@property BOOL beta;
@property BOOL cancelled;
@property BOOL armed;
@property BOOL checkPending;
@property uint64_t received;
@property uint64_t expected;
@property uint64_t build;
@property(copy) void (^found)(SPUUserUpdateChoice);
@property(copy) void (^ready)(SPUUserUpdateChoice);
@property(copy) void (^cancelDownload)(void);
- (void)event:(uint32_t)event progress:(uint32_t)progress;
- (void)cancel;
- (void)check;
@end
static OinkUpdateDriver *owner;
static char updateReadinessContext;
@implementation OinkUpdateDriver
@synthesize updater=_updater;
- (void)setUpdater:(SPUUpdater *)updater {
    if (_updater==updater) return;
    [_updater removeObserver:self forKeyPath:@"canCheckForUpdates" context:&updateReadinessContext];
    _updater=updater;
    [_updater addObserver:self forKeyPath:@"canCheckForUpdates" options:0 context:&updateReadinessContext];
}
- (void)dealloc {
    [_updater removeObserver:self forKeyPath:@"canCheckForUpdates" context:&updateReadinessContext];
}
- (void)observeValueForKeyPath:(NSString *)keyPath ofObject:(id)object change:(NSDictionary *)change context:(void *)context {
    if (context!=&updateReadinessContext) {
        [super observeValueForKeyPath:keyPath ofObject:object change:change context:context];return;
    }
    if (!_checkPending || !_updater.canCheckForUpdates) return;
    // Sparkle 2.9.6 also uses sessionInProgress while probing the installer to
    // schedule its next automatic check. That probe has no didFinishUpdateCycle
    // callback. Observe the public KVO-compliant readiness property instead.
    // It changes before sessionInProgress clears, so hand off after Sparkle's
    // current callback unwinds; never reenter its setter/completion stack.
    dispatch_async(dispatch_get_main_queue(), ^{
        if (self->_checkPending) [self check];
    });
}
- (void)check {
    // Retain one user intent while Sparkle is busy. Readiness notifications
    // deliver it once; no timer, failed-check retry, or private installer state.
    if (!_updater.canCheckForUpdates || _updater.sessionInProgress) {_checkPending=YES;return;}
    _checkPending=NO;
    [_updater checkForUpdates];
}
- (void)event:(uint32_t)event progress:(uint32_t)progress {
    if (_callback) _callback(event,_build,_expected,progress,"","");
}
- (NSString *)feedURLStringForUpdater:(SPUUpdater *)updater {
    return [NSString stringWithFormat:@"https://updates.oink.hachiwii.com/mac/v1/mac-2026-01/%@/appcast.xml",_beta?@"beta":@"stable"];
}
- (NSSet<NSString *> *)allowedChannelsForUpdater:(SPUUpdater *)updater {return _beta?[NSSet setWithObject:@"beta"]:[NSSet set];}
- (void)showUpdatePermissionRequest:(SPUUpdatePermissionRequest *)request reply:(void (^)(SUUpdatePermissionResponse *))reply {
    reply([[SUUpdatePermissionResponse alloc] initWithAutomaticUpdateChecks:NO sendSystemProfile:NO]);
}
- (void)showUserInitiatedUpdateCheckWithCancellation:(void (^)(void))cancellation {
    _cancelDownload=[cancellation copy]; _cancelled=NO; [self event:1 progress:0];
}
- (void)showUpdateFoundWithAppcastItem:(SUAppcastItem *)item state:(SPUUserUpdateState *)state reply:(void (^)(SPUUserUpdateChoice))reply {
    _checkPending=NO; // An in-flight check supplied the requested result.
    _cancelDownload=nil; _cancelled=NO; _build=item.versionString.longLongValue; _expected=item.contentLength;
    if (_build==0 || item.informationOnlyUpdate || item.majorUpgrade) {reply(SPUUserUpdateChoiceDismiss); [self event:8 progress:4]; return;}
    // Sparkle chooses only one localized <description>. A single signed custom
    // element preserves both translations as JSON for the common Rust validator
    // and Candidate shape; never fall back to localized HTML itemDescription.
    id notes=item.propertiesDictionary[@"oink:releaseNotes"];
    if (![notes isKindOfClass:NSString.class]) notes=@"";
    // A cached installer can send an Installing state here. Never reply Install
    // before a new backend drain token; retain it as the ready continuation.
    if (state.stage==SPUUserUpdateStageInstalling) {
        if (_callback) _callback(2,_build,_expected,0,item.displayVersionString.UTF8String,[notes UTF8String]);
        _armed=YES; _ready=[reply copy]; [self event:5 progress:0]; return;
    }
    _found=[reply copy];
    if (_callback) _callback(2,_build,_expected,0,item.displayVersionString.UTF8String,[notes UTF8String]);
}
- (void)showUpdateReleaseNotesWithDownloadData:(SPUDownloadData *)data {}
- (void)showUpdateReleaseNotesFailedToDownloadWithError:(NSError *)error {[self event:8 progress:1];}
- (void)showUpdateNotFoundWithError:(NSError *)error acknowledgement:(void (^)(void))acknowledgement {
    _checkPending=NO;
    NSNumber *reason=error.userInfo[SPUNoUpdateFoundReasonKey];
    [self event:reason.integerValue==SPUNoUpdateFoundReasonOnLatestVersion?0:8 progress:4]; acknowledgement();
}
- (void)showUpdaterError:(NSError *)error acknowledgement:(void (^)(void))acknowledgement {
    _checkPending=NO;
    // Finite categories only; native error descriptions can contain local paths.
    [self event:8 progress:[error.domain isEqualToString:NSURLErrorDomain]?1:2]; acknowledgement();
}
- (void)showDownloadInitiatedWithCancellation:(void (^)(void))cancellation {_cancelDownload=[cancellation copy];_received=0;[self event:3 progress:0];}
- (void)showDownloadDidReceiveExpectedContentLength:(uint64_t)length {_expected=length;}
- (void)showDownloadDidReceiveDataOfLength:(uint64_t)length {_received+=length;[self event:3 progress:_expected?(uint32_t)MIN(100.0,100.0*(double)_received/(double)_expected):0];}
- (void)showDownloadDidStartExtractingUpdate {_cancelDownload=nil;_armed=YES;[self event:4 progress:0];}
- (void)showExtractionReceivedProgress:(double)progress {[self event:4 progress:(uint32_t)(100*MAX(0,MIN(1,progress)))];}
- (void)showReadyToInstallAndRelaunch:(void (^)(SPUUserUpdateChoice))reply {
    _armed=YES;
    // Sparkle 2.9.6 SPUUserDriver.h: Ready Skip cancels the installer without
    // skipping the version. Dismiss would leave install-on-quit armed.
    if (_cancelled) {reply(SPUUserUpdateChoiceSkip);return;}
    _ready=[reply copy]; [self event:5 progress:100];
}
- (void)showInstallingUpdateWithApplicationTerminated:(BOOL)terminated retryTerminatingApplication:(void (^)(void))retry {[self event:6 progress:0];}
- (void)showUpdateInstalledAndRelaunched:(BOOL)relaunched acknowledgement:(void (^)(void))acknowledgement {[self event:7 progress:100];acknowledgement();}
- (void)dismissUpdateInstallation {
    _found=nil;_ready=nil;_cancelDownload=nil;_armed=NO;[self event:9 progress:0];
}
- (void)cancel {
    _checkPending=NO;
    _cancelled=YES;
    if (_ready) {void (^reply)(SPUUserUpdateChoice)=_ready;_ready=nil;reply(SPUUserUpdateChoiceSkip);}
    else if (_found) {void (^reply)(SPUUserUpdateChoice)=_found;_found=nil;reply(SPUUserUpdateChoiceDismiss);}
    else if (_cancelDownload) {void (^cancel)(void)=_cancelDownload;_cancelDownload=nil;cancel();}
    // Extraction has no cancellation continuation until showReady. Keep the
    // application alive until dismissUpdateInstallation confirms abort.
}
@end
bool oink_update_start(UpdateEvent callback,bool beta,bool checks) {
    NSCAssert(NSThread.isMainThread,@"main thread");
    NSBundle *bundle=NSBundle.mainBundle;
    // Bundle policy/public-key fields are sealed and checked by packaging;
    // Sparkle validates its own configuration when startUpdater runs.
    if (owner) return true;
    owner=[OinkUpdateDriver new];owner.callback=callback;owner.beta=beta;
    owner.updater=[[SPUUpdater alloc] initWithHostBundle:bundle applicationBundle:bundle userDriver:owner delegate:owner];
    // The custom driver owns download/install intent; Sparkle only schedules checks.
    owner.updater.automaticallyDownloadsUpdates=NO;
    // Oink's persisted policy is authoritative, including a corrupt/off policy.
    // Sparkle persists its own mirror in defaults; an older mirror must not
    // override the setting shown by Oink after restart.
    owner.updater.automaticallyChecksForUpdates=checks;
    NSError *error=nil;
    if (![owner.updater startUpdater:&error]) {owner.updater=nil;owner=nil;return false;}
    return true;
}
void oink_update_check(void) {[owner check];}
void oink_update_download(void) {
    if (owner.found) {void (^reply)(SPUUserUpdateChoice)=owner.found;owner.found=nil;reply(SPUUserUpdateChoiceInstall);}
}
bool oink_update_install(void) {
    if (!owner.ready || owner.cancelled) return false;
    void (^reply)(SPUUserUpdateChoice)=owner.ready;owner.ready=nil;reply(SPUUserUpdateChoiceInstall);return true;
}
void oink_update_cancel(void) {[owner cancel];}
bool oink_update_quit_allowed(void) {return !owner.armed;}
bool oink_update_checks_enabled(void) {return owner.updater.automaticallyChecksForUpdates;}
void oink_update_preferences(bool checks,bool beta) {
    [owner cancel];owner.beta=beta;owner.updater.automaticallyChecksForUpdates=checks;
    [owner.updater resetUpdateCycle];
}

uint64_t oink_update_current_build(void) {return [NSBundle.mainBundle.infoDictionary[@"CFBundleVersion"] longLongValue];}
