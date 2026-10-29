// Isolated, hidden native regression; no installed app or injected input.
// clang -fobjc-arc -framework AppKit -framework WebKit -framework Security
// local-drop-tests.m -o <external-evidence>/local-drop-tests
#import <WebKit/WebKit.h>
#import "local-update.m"
#define CHECK(test) do { if (!(test)) { fprintf(stderr,"FAIL line %d: %s\n",__LINE__,#test); return 1; } } while (0)
@interface DropTestWebView : WKWebView
@property NSUInteger enteredCalls;
@property NSUInteger updatedCalls;
@property NSUInteger performCalls;
@end
@implementation DropTestWebView
- (NSDragOperation)draggingEntered:(id<NSDraggingInfo>)sender {self.enteredCalls++;return NSDragOperationLink;}
- (NSDragOperation)draggingUpdated:(id<NSDraggingInfo>)sender {self.updatedCalls++;return NSDragOperationMove;}
- (BOOL)performDragOperation:(id<NSDraggingInfo>)sender {self.performCalls++;return NO;}
@end
@interface DropTestChildWebView : DropTestWebView @end
@implementation DropTestChildWebView
- (NSDragOperation)draggingEntered:(id<NSDraggingInfo>)sender {return [super draggingEntered:sender];}
- (NSDragOperation)draggingUpdated:(id<NSDraggingInfo>)sender {return [super draggingUpdated:sender];}
- (BOOL)performDragOperation:(id<NSDraggingInfo>)sender {return [super performDragOperation:sender];}
@end
@interface DropTestDrag : NSObject
@property(strong) NSPasteboard *draggingPasteboard;
@end
@implementation DropTestDrag
@end
@interface DropTestObserver : NSObject
@property NSUInteger changes;
@end
@implementation DropTestObserver
- (void)observeValueForKeyPath:(NSString *)keyPath ofObject:(id)object change:(NSDictionary *)change context:(void *)context {self.changes++;}
@end
static NSUInteger received;
static void receivePackage(const char *path) {if (strcmp(path,"/tmp/oink-test.dmg")==0) received++;}
int main(void) {@autoreleasepool{
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyProhibited];
    NSWindow *window=[[NSWindow alloc] initWithContentRect:NSMakeRect(0,0,1040,680) styleMask:NSWindowStyleMaskTitled|NSWindowStyleMaskFullSizeContentView backing:NSBackingStoreBuffered defer:NO];
    window.releasedWhenClosed=NO;
    WKWebViewConfiguration *config=[WKWebViewConfiguration new];
    DropTestWebView *web=[[DropTestWebView alloc] initWithFrame:window.contentView.bounds configuration:config];
    DropTestWebView *other=[[DropTestWebView alloc] initWithFrame:NSZeroRect configuration:config];
    [window.contentView addSubview:web];
    DropTestObserver *observer=[DropTestObserver new];
    [web addObserver:observer forKeyPath:@"frame" options:0 context:NULL];
    Class identity=web.class;
    Class runtimeIdentity=object_getClass(web);
    CHECK(identity==DropTestWebView.class);
    CHECK(!NSIsEmptyRect(web.safeAreaRect));
    [web registerForDraggedTypes:@[@"oink-test-original-type"]];
    oink_local_attach_drop((__bridge void *)web,receivePackage);
    oink_local_attach_drop((__bridge void *)web,receivePackage);
    CHECK(web.class==identity);
    CHECK(object_getClass(web)==runtimeIdentity);
    CHECK([web.registeredDraggedTypes containsObject:@"oink-test-original-type"]);
    CHECK([web.registeredDraggedTypes containsObject:NSPasteboardTypeFileURL]);
    for (int i=0;i<20;i++) {
        web.frame=NSMakeRect(0,0,1000+i,640+i);
        CHECK(!NSIsEmptyRect(web.safeAreaRect));
        CHECK(!NSIsEmptyRect(web.subviews.firstObject.safeAreaRect));
    }
    CHECK(observer.changes>0);
    [web removeObserver:observer forKeyPath:@"frame"];
    CHECK(web.class==identity);
    web.frame=NSMakeRect(0,0,1040,680);
    CHECK(!NSIsEmptyRect(web.safeAreaRect));
    DropTestDrag *drag=[DropTestDrag new];
    drag.draggingPasteboard=[NSPasteboard pasteboardWithUniqueName];
    [drag.draggingPasteboard writeObjects:@[[NSURL fileURLWithPath:@"/tmp/oink-test.dmg"]]];
    CHECK([web draggingEntered:(id<NSDraggingInfo>)drag]==NSDragOperationCopy);
    CHECK([web draggingUpdated:(id<NSDraggingInfo>)drag]==NSDragOperationCopy);
    CHECK([web performDragOperation:(id<NSDraggingInfo>)drag]);
    CHECK(received==1);
    CHECK(web.enteredCalls==0 && web.updatedCalls==0 && web.performCalls==0);
    CHECK([other draggingEntered:(id<NSDraggingInfo>)drag]==NSDragOperationLink);
    CHECK([other draggingUpdated:(id<NSDraggingInfo>)drag]==NSDragOperationMove);
    CHECK(![other performDragOperation:(id<NSDraggingInfo>)drag]);
    CHECK(other.enteredCalls==1 && other.updatedCalls==1 && other.performCalls==1);
    [drag.draggingPasteboard clearContents];
    [drag.draggingPasteboard writeObjects:@[[NSURL fileURLWithPath:@"/tmp/unrelated.txt"]]];
    CHECK([web draggingEntered:(id<NSDraggingInfo>)drag]==NSDragOperationLink);
    CHECK([web draggingUpdated:(id<NSDraggingInfo>)drag]==NSDragOperationMove);
    CHECK(![web performDragOperation:(id<NSDraggingInfo>)drag]);
    CHECK(web.enteredCalls==1 && web.updatedCalls==1 && web.performCalls==1);
    // A later window/subclass reuses the interception without wrapping a saved
    // interceptor again. Removing/re-adding a KVO observer must preserve it too.
    DropTestChildWebView *child=[[DropTestChildWebView alloc] initWithFrame:NSMakeRect(0,0,100,100) configuration:config];
    oink_local_attach_drop((__bridge void *)child,receivePackage);
    CHECK(child.class==DropTestChildWebView.class);
    CHECK([child draggingEntered:(id<NSDraggingInfo>)drag]==NSDragOperationLink);
    CHECK([child draggingUpdated:(id<NSDraggingInfo>)drag]==NSDragOperationMove);
    CHECK(![child performDragOperation:(id<NSDraggingInfo>)drag]);
    CHECK(child.enteredCalls==1 && child.updatedCalls==1 && child.performCalls==1);
    [web addObserver:observer forKeyPath:@"frame" options:0 context:NULL];
    web.frame=NSMakeRect(0,0,1000,640);
    CHECK(!NSIsEmptyRect(web.safeAreaRect));
    [web removeObserver:observer forKeyPath:@"frame"];
    [drag.draggingPasteboard clearContents];
    [drag.draggingPasteboard writeObjects:@[[NSURL fileURLWithPath:@"/tmp/oink-test.dmg"]]];
    CHECK([child draggingEntered:(id<NSDraggingInfo>)drag]==NSDragOperationCopy);
    CHECK([child draggingUpdated:(id<NSDraggingInfo>)drag]==NSDragOperationCopy);
    CHECK([child performDragOperation:(id<NSDraggingInfo>)drag]);
    CHECK(received==2);
    CHECK([web draggingEntered:(id<NSDraggingInfo>)drag]==NSDragOperationCopy);
    [drag.draggingPasteboard releaseGlobally];
    [window close];
    puts("PASS: KVO class identity/notifications/re-observation, 20 safe-area invalidations, package delivery, original drag forwarding, unbound sibling, inherited interception, idempotent attach");
    return 0;
}}
