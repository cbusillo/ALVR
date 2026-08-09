#import <AppKit/AppKit.h>
#import <Network/Network.h>
#import <dns_sd.h>

enum alvr_local_network_consent_outcome {
    ALVR_LOCAL_NETWORK_READY = 0,
    ALVR_LOCAL_NETWORK_POLICY_DENIED = 1,
    ALVR_LOCAL_NETWORK_WAITING = 2,
    ALVR_LOCAL_NETWORK_FAILED = 3,
    ALVR_LOCAL_NETWORK_CANCELLED = 4,
    ALVR_LOCAL_NETWORK_SETUP_FAILED = 5,
};

static NSString *error_description(nw_error_t error)
{
    if (error == NULL) return @"No additional error information is available.";
    nw_error_domain_t domain = nw_error_get_error_domain(error);
    int code = nw_error_get_error_code(error);
    return [NSString stringWithFormat:@"Network error domain %u, code %d.", domain, code];
}

int alvr_run_local_network_consent(void)
{
    @autoreleasepool {
        NSApplication *application = [NSApplication sharedApplication];
        [application setActivationPolicy:NSApplicationActivationPolicyRegular];

        NSAlert *alert = [[NSAlert alloc] init];
        alert.messageText = @"Allow Local Network access for ALVR macOS Bridge";
        alert.informativeText =
            @"macOS may display a Local Network permission prompt. Respond to it, "
             "then return here and click Continue.";
        alert.alertStyle = NSAlertStyleInformational;
        [alert addButtonWithTitle:@"Continue"];

        nw_browse_descriptor_t descriptor =
            nw_browse_descriptor_create_bonjour_service("_alvr._tcp", NULL);
        nw_parameters_t parameters = nw_parameters_create_secure_tcp(
            NW_PARAMETERS_DISABLE_PROTOCOL,
            NW_PARAMETERS_DEFAULT_CONFIGURATION);
        if (descriptor == NULL || parameters == NULL) {
            return ALVR_LOCAL_NETWORK_SETUP_FAILED;
        }
        nw_browser_t browser = nw_browser_create(descriptor, parameters);
        if (browser == NULL) return ALVR_LOCAL_NETWORK_SETUP_FAILED;

        __block int outcome = ALVR_LOCAL_NETWORK_WAITING;
        nw_browser_set_queue(browser, dispatch_get_main_queue());
        nw_browser_set_state_changed_handler(
            browser,
            ^(nw_browser_state_t state, nw_error_t error) {
                switch (state) {
                case nw_browser_state_ready:
                    outcome = ALVR_LOCAL_NETWORK_READY;
                    alert.informativeText =
                        @"Bonjour discovery is active for this ALVR bridge identity. "
                         "Click Continue to return to the runtime CLI.";
                    break;
                case nw_browser_state_waiting:
                    if (error != NULL &&
                        nw_error_get_error_domain(error) == nw_error_domain_dns &&
                        nw_error_get_error_code(error) == kDNSServiceErr_PolicyDenied) {
                        outcome = ALVR_LOCAL_NETWORK_POLICY_DENIED;
                        alert.informativeText =
                            @"Local Network access is denied. Enable ALVR macOS Bridge "
                             "in System Settings > Privacy & Security > Local Network, "
                             "then click Continue.";
                    } else {
                        outcome = ALVR_LOCAL_NETWORK_WAITING;
                        alert.informativeText = error_description(error);
                    }
                    break;
                case nw_browser_state_failed:
                    outcome = ALVR_LOCAL_NETWORK_FAILED;
                    alert.informativeText = error_description(error);
                    break;
                case nw_browser_state_cancelled:
                    if (outcome != ALVR_LOCAL_NETWORK_READY &&
                        outcome != ALVR_LOCAL_NETWORK_POLICY_DENIED) {
                        outcome = ALVR_LOCAL_NETWORK_CANCELLED;
                    }
                    break;
                case nw_browser_state_invalid:
                    break;
                }
            });
        nw_browser_start(browser);

        dispatch_after(
            dispatch_time(DISPATCH_TIME_NOW, 120 * NSEC_PER_SEC),
            dispatch_get_main_queue(),
            ^{
                if ([NSApp modalWindow] == alert.window) {
                    outcome = ALVR_LOCAL_NETWORK_CANCELLED;
                    [NSApp abortModal];
                }
            });

        [application activateIgnoringOtherApps:YES];
        [alert runModal];
        nw_browser_cancel(browser);
        return outcome;
    }
}
