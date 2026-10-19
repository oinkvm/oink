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
        assert(owner.authorized);
        assert(!oink_update_install()); // Continuation consumed exactly once.
        assert(!oink_update_quit_allowed());
        [owner dismissUpdateInstallation];
        assert(oink_update_quit_allowed());
    }
    return 0;
}
