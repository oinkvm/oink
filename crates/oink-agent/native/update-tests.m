// Isolated tests of the production Objective-C continuation owner. No updater,
// NSApplication, installed bundle, permissions, network, or real installer starts.
#import "update.m"
#include <assert.h>
static NSString *candidateNotes;
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
        // Public legacy initializer is sufficient for an isolated parsed-item
        // fixture; no updater or application state resolver starts here.
        NSDictionary *notes=@{@"zh-Hans":@"修复问题。",@"en":@"Fix a problem."};
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
        owner=[OinkUpdateDriver new];owner.callback=captureCandidate;
        [owner showUpdateFoundWithAppcastItem:item state:(SPUUserUpdateState *)[TestUpdateState new] reply:^(SPUUserUpdateChoice choice) {}];
        assert([[NSJSONSerialization JSONObjectWithData:[candidateNotes dataUsingEncoding:NSUTF8StringEncoding] options:0 error:NULL] isEqual:notes]);
        [properties removeObjectForKey:@"oink:releaseNotes"];
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        item=[[SUAppcastItem alloc] initWithDictionary:properties];
#pragma clang diagnostic pop
        [owner showUpdateFoundWithAppcastItem:item state:(SPUUserUpdateState *)[TestUpdateState new] reply:^(SPUUserUpdateChoice choice) {}];
        assert([candidateNotes isEqualToString:@""]); // Rust rejects missing bilingual metadata; no localized fallback.

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
        oink_update_preferences(false,false); // Notify/off/channel/deferral path.
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
