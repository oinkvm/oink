#import <Foundation/Foundation.h>
#import <objc/objc-exception.h>
#include <signal.h>
#include <unistd.h>
static void prior_abort(int signal) { (void)signal; (void)write(2, "fixture_previous_abort\n", 23); }
static unsigned observed;
static id prior(id exception) { observed++; return exception; }
void fixture_prior(void) { objc_setExceptionPreprocessor(prior); signal(SIGABRT, prior_abort); }
unsigned fixture_observed(void) { return observed; }
__attribute__((noinline)) void fixture_throw(void) {
    @throw [NSException exceptionWithName:@"OinkTestException" reason:@"fixture \"quoted\"\n原因" userInfo:nil];
}
void fixture_caught(void) {
    @try { fixture_throw(); } @catch (NSException *exception) {
        if (![exception.name isEqualToString:@"OinkTestException"]) abort();
    }
}
@interface OinkRecursiveException : NSException
@end
@implementation OinkRecursiveException
- (NSString *)reason { @throw [NSException exceptionWithName:@"Nested" reason:@"recursive getter" userInfo:nil]; }
@end
void fixture_recursive(void) {
    @try { @throw [[OinkRecursiveException alloc] initWithName:@"Recursive" reason:nil userInfo:nil]; }
    @catch (__unused id exception) {}
}
void fixture_bad_throw(void) {
    @throw [NSException exceptionWithName:@"/private/poison-name" reason:@"poison-reason /Users/private" userInfo:nil];
}
