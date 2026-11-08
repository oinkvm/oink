// Detection only: Rust aborts in __rust_foreign_exception before an uncaught
// exception handler can run. Observe @throw before unwinding, including handled
// throws; never turn an observation into a fatal report. Available since 10.5.
#import <Foundation/Foundation.h>
#import <objc/objc-exception.h>
#include <dlfcn.h>
#include <errno.h>
#include <execinfo.h>
#include <mach-o/loader.h>
#include <pthread.h>
#include <stdbool.h>
#include <signal.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define RECORD_SIZE 16384
#define RECORD_COUNT 8
static pthread_mutex_t gate = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static struct sigaction previous_abort;
static atomic_flag report_gate = ATOMIC_FLAG_INIT;
extern unsigned int __cxa_uncaught_exceptions(void);
static _Atomic(objc_exception_preprocessor) previous;
static int output = -1, report_output = -1;
static uint64_t report_epoch, local_count, report_count;
static __thread uint64_t last_report_epoch, last_report_count;
static uint64_t sequence;
static __thread bool observing;

// Escape bounded UTF-8 bytes without invoking ObjC formatting/description.
static size_t json_string(char *out, const char *in) {
    static const char hex[] = "0123456789abcdef";
    size_t at = 0;
    out[at++] = '"';
    for (const unsigned char *p = (const unsigned char *)in; *p; ++p) {
        if (*p == '"' || *p == '\\') { out[at++] = '\\'; out[at++] = *p; }
        else if (*p < 32) {
            memcpy(out + at, "\\u00", 4); at += 4;
            out[at++] = hex[*p >> 4]; out[at++] = hex[*p & 15];
        } else out[at++] = *p;
    }
    out[at++] = '"';
    return at;
}
static void text(NSString *value, char *out, size_t size) {
    NSUInteger used = 0;
    [value getBytes:out maxLength:size - 1 usedLength:&used encoding:NSUTF8StringEncoding
        options:0 range:NSMakeRange(0, MIN(value.length, size - 1)) remainingRange:NULL];
    out[used] = 0;
}
static void image_uuid(const struct mach_header_64 *header, char out[33]) {
    static const char hex[] = "0123456789abcdef";
    out[0] = 0;
    if (!header || header->magic != MH_MAGIC_64 || header->sizeofcmds > 1024 * 1024) return;
    const char *p = (const char *)(header + 1), *end = p + header->sizeofcmds;
    for (uint32_t i = 0; i < header->ncmds && p + sizeof(struct load_command) <= end; ++i) {
        const struct load_command *command = (const void *)p;
        if (command->cmdsize < sizeof(*command) || command->cmdsize > (size_t)(end - p)) break;
        if (command->cmd == LC_UUID && command->cmdsize >= sizeof(struct uuid_command)) {
            const struct uuid_command *uuid = (const void *)p;
            for (int j = 0; j < 16; ++j) {
                out[j * 2] = hex[uuid->uuid[j] >> 4]; out[j * 2 + 1] = hex[uuid->uuid[j] & 15];
            }
            out[32] = 0; return;
        }
        p += command->cmdsize;
    }
}
static id observe(id exception) {
    int saved_errno = errno;
    if (!observing && pthread_mutex_trylock(&gate) == 0) {
        if (output >= 0 || report_output >= 0) {
            observing = true;
            char name[129] = "<non-NSException>", reason[1025] = "";
            // A hostile/custom subclass may throw while reading name/reason.
            // The TLS guard prevents recursion; leave its exception locally caught.
            bool complete_name = false;
            @try {
                if ([exception isKindOfClass:NSException.class]) {
                    NSString *full_name = [exception name];
                    complete_name = full_name.length <= 128;
                    text(full_name, name, sizeof(name));
                    if (output >= 0) text([exception reason], reason, sizeof(reason));
                }
            } @catch (__unused id ignored) {}
            void *pcs[64];
            int count = backtrace(pcs, 64);
            uint64_t thread = 0;
            pthread_threadid_np(NULL, &thread);
            char record[RECORD_SIZE];
            memset(record, ' ', sizeof(record));
            size_t at = (size_t)snprintf(record, sizeof(record),
                "{\"fatal\":0,\"schema\":1,\"sequence\":%llu,\"pid\":%d,\"thread\":%llu,\"unix_seconds\":%lld,\"name\":",
                (unsigned long long)++sequence, getpid(), (unsigned long long)thread, (long long)time(NULL));
            at += json_string(record + at, name);
            memcpy(record + at, ",\"reason\":", 10); at += 10;
            at += json_string(record + at, reason);
            memcpy(record + at, ",\"frames\":[", 11); at += 11;
            for (int i = 0; i < count; ++i) {
                // Name/reason escaping uses at most 6 * (128 + 1024)
                // bytes. Each numeric frame fits in 160, including its NUL.
                if (at + 160 >= sizeof(record)) break;
                Dl_info info = {0};
                char uuid[33] = "";
                bool found = dladdr(pcs[i], &info) != 0;
                if (found) image_uuid(info.dli_fbase, uuid);
                // Raw return PCs: retain UUID + image-relative address for offline
                // symbolication using the matching release dSYM, even after ASLR.
                at += (size_t)snprintf(record + at, sizeof(record) - at,
                    "%s{\"uuid\":\"%s\",\"offset\":%llu,\"pc\":%llu}", i ? "," : "", uuid,
                    (unsigned long long)(found ? (uintptr_t)pcs[i] - (uintptr_t)info.dli_fbase : 0),
                    (unsigned long long)(uintptr_t)pcs[i]);
            }
            memcpy(record + at, "]}", 2); at += 2;
            // snprintf's trailing NUL must not enter the padded NDJSON record.
            memset(record + at, ' ', sizeof(record) - at);
            record[sizeof(record) - 1] = '\n';
            if (output >= 0) {
                ++local_count;
                (void)pwrite(output, record, sizeof(record), (off_t)(((local_count - 1) % RECORD_COUNT) * RECORD_SIZE));
            }
            if (report_output >= 0) {
                // The upload-eligible snapshot NEVER stores free-text reason or
                // arbitrary type names. Only ASCII identifiers survive here.
                bool identifier = complete_name && name[0] != 0 && strlen(name) <= 128;
                for (const unsigned char *p = (const unsigned char *)name; *p; ++p)
                    if (!((*p >= 'a' && *p <= 'z') || (*p >= 'A' && *p <= 'Z') ||
                          (*p >= '0' && *p <= '9') || *p == '_')) identifier = false;
                // Build a second record from the numeric frames already captured.
                size_t safe_at = (size_t)snprintf(record, sizeof(record),
                    "{\"fatal\":0,\"schema\":1,\"sequence\":%llu,\"pid\":%d,\"thread\":%llu,\"unix_seconds\":%lld,\"name\":",
                    (unsigned long long)sequence, getpid(), (unsigned long long)thread, (long long)time(NULL));
                safe_at += json_string(record + safe_at, identifier ? name : "other");
                memcpy(record + safe_at, ",\"frames\":[", 11); safe_at += 11;
                for (int i = 0; i < count; ++i) {
                    if (safe_at + 160 >= sizeof(record)) break;
                    Dl_info info = {0}; char uuid[33] = "";
                    bool found = dladdr(pcs[i], &info) != 0;
                    if (found) image_uuid(info.dli_fbase, uuid);
                    safe_at += (size_t)snprintf(record + safe_at, sizeof(record) - safe_at,
                        "%s{\"uuid\":\"%s\",\"offset\":%llu}", i ? "," : "", uuid,
                        (unsigned long long)(found ? (uintptr_t)pcs[i] - (uintptr_t)info.dli_fbase : 0));
                }
                memcpy(record + safe_at, "]}", 2); safe_at += 2;
                memset(record + safe_at, ' ', sizeof(record) - safe_at);
                record[sizeof(record) - 1] = '\n';
                while (atomic_flag_test_and_set_explicit(&report_gate, memory_order_acquire)) sched_yield();
                last_report_epoch = report_epoch;
                last_report_count = ++report_count;
                (void)pwrite(report_output, record, sizeof(record), (off_t)(((report_count - 1) % RECORD_COUNT) * RECORD_SIZE));
                atomic_flag_clear_explicit(&report_gate, memory_order_release);
            }
            observing = false;
        }
        pthread_mutex_unlock(&gate);
    }
    errno = saved_errno;
    // Preserve other runtime/crash tooling preprocessors and the thrown object.
    objc_exception_preprocessor prior = atomic_load_explicit(&previous, memory_order_acquire);
    return prior ? prior(exception) : exception;
}
// abort while an ObjC/C++ exception is still escaping Rust must be observed
// before Apple's post-unwind .ips is produced. A normally caught exception has
// zero uncaught exceptions, so even a subsequent unrelated abort cannot arm it.
// libc++abi's getter only reads already-initialized thread-local EH state.
static void abort_signal(int signal, siginfo_t *info, void *context) {
    int saved_errno = errno;
    if (!observing && last_report_count && __cxa_uncaught_exceptions() > 0 &&
        !atomic_flag_test_and_set_explicit(&report_gate, memory_order_acquire)) {
        if (report_output >= 0 && last_report_epoch == report_epoch && last_report_count &&
            report_count - last_report_count < RECORD_COUNT) {
            const char fatal = '1';
            (void)pwrite(report_output, &fatal, 1,
                (off_t)(((last_report_count - 1) % RECORD_COUNT) * RECORD_SIZE + 9));
        }
        atomic_flag_clear_explicit(&report_gate, memory_order_release);
    }
    errno = saved_errno;
    if (previous_abort.sa_handler != SIG_DFL && previous_abort.sa_handler != SIG_IGN) {
        if (previous_abort.sa_flags & SA_SIGINFO) previous_abort.sa_sigaction(signal, info, context);
        else previous_abort.sa_handler(signal);
    }
    // Returning preserves libc abort's reset-and-reraise and OS crash capture.
}

static void install(void) {
    struct sigaction action = {0};
    action.sa_sigaction = abort_signal;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    sigaction(SIGABRT, &action, &previous_abort);
    atomic_store_explicit(&previous, objc_setExceptionPreprocessor(observe), memory_order_release);
}

// Takes ownership of fd; -1 disables recording. Configuration waits for a live
// record before closing its descriptor; the throw hook never waits on this lock.
void oink_objc_exception_output(int fd) {
    pthread_mutex_lock(&gate);
    if (output >= 0) close(output);
    output = fd;
    local_count = 0;
    pthread_once(&once, install);
    pthread_mutex_unlock(&gate);
}

void oink_objc_exception_report_output(int fd) {
    pthread_mutex_lock(&gate);
    while (atomic_flag_test_and_set_explicit(&report_gate, memory_order_acquire)) sched_yield();
    if (report_output >= 0) close(report_output);
    report_output = fd;
    report_count = 0; ++report_epoch;
    atomic_flag_clear_explicit(&report_gate, memory_order_release);
    pthread_once(&once, install);
    pthread_mutex_unlock(&gate);
}
