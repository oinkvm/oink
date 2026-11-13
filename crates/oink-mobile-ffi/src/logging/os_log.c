// iOS system log output of the Rust tracing subscriber (`logging.rs`).
#include <CoreFoundation/CoreFoundation.h>
#include <os/log.h>
#include <pthread.h>
#include <stdint.h>

static char subsystem[256] = "com.hachiwii.oink";
static pthread_once_t subsystem_once = PTHREAD_ONCE_INIT;

// The subsystem is the bundle id of the process (the app or the extension),
// read once; the fallback only applies to a process without a bundle id.
static void read_subsystem(void) {
    CFBundleRef bundle = CFBundleGetMainBundle();
    CFStringRef identifier = bundle ? CFBundleGetIdentifier(bundle) : NULL;
    if (identifier) {
        CFStringGetCString(identifier, subsystem, sizeof subsystem, kCFStringEncodingUTF8);
    }
}

// Returns a log object that lives for the rest of the process.
void *oink_os_log_create(const char *category) {
    pthread_once(&subsystem_once, read_subsystem);
    return (void *)os_log_create(subsystem, category);
}

// The text was formatted by Rust; `public` keeps it readable in Console and
// `log stream` instead of `<private>`.
void oink_os_log_write(void *log, uint8_t type, const char *message) {
    os_log_with_type((os_log_t)log, (os_log_type_t)type, "%{public}s", message);
}
