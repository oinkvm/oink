// Read-only genuine-signature checks plus disposable copied bundles. Never launch
// the supplied reference app. clang -fobjc-arc -framework AppKit -framework
// Security local-update-tests.m -o <external-evidence>/local-update-tests
#import "local-update.m"
@interface FakePackageDrag : NSObject
@property(strong) NSPasteboard *draggingPasteboard;
@end
@implementation FakePackageDrag
@end
// Headless KVO fixture: no NSApplication, windows or WebKit are created.
@interface ProbeDropView:NSView
@property BOOL observed;
@end
static unsigned forwardedEnter,forwardedExit,observations;
@implementation ProbeDropView
-(NSDragOperation)draggingEntered:(id<NSDraggingInfo>)drag {forwardedEnter++;return NSDragOperationLink;}
-(NSDragOperation)draggingUpdated:(id<NSDraggingInfo>)drag {return NSDragOperationLink;}
-(void)draggingExited:(id<NSDraggingInfo>)drag {forwardedExit++;}
-(BOOL)performDragOperation:(id<NSDraggingInfo>)drag {return NO;}
@end
@interface DropObserver:NSObject
@end
@implementation DropObserver
-(void)observeValueForKeyPath:(NSString*)key ofObject:(id)object change:(NSDictionary*)change context:(void*)context {observations++;}
@end
static void discardDrop(const char *path) {(void)path;}
static uint8_t hovering=0;
static void recordHover(uint8_t active) {hovering=active;}
int main(int argc,const char **argv) {
    @autoreleasepool {
        if (argc!=2) return 2;
        NSView *view=[NSView new];[view registerForDraggedTypes:@[@"oink-test-original-type"]];
        Class reported=view.class, runtime=object_getClass(view);
        oink_local_attach_drop((__bridge void *)view,discardDrop,recordHover);
        if(view.class!=reported || object_getClass(view)!=runtime)return 13;
        if (![view.registeredDraggedTypes containsObject:@"oink-test-original-type"]) return 9;
        FakePackageDrag *drag=[FakePackageDrag new];drag.draggingPasteboard=[NSPasteboard pasteboardWithUniqueName];
        [drag.draggingPasteboard writeObjects:@[[NSURL fileURLWithPath:@"/tmp/received/Oink.dmg"]]];
        if (packageObjects((id<NSDraggingInfo>)drag).count!=1) return 7;
        if ([(id<NSDraggingDestination>)view draggingEntered:(id<NSDraggingInfo>)drag]!=NSDragOperationCopy || !hovering) return 10;
        if (![(id<NSDraggingDestination>)view performDragOperation:(id<NSDraggingInfo>)drag] || hovering) return 11;
        [drag.draggingPasteboard clearContents];
        [drag.draggingPasteboard writeObjects:@[[NSURL fileURLWithPath:@"/tmp/unrelated.txt"]]];
        if (packageObjects((id<NSDraggingInfo>)drag)) return 8;
        [drag.draggingPasteboard clearContents];
        [drag.draggingPasteboard writeObjects:@[[NSURL fileURLWithPath:@"/tmp/other-platform.msi"]]];
        if(packageObjects((id<NSDraggingInfo>)drag))return 12;

        ProbeDropView *watched=[ProbeDropView new];DropObserver *observer=[DropObserver new];
        [watched addObserver:observer forKeyPath:@"observed" options:0 context:NULL];
        Class watchedReported=watched.class,watchedRuntime=object_getClass(watched);
        if(watchedRuntime==watchedReported)return 19; // Require actual KVO observation.
        oink_local_attach_drop((__bridge void*)watched,discardDrop,recordHover);
        if(watched.class!=watchedReported || object_getClass(watched)!=watchedRuntime)return 14;
        watched.observed=YES;if(observations!=1)return 15;
        ProbeDropView *unattached=[ProbeDropView new];
        if([unattached draggingEntered:(id<NSDraggingInfo>)drag]!=NSDragOperationLink || forwardedEnter!=1)return 16;
        [drag.draggingPasteboard clearContents];
        [drag.draggingPasteboard writeObjects:@[[NSURL fileURLWithPath:@"/tmp/received/Oink.dmg"]]];
        if([watched draggingEntered:(id<NSDraggingInfo>)drag]!=NSDragOperationCopy || !hovering)return 17;
        if([watched draggingUpdated:(id<NSDraggingInfo>)drag]!=NSDragOperationCopy || !hovering)return 20;
        [watched draggingExited:(id<NSDraggingInfo>)drag];
        if(hovering || forwardedExit!=1)return 18;
        [watched removeObserver:observer forKeyPath:@"observed"];
        [drag.draggingPasteboard releaseGlobally];
        const char *app=argv[1];
        if (oink_local_verify_app(app)!=0) {puts("genuine reference rejected");return 1;}
        NSString *wrongTeam=[identityRequirement stringByReplacingOccurrencesOfString:@"893V37X2CW" withString:@"0000000000"];
        if (verifyIdentity(app,wrongTeam)!=1) return 3;
        NSString *wrongProduct=[identityRequirement stringByReplacingOccurrencesOfString:@"com.hachiwii.oink" withString:@"com.example.other"];
        if (verifyIdentity(app,wrongProduct)!=1) return 4;
        NSString *root=[NSTemporaryDirectory() stringByAppendingPathComponent:[NSString stringWithFormat:@"oink-verifier-%@.noindex",NSUUID.UUID.UUIDString]];
        [NSFileManager.defaultManager createDirectoryAtPath:root withIntermediateDirectories:NO attributes:nil error:NULL];
        NSString *copy=[root stringByAppendingPathComponent:@"Oink.app"];
        NSTask *ditto=[NSTask new];ditto.executableURL=[NSURL fileURLWithPath:@"/usr/bin/ditto"];
        ditto.arguments=@[[NSString stringWithUTF8String:app],copy];[ditto launchAndReturnError:NULL];[ditto waitUntilExit];
        NSString *info=[copy stringByAppendingPathComponent:@"Contents/Info.plist"];
        NSFileHandle *handle=[NSFileHandle fileHandleForWritingAtPath:info];[handle seekToEndOfFile];[handle writeData:[@"tampered" dataUsingEncoding:NSUTF8StringEncoding]];[handle closeFile];
        BOOL rejected=oink_local_verify_app(copy.UTF8String)!=0;
        [NSFileManager.defaultManager removeItemAtPath:root error:NULL];
        if (!rejected) return 5;
        if (oink_local_verify_app("/System/Applications/Calculator.app")==0) return 6;
        puts("PASS: KVO identity/observation; original IMP forwarding; URL hover/exit/drop; genuine signature/notarization/runtime; wrong team/product/signer; tampered seal");
        return 0;
    }
}
