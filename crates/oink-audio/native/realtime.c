// Auxiliary audio scheduling. Creation/disposal happen on an ordinary thread;
// enter/leave happen on the dedicated worker outside its steady-state loop.
#include <CoreAudio/CoreAudio.h>
#include <os/workgroup.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <stdlib.h>
#include <stdint.h>
#include <errno.h>

struct OinkRealtime {
    os_workgroup_t group;
    os_workgroup_join_token_s token;
    thread_extended_policy_data_t previous_extended;
    thread_port_t thread;
    uint32_t period;
    int joined;
};

void *oink_realtime_create(uint32_t device, uint64_t period_ns, int32_t *error) {
    *error = 0;
    if (!device || period_ns == 0 || period_ns > 1000000000) { *error = EINVAL; return NULL; }
    struct OinkRealtime *state = calloc(1, sizeof(*state));
    if (!state) { *error = ENOMEM; return NULL; }
    AudioObjectPropertyAddress address = {
        kAudioDevicePropertyIOThreadOSWorkgroup,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain
    };
    UInt32 size = sizeof(state->group);
    OSStatus status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &state->group);
    if (status != noErr || !state->group) {
        *error = status != noErr ? status : EINVAL;
        free(state);
        return NULL;
    }
    // HAL returns a retained reference to this property (AudioHardware.h).
    mach_timebase_info_data_t clock;
    mach_timebase_info(&clock);
    uint64_t ticks = period_ns * clock.denom / clock.numer;
    state->period = (uint32_t)(ticks > UINT32_MAX ? UINT32_MAX : ticks);
    return state;
}
int32_t oink_realtime_enter(void *opaque) {
    struct OinkRealtime *state = opaque;
    state->thread = mach_thread_self();
    mach_msg_type_number_t count = THREAD_EXTENDED_POLICY_COUNT;
    boolean_t is_default = FALSE;
    kern_return_t result = thread_policy_get(state->thread, THREAD_EXTENDED_POLICY,
        (thread_policy_t)&state->previous_extended, &count, &is_default);
    if (result != KERN_SUCCESS) return result;
    if (!state->previous_extended.timeshare) return EINVAL;
    thread_time_constraint_policy_data_t policy = {
        .period = state->period,
        .computation = state->period / 2,
        .constraint = state->period,
        .preemptible = TRUE
    };
    result = thread_policy_set(state->thread, THREAD_TIME_CONSTRAINT_POLICY,
        (thread_policy_t)&policy, THREAD_TIME_CONSTRAINT_POLICY_COUNT);
    if (result != KERN_SUCCESS) return result;
    int joined = os_workgroup_join(state->group, &state->token);
    if (joined != 0) {
        thread_policy_set(state->thread, THREAD_EXTENDED_POLICY,
            (thread_policy_t)&state->previous_extended, THREAD_EXTENDED_POLICY_COUNT);
        return joined;
    }
    state->joined = 1;
    return 0;
}
int32_t oink_realtime_leave(void *opaque) {
    struct OinkRealtime *state = opaque;
    if (state->joined) {
        os_workgroup_leave(state->group, &state->token);
        state->joined = 0;
    }
    // Workers are newly spawned timeshare threads. Restore their original
    // policy before any ordinary-thread cleanup or error formatting occurs.
    kern_return_t result = thread_policy_set(state->thread, THREAD_EXTENDED_POLICY,
        (thread_policy_t)&state->previous_extended, THREAD_EXTENDED_POLICY_COUNT);
    mach_port_deallocate(mach_task_self(), state->thread);
    state->thread = MACH_PORT_NULL;
    return result;
}
void oink_realtime_destroy(void *opaque) {
    struct OinkRealtime *state = opaque;
    if (!state) return;
    if (state->thread != MACH_PORT_NULL) mach_port_deallocate(mach_task_self(), state->thread);
    os_release(state->group);
    free(state);
}

uint32_t oink_audio_wake_create(int32_t *error) {
    semaphore_t semaphore = MACH_PORT_NULL;
    *error = semaphore_create(mach_task_self(), &semaphore, SYNC_POLICY_FIFO, 0);
    return semaphore;
}
int32_t oink_audio_wake_signal(uint32_t semaphore) { return semaphore_signal(semaphore); }
int32_t oink_audio_wake_wait(uint32_t semaphore) { return semaphore_wait(semaphore); }
void oink_audio_wake_destroy(uint32_t semaphore) { semaphore_destroy(mach_task_self(), semaphore); }
