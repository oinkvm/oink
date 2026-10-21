// Isolated tests of the production Objective-C continuation owner. No updater,
// NSApplication, installed bundle, permissions, network, or real installer starts.
#import "update.m"
#include <assert.h>
static NSString *candidateNotes;
static NSMutableArray<NSArray<NSNumber *> *> *timingRecords;
static void captureTiming(uint32_t boundary,uint64_t now,uint64_t elapsed,uint64_t previous,uint64_t bytes) {
    [timingRecords addObject:@[@(boundary),@(now),@(elapsed),@(previous),@(bytes)]];
}
@interface TestUpdateTiming : OinkUpdateTiming
@property uint64_t clock;
@end
@implementation TestUpdateTiming
- (uint64_t)now {return _clock+=10;}
@end
static uint32_t failureCategory;
static void captureFailure(uint32_t event,uint64_t build,uint64_t size,uint32_t progress,const char *version,const char *notes) {
    if (event==8) failureCategory=progress;
}
static void captureCandidate(uint32_t event,uint64_t build,uint64_t size,uint32_t progress,const char *version,const char *notes) {
    if (event==2) {
        assert(build==2);
        candidateNotes=[NSString stringWithUTF8String:notes];
    }
}
@interface TestUpdateState : NSObject
@end
@implementation TestUpdateState
- (SPUUserUpdateStage)stage {return SPUUserUpdateStageNotDownloaded;}
@end
@interface TestUpdater : NSObject
@property BOOL sessionInProgress;
@property BOOL canCheckForUpdates;
@property NSUInteger checks;
@end
@implementation TestUpdater
- (void)checkForUpdates {
    // Sparkle ignores this call until its previous driver has finished cleanup.
    if (!_canCheckForUpdates || _sessionInProgress) return;
    _checks++; self.sessionInProgress=YES; self.canCheckForUpdates=NO;
}
@end
static void mainQueueTurn(void) {
    __block BOOL done=NO;
    CFRunLoopRef loop=CFRunLoopGetCurrent();
    dispatch_async(dispatch_get_main_queue(), ^{done=YES;CFRunLoopStop(loop);});
    while (!done) CFRunLoopRun();
}
int main(void) {
    @autoreleasepool {
        OinkUpdateDriver *stableDriver=[OinkUpdateDriver new];
        SPUUpdater *stableUpdater=(SPUUpdater *)[TestUpdater new];
        assert([[stableDriver feedURLStringForUpdater:stableUpdater] isEqualToString:@"https://updates.oink.hachiwii.com/mac/v1/mac-2026-01/stable/appcast.xml"]);
        assert([stableDriver allowedChannelsForUpdater:stableUpdater].count==0);
        // Sparkle wraps URLSession failures in SUDownloadError before showing
        // them to our driver. A TLS failure must stay Network across the bridge.
        owner=[OinkUpdateDriver new];owner.callback=captureFailure;
        NSError *tls=[NSError errorWithDomain:NSURLErrorDomain code:NSURLErrorSecureConnectionFailed userInfo:nil];
        NSError *download=[NSError errorWithDomain:SUSparkleErrorDomain code:SUDownloadError userInfo:@{NSUnderlyingErrorKey:tls}];
        __block BOOL errorAcknowledged=NO;
        [owner showUpdaterError:download acknowledgement:^{errorAcknowledged=YES;}];
        assert(errorAcknowledged && failureCategory==1);
        [owner showUpdaterError:tls acknowledgement:^{}];
        assert(failureCategory==1);
        NSError *signature=[NSError errorWithDomain:SUSparkleErrorDomain code:SUValidationError userInfo:nil];
        [owner showUpdaterError:signature acknowledgement:^{}];
        assert(failureCategory==2);
        NSError *invalidDownload=[NSError errorWithDomain:SUSparkleErrorDomain code:SUDownloadError userInfo:@{NSUnderlyingErrorKey:signature}];
        [owner showUpdaterError:invalidDownload acknowledgement:^{}];
        assert(failureCategory==2); // A download wrapper alone is not Network.
        [owner showUpdaterError:[NSError errorWithDomain:SUSparkleErrorDomain code:SUDownloadError userInfo:nil] acknowledgement:^{}];
        assert(failureCategory==2);

        // Public legacy initializer is sufficient for an isolated parsed-item
        // fixture; no updater or application state resolver starts here.
        NSDictionary *notes=@{@"zh-Hans":@"修复问题。",@"en":@"Fix a problem.",@"ja":@"テスト",@"ko":@"테스트",@"es-MX":@"Prueba"};
        NSString *json=[[NSString alloc] initWithData:[NSJSONSerialization dataWithJSONObject:notes options:0 error:NULL] encoding:NSUTF8StringEncoding];
        NSMutableDictionary *properties=[@{@"sparkle:version":@"2",@"sparkle:shortVersionString":@"1.0.1",
            @"enclosure":@{@"url":@"https://example.test/update.dmg",@"length":@"2"},
            @"description":@{@"content":@"Only one locale chosen by Sparkle",@"format":@"plain-text"},
            @"oink:releaseNotes":json} mutableCopy];
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        SUAppcastItem *item=[[SUAppcastItem alloc] initWithDictionary:properties];
#pragma clang diagnostic pop
        assert(item);
        // Fake Sparkle callbacks and a deterministic monotonic clock exercise
        // the production logger, without an updater, download or real install.
        TestUpdater *timingUpdater=[TestUpdater new];
        timingRecords=[NSMutableArray new];
        owner=[OinkUpdateDriver new];owner.timing=[TestUpdateTiming new];owner.timing.callback=captureTiming;
        [owner showDownloadInitiatedWithCancellation:^{}];
        [owner.timing boundary:OinkDownloadStarted bytes:0];
        [owner showDownloadDidReceiveExpectedContentLength:100]; // Log received bytes, not the estimate.
        [owner showDownloadDidReceiveDataOfLength:12];
        [owner showDownloadDidReceiveDataOfLength:8];
        for (int repeat=0;repeat<2;repeat++) [owner updater:(SPUUpdater *)timingUpdater didDownloadUpdate:item];
        for (int repeat=0;repeat<2;repeat++) [owner updater:(SPUUpdater *)timingUpdater willExtractUpdate:item];
        for (int repeat=0;repeat<2;repeat++) [owner showDownloadDidStartExtractingUpdate];
        for (int repeat=0;repeat<2;repeat++) [owner updater:(SPUUpdater *)timingUpdater didExtractUpdate:item];
        [owner showExtractionReceivedProgress:0.5];
        [owner showExtractionReceivedProgress:1.0];
        [owner showExtractionReceivedProgress:1.0];
        [owner showExtractionReceivedProgress:1.1];
        __block BOOL installed=NO;
        for (int repeat=0;repeat<2;repeat++) [owner showReadyToInstallAndRelaunch:^(SPUUserUpdateChoice choice) {installed=choice==SPUUserUpdateChoiceInstall;}];
        for (int repeat=0;repeat<2;repeat++) oink_update_timing(OinkInstallClicked);
        for (int repeat=0;repeat<2;repeat++) oink_update_timing(OinkQuiesceDone);
        assert(oink_update_install() && installed);
        assert(!oink_update_install());
        for (int repeat=0;repeat<2;repeat++) [owner updater:(SPUUpdater *)timingUpdater willInstallUpdate:item];
        for (int repeat=0;repeat<2;repeat++) [owner updaterWillRelaunchApplication:(SPUUpdater *)timingUpdater];
        NSArray *boundaries=@[@1,@2,@9,@3,@10,@4,@5,@6,@7,@8,@11,@12];
        assert(timingRecords.count==boundaries.count);
        for (NSUInteger i=0;i<boundaries.count;i++) {
            NSArray<NSNumber *> *record=timingRecords[i];
            assert([record[0] isEqual:boundaries[i]]);
            assert(record[1].unsignedLongLongValue==(i+1)*10);
            assert(record[2].unsignedLongLongValue==i*10);
            assert(record[3].unsignedLongLongValue==(i==0?0:10));
            assert(record[4].unsignedLongLongValue==(i==1?20:0));
        }
        [owner showDownloadInitiatedWithCancellation:^{}]; // New attempt resets dedup and elapsed.
        assert(timingRecords.lastObject[2].unsignedLongLongValue==0);
        [owner.timing reset];
        [owner showReadyToInstallAndRelaunch:^(SPUUserUpdateChoice choice) {}];
        assert(timingRecords.lastObject[2].unsignedLongLongValue==UINT64_MAX); // Cached resume has no start.

        owner=[OinkUpdateDriver new];owner.callback=captureCandidate;
        [owner showUpdateFoundWithAppcastItem:item state:(SPUUserUpdateState *)[TestUpdateState new] reply:^(SPUUserUpdateChoice choice) {}];
        assert([[NSJSONSerialization JSONObjectWithData:[candidateNotes dataUsingEncoding:NSUTF8StringEncoding] options:0 error:NULL] isEqual:notes]);
        [properties removeObjectForKey:@"oink:releaseNotes"];
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        item=[[SUAppcastItem alloc] initWithDictionary:properties];
#pragma clang diagnostic pop
        [owner showUpdateFoundWithAppcastItem:item state:(SPUUserUpdateState *)[TestUpdateState new] reply:^(SPUUserUpdateChoice choice) {}];
        assert([candidateNotes isEqualToString:@""]); // Rust rejects missing notes metadata; no localized fallback.

        owner=[OinkUpdateDriver new];
        assert(oink_update_quit_allowed());
        assert(!oink_update_install());
        __block int reply=-1;
        [owner showReadyToInstallAndRelaunch:^(SPUUserUpdateChoice choice) {reply=(int)choice;}];
        assert(!oink_update_quit_allowed());
        oink_update_cancel();
        assert(reply==SPUUserUpdateChoiceSkip);
        assert(!oink_update_quit_allowed()); // Cancellation request is not ACK.
        assert(!oink_update_install());
        [owner dismissUpdateInstallation];
        assert(oink_update_quit_allowed());

        owner=[OinkUpdateDriver new];
        TestUpdater *updater=[TestUpdater new]; updater.sessionInProgress=YES;
        owner.updater=(SPUUpdater *)updater;
        __block BOOL acknowledged=NO;
        NSError *error=[NSError errorWithDomain:SUSparkleErrorDomain code:SUValidationError userInfo:nil];
        [owner showUpdaterError:error acknowledgement:^{acknowledged=YES;}];
        assert(acknowledged); // Error acknowledgement precedes asynchronous cleanup.
        oink_update_check(); oink_update_check();
        assert(updater.checks==0);
        [owner dismissUpdateInstallation];
        // Sparkle's scheduling probe clears readiness before session state,
        // without a didFinishUpdateCycle callback. Never reenter its KVO setter.
        updater.canCheckForUpdates=YES;
        assert(updater.checks==0);
        updater.sessionInProgress=NO;
        mainQueueTurn();
        assert(updater.checks==1); // One pending user intent survives the old driver's cleanup.

        owner=[OinkUpdateDriver new];
        updater=[TestUpdater new]; updater.sessionInProgress=YES;
        owner.updater=(SPUUpdater *)updater;
        oink_update_check();
        updater.sessionInProgress=NO; updater.canCheckForUpdates=YES;
        // A completed update cycle immediately starts the scheduling probe.
        // Its next readiness notification, rather than an artificial delay,
        // delivers the original Check after both callbacks have unwound.
        updater.canCheckForUpdates=NO; updater.sessionInProgress=YES;
        mainQueueTurn();
        assert(updater.checks==0 && owner.checkPending);
        updater.canCheckForUpdates=YES; updater.sessionInProgress=NO;
        mainQueueTurn();
        assert(updater.checks==1 && !owner.checkPending);

        owner=[OinkUpdateDriver new];
        updater=[TestUpdater new]; updater.sessionInProgress=YES;
        owner.updater=(SPUUpdater *)updater;
        oink_update_check(); oink_update_cancel();
        updater.canCheckForUpdates=YES; updater.sessionInProgress=NO;
        mainQueueTurn();
        assert(updater.checks==0); // Cancel withdraws the queued check too.

        owner=[OinkUpdateDriver new];
        updater=[TestUpdater new]; updater.sessionInProgress=YES;
        owner.updater=(SPUUpdater *)updater;
        oink_update_check();
        updater.canCheckForUpdates=YES; updater.sessionInProgress=NO;
        oink_update_cancel();
        mainQueueTurn();
        assert(updater.checks==0); // Cancellation also wins after notification.

        owner=[OinkUpdateDriver new];
        updater=[TestUpdater new]; updater.sessionInProgress=YES;
        owner.updater=(SPUUpdater *)updater;
        oink_update_check();
        NSError *notFound=[NSError errorWithDomain:SUSparkleErrorDomain code:SUNoUpdateError
            userInfo:@{SPUNoUpdateFoundReasonKey:@(SPUNoUpdateFoundReasonOnLatestVersion)}];
        acknowledged=NO;
        [owner showUpdateNotFoundWithError:notFound acknowledgement:^{acknowledged=YES;}];
        assert(acknowledged);
        updater.canCheckForUpdates=YES; updater.sessionInProgress=NO;
        mainQueueTurn();
        assert(updater.checks==0); // A result from the active check satisfies the request.
        oink_update_check();
        assert(updater.checks==1); // A later explicit check can immediately start anew.

        owner=[OinkUpdateDriver new]; reply=-1;
        [owner showDownloadDidStartExtractingUpdate];
        oink_update_preferences(false); // Checks off/deferral path.
        assert(!oink_update_quit_allowed());
        [owner showReadyToInstallAndRelaunch:^(SPUUserUpdateChoice choice) {reply=(int)choice;}];
        assert(reply==SPUUserUpdateChoiceSkip);
        assert(!oink_update_quit_allowed());
        [owner dismissUpdateInstallation];
        assert(oink_update_quit_allowed());

        owner=[OinkUpdateDriver new]; reply=-1;
        [owner showReadyToInstallAndRelaunch:^(SPUUserUpdateChoice choice) {reply=(int)choice;}];
        assert(oink_update_install());
        assert(reply==SPUUserUpdateChoiceInstall);
        assert(![owner respondsToSelector:@selector(updater:shouldPostponeRelaunchForUpdate:untilInvokingBlock:)]) ;
        assert(!oink_update_install()); // Continuation consumed exactly once.
        assert(!oink_update_quit_allowed());
        [owner dismissUpdateInstallation];
        assert(oink_update_quit_allowed());
    }
    return 0;
}
