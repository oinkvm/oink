// No updater/feed settings change here. All slices and sealed nested code must
// meet Apple's Developer ID requirement; notarization is a separate requirement.
#import <Foundation/Foundation.h>
#import <Security/Security.h>
static NSString *const identityRequirement=@"anchor apple generic and identifier \"com.hachiwii.oink\" and certificate leaf[subject.OU] = \"893V37X2CW\" and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";
static uint32_t verifyIdentity(const char *path, NSString *identity) {
    @autoreleasepool {
        NSURL *url=[NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        SecStaticCodeRef code=NULL; SecRequirementRef requirement=NULL;
        CFDictionaryRef info=NULL;
        uint32_t result=1;
        NSNumber *flags=nil;
        if (SecStaticCodeCreateWithPath((__bridge CFURLRef)url,kSecCSDefaultFlags,&code)!=errSecSuccess) goto done;
        if (SecRequirementCreateWithString((__bridge CFStringRef)identity,kSecCSDefaultFlags,&requirement)!=errSecSuccess) goto done;
        if (SecStaticCodeCheckValidity(code,kSecCSCheckAllArchitectures|kSecCSCheckNestedCode|kSecCSStrictValidate,requirement)!=errSecSuccess) goto done;
        CFRelease(requirement); requirement=NULL;
        if (SecCodeCopySigningInformation(code,kSecCSSigningInformation,&info)!=errSecSuccess) goto done;
        flags=(__bridge NSNumber *)CFDictionaryGetValue(info,kSecCodeInfoFlags);
        if (!(flags.unsignedIntValue & kSecCodeSignatureRuntime)) goto done;
        result=2;
        if (SecRequirementCreateWithString(CFSTR("notarized"),kSecCSDefaultFlags,&requirement)!=errSecSuccess) goto done;
        if (SecStaticCodeCheckValidity(code,kSecCSCheckAllArchitectures|kSecCSStrictValidate,requirement)!=errSecSuccess) goto done;
        result=0;
    done:
        if (info) CFRelease(info);
        if (requirement) CFRelease(requirement);
        if (code) CFRelease(code);
        return result;
    }
}

uint32_t oink_local_verify_app(const char *path) {return verifyIdentity(path,identityRequirement);}

#import <AppKit/AppKit.h>
#import <objc/runtime.h>
#import <objc/message.h>
typedef void (*LocalDrop)(const char *);
typedef void (*LocalHover)(uint8_t);
@interface OinkLocalDrop : NSObject
@property LocalDrop callback;
@property LocalHover hover;
@property BOOL hovered;
@property(strong) NSOperationQueue *queue;
@end
@implementation OinkLocalDrop
@end
static char dropOwnerKey,dropMethodsKey;
static BOOL packageName(NSString *name) {return [name.pathExtension.lowercaseString isEqualToString:@"dmg"];}
static NSArray *packageObjects(id<NSDraggingInfo> drag) {
    NSPasteboard *board=drag.draggingPasteboard;
    NSArray *urls=[board readObjectsForClasses:@[NSURL.class] options:@{NSPasteboardURLReadingFileURLsOnlyKey:@YES}];
    if (urls.count==1) {
        NSURL *url=urls.firstObject;
        if (packageName((url.filePathURL ?: url).path)) return urls;
    }
    NSArray *promises=[board readObjectsForClasses:@[NSFilePromiseReceiver.class] options:@{}];
    if (promises.count==1) {
        NSFilePromiseReceiver *promise=promises.firstObject;
        if (promise.fileNames.count==1 && packageName(promise.fileNames.firstObject)) return promises;
    }
    return nil;
}
static void updateHover(id self,BOOL value) {
    OinkLocalDrop *owner=objc_getAssociatedObject(self,&dropOwnerKey);
    if (owner.hovered==value) return;
    owner.hovered=value;if(owner.hover)owner.hover(value?1:0);
}
static void dropExited(id self,SEL selector,id<NSDraggingInfo> drag,IMP original) {
    updateHover(self,NO);
    if (original) ((void (*)(id,SEL,id))original)(self,selector,drag);
}
static NSDragOperation dropEntered(id self,SEL selector,id<NSDraggingInfo> drag,IMP original) {
    OinkLocalDrop *owner=objc_getAssociatedObject(self,&dropOwnerKey);
    BOOL valid=owner.callback && packageObjects(drag)!=nil;updateHover(self,valid);
    if (valid) return NSDragOperationCopy;
    return original ? ((NSDragOperation (*)(id,SEL,id))original)(self,selector,drag) : NSDragOperationNone;
}
static BOOL dropPerform(id self,SEL selector,id<NSDraggingInfo> drag,IMP original) {
    updateHover(self,NO);
    OinkLocalDrop *owner=objc_getAssociatedObject(self,&dropOwnerKey);
    NSArray *objects=owner.callback ? packageObjects(drag) : nil;
    if (!objects || !owner.callback) {
        return original ? ((BOOL (*)(id,SEL,id))original)(self,selector,drag) : NO;
    }
    id object=objects.firstObject;
    if ([object isKindOfClass:NSURL.class]) {
        NSURL *url=[object filePathURL] ?: object;
        owner.callback(url.path.UTF8String);return YES;
    }
    NSString *directory=[NSTemporaryDirectory() stringByAppendingPathComponent:[NSString stringWithFormat:@"oink-drop-%@.noindex",NSUUID.UUID.UUIDString]];
    if (![NSFileManager.defaultManager createDirectoryAtPath:directory withIntermediateDirectories:NO attributes:@{NSFilePosixPermissions:@0700} error:NULL]) return NO;
    // Oink's native cross-device drag can offer a File Provider URL or a file
    // promise. Wry 0.57 reads only NSFilenamesPboardType, losing modern URLs and
    // promises. The attached instance adds those two AppKit forms while
    // forwarding all other drags to the original Wry implementation.
    [(NSFilePromiseReceiver *)object receivePromisedFilesAtDestination:[NSURL fileURLWithPath:directory] options:@{} operationQueue:owner.queue reader:^(NSURL *url,NSError *error){
        if (!error && packageName(url.path)) owner.callback(url.path.UTF8String);
    }];
    return YES;
}
void oink_local_attach_drop(void *raw,LocalDrop callback,LocalHover hover) {
    NSView *view=(__bridge NSView *)raw;
    if (objc_getAssociatedObject(view,&dropOwnerKey)) return;
    OinkLocalDrop *owner=[OinkLocalDrop new];owner.callback=callback;owner.hover=hover;
    owner.queue=[NSOperationQueue new];owner.queue.maxConcurrentOperationCount=1;
    // AppKit may already have installed a KVO runtime subclass. Adding another
    // subclass with object_setClass makes KVO's -class return Nil on macOS
    // 26.6.2; safe-area recomputation then aborts at NSDP's "imp != NULL"
    // assertion (including Siri's UIIntelligenceSupport view collection).
    // Intercept only these drag methods on the reported class, leaving isa and
    // KVO bookkeeping intact. Unattached views and unrelated drags call the
    // saved original IMPs; super dispatch would re-enter these interceptors
    // through the KVO subclass. Disabling Writing Tools did not fix the crash.
    // Hidden WKWebView regression verified on macOS 26.6.2 (25G83); no private
    // AppKit selectors or system-wide intelligence settings are changed.
    Class base=view.class;
    if (!objc_getAssociatedObject(base,&dropMethodsKey)) {
        Method entered=class_getInstanceMethod(base,@selector(draggingEntered:));
        Method updated=class_getInstanceMethod(base,@selector(draggingUpdated:));
        Method exited=class_getInstanceMethod(base,@selector(draggingExited:));
        Method perform=class_getInstanceMethod(base,@selector(performDragOperation:));
        IMP originalEntered=entered ? method_getImplementation(entered) : NULL;
        IMP originalUpdated=updated ? method_getImplementation(updated) : NULL;
        IMP originalExited=exited ? method_getImplementation(exited) : NULL;
        IMP originalPerform=perform ? method_getImplementation(perform) : NULL;
        // Capture the implementation for this exact class, so an inherited
        // interceptor or a subclass calling super keeps its own forwarding path.
        IMP enteredAdapter=imp_implementationWithBlock(^NSDragOperation(id receiver,id<NSDraggingInfo> drag) {
            return dropEntered(receiver,@selector(draggingEntered:),drag,originalEntered);
        });
        IMP updatedAdapter=imp_implementationWithBlock(^NSDragOperation(id receiver,id<NSDraggingInfo> drag) {
            return dropEntered(receiver,@selector(draggingUpdated:),drag,originalUpdated);
        });
        IMP exitedAdapter=imp_implementationWithBlock(^void(id receiver,id<NSDraggingInfo> drag) {
            dropExited(receiver,@selector(draggingExited:),drag,originalExited);
        });
        IMP performAdapter=imp_implementationWithBlock(^BOOL(id receiver,id<NSDraggingInfo> drag) {
            return dropPerform(receiver,@selector(performDragOperation:),drag,originalPerform);
        });
        class_replaceMethod(base,@selector(draggingEntered:),enteredAdapter,entered ? method_getTypeEncoding(entered) : "Q@:@");
        class_replaceMethod(base,@selector(draggingUpdated:),updatedAdapter,updated ? method_getTypeEncoding(updated) : "Q@:@");
        class_replaceMethod(base,@selector(draggingExited:),exitedAdapter,exited ? method_getTypeEncoding(exited) : "v@:@");
        class_replaceMethod(base,@selector(performDragOperation:),performAdapter,perform ? method_getTypeEncoding(perform) : "B@:@");
        objc_setAssociatedObject(base,&dropMethodsKey,@YES,OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    }
    objc_setAssociatedObject(view,&dropOwnerKey,owner,OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    // Preserve Wry's legacy filenames and unrelated registered representations.
    NSMutableArray *types=[NSMutableArray arrayWithArray:view.registeredDraggedTypes ?: @[]];
    [types addObjectsFromArray:NSFilePromiseReceiver.readableDraggedTypes];
    [types addObject:NSPasteboardTypeFileURL];
    [view registerForDraggedTypes:types];
}
