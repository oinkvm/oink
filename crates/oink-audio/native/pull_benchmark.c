// Synthetic scheduling only: no device, workgroup, settings or audio I/O.
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <stdint.h>
#include <stdlib.h>
#include <dlfcn.h>
struct Bench { thread_port_t thread; thread_extended_policy_data_t previous; };
void *oink_pull_benchmark_enter(uint64_t period_ns, int32_t *error) {
    struct Bench *state = calloc(1, sizeof(*state));
    if (!state) { *error = -1; return NULL; }
    state->thread = mach_thread_self();
    mach_msg_type_number_t count = THREAD_EXTENDED_POLICY_COUNT; boolean_t def = FALSE;
    *error = thread_policy_get(state->thread, THREAD_EXTENDED_POLICY, (thread_policy_t)&state->previous, &count, &def);
    mach_timebase_info_data_t clock; mach_timebase_info(&clock);
    uint64_t ticks = period_ns * clock.denom / clock.numer;
    thread_time_constraint_policy_data_t policy = { .period = (uint32_t)ticks, .computation = (uint32_t)ticks / 2,
        .constraint = (uint32_t)ticks, .preemptible = TRUE };
    if (!*error) *error = thread_policy_set(state->thread, THREAD_TIME_CONSTRAINT_POLICY, (thread_policy_t)&policy, THREAD_TIME_CONSTRAINT_POLICY_COUNT);
    if (*error) { mach_port_deallocate(mach_task_self(), state->thread); free(state); return NULL; }
    return state;
}
void oink_pull_benchmark_leave(void *opaque) {
    struct Bench *state = opaque;
    thread_policy_set(state->thread, THREAD_EXTENDED_POLICY, (thread_policy_t)&state->previous, THREAD_EXTENDED_POLICY_COUNT);
    mach_port_deallocate(mach_task_self(), state->thread); free(state);
}
void *oink_pull_audit_symbol(const char *name) { return dlsym(RTLD_DEFAULT, name); }
