#import <AppKit/AppKit.h>
#import <AVFoundation/AVFoundation.h>
#include <stdbool.h>
#include <stdint.h>

bool oink_microphone_request_permission(void) {
    NSString *purpose = [[NSBundle mainBundle] objectForInfoDictionaryKey:@"NSMicrophoneUsageDescription"];
    if (![purpose isKindOfClass:NSString.class] || purpose.length == 0) return false;
    dispatch_async(dispatch_get_main_queue(), ^{
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
            (void)granted; // The API queries authorizationStatus; no audio is opened.
        }];
    });
    return true;
}
