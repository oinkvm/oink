// No browser is created until the user clicks Grant. Network.framework keeps
// waiting across the Local Network alert and observes later Settings changes.
#import <Foundation/Foundation.h>
#import <Network/Network.h>
extern void oink_observe_local_network(int state); // completed operation: 1 granted, 2 denied
static nw_browser_t browser;
static dispatch_queue_t queue;
static uint64_t generation;
void oink_request_local_network(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{ queue = dispatch_queue_create("app.oink.permission.network", DISPATCH_QUEUE_SERIAL); });
    dispatch_async(queue, ^{
        const uint64_t requested = ++generation;
        if (browser) nw_browser_cancel(browser);
        nw_browse_descriptor_t descriptor = nw_browse_descriptor_create_bonjour_service("_oink._udp", "local.");
        browser = nw_browser_create(descriptor, nw_parameters_create_secure_udp(NW_PARAMETERS_DISABLE_PROTOCOL, NW_PARAMETERS_DEFAULT_CONFIGURATION));
        nw_browser_set_queue(browser, queue);
        nw_browser_set_state_changed_handler(browser, ^(nw_browser_state_t state, nw_error_t error) {
            (void)state;
            if (requested != generation) return;
            if (error && nw_error_get_error_domain(error) == nw_error_domain_dns &&
                (nw_error_get_error_code(error) == -65570 || nw_error_get_error_code(error) == -65555))
                oink_observe_local_network(2); // PolicyDenied / NoAuth.
        });
        // Ready means the browser started, not that a protected query succeeded.
        nw_browser_set_browse_results_changed_handler(browser, ^(nw_browse_result_t old, nw_browse_result_t result, bool complete) {
            (void)old; (void)complete;
            if (requested == generation && result) oink_observe_local_network(1);
        });
        nw_browser_start(browser);
    });
}
