#import <AppKit/AppKit.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdatomic.h>
#include <unistd.h>

extern void oink_audio_devices_wake(void);

@interface OinkAudioPower : NSObject {
@public
    _Atomic uint64_t epoch;
    _Atomic uint64_t acknowledged;
    _Atomic bool sleeping;
}
@property(strong) id sleepObserver;
@property(strong) id wakeObserver;
@end

@implementation OinkAudioPower
- (instancetype)init {
    self = [super init];
    if (!self) return nil;
    atomic_init(&epoch, 1);
    atomic_init(&acknowledged, 1);
    atomic_init(&sleeping, false);
    __weak OinkAudioPower *weakSelf = self;
    NSNotificationCenter *center = NSWorkspace.sharedWorkspace.notificationCenter;
    self.sleepObserver = [center addObserverForName:NSWorkspaceWillSleepNotification object:nil queue:nil
        usingBlock:^(NSNotification *notification) {
            (void)notification;
            OinkAudioPower *state = weakSelf;
            if (!state) return;
            atomic_store(&state->sleeping, true);
            uint64_t changed = atomic_fetch_add(&state->epoch, 1) + 1;
            oink_audio_devices_wake();
            // The actor drops capture/playback and acknowledges before sleep.
            // Bound the notification handler; never indefinitely delay the OS.
            for (unsigned i = 0; i < 250 && atomic_load(&state->acknowledged) < changed; ++i) usleep(1000);
        }];
    self.wakeObserver = [center addObserverForName:NSWorkspaceDidWakeNotification object:nil queue:nil
        usingBlock:^(NSNotification *notification) {
            (void)notification;
            OinkAudioPower *state = weakSelf;
            if (!state) return;
            atomic_store(&state->sleeping, false);
            atomic_fetch_add(&state->epoch, 1);
            oink_audio_devices_wake();
        }];
    if (!self.sleepObserver || !self.wakeObserver) return nil;
    return self;
}
- (void)dealloc {
    NSNotificationCenter *center = NSWorkspace.sharedWorkspace.notificationCenter;
    if (_sleepObserver) [center removeObserver:_sleepObserver];
    if (_wakeObserver) [center removeObserver:_wakeObserver];
}
@end

void *oink_audio_power_start(void) {
    return (__bridge_retained void *)[[OinkAudioPower alloc] init];
}
uint64_t oink_audio_power_state(void *raw, bool *asleep) {
    OinkAudioPower *state = (__bridge OinkAudioPower *)raw;
    uint64_t before, after;
    do {
        before = atomic_load(&state->epoch);
        *asleep = atomic_load(&state->sleeping);
        after = atomic_load(&state->epoch);
    } while (before != after);
    return after;
}
void oink_audio_power_ack(void *raw, uint64_t value) {
    OinkAudioPower *state = (__bridge OinkAudioPower *)raw;
    atomic_store(&state->acknowledged, value);
}
void oink_audio_power_stop(void *raw) {
    if (raw) { id state = (__bridge_transfer id)raw; (void)state; }
}
