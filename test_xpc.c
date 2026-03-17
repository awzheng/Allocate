#include <stdio.h>
#include <xpc/xpc.h>
#include <dispatch/dispatch.h>
#include <unistd.h>

int main(void) {
    printf("[test_xpc] Creating Mach service connection...\n");
    xpc_connection_t conn = xpc_connection_create_mach_service(
        "com.andrewzheng.allocate.daemon", NULL, 0);

    if (conn == NULL) {
        printf("[test_xpc] FATAL: xpc_connection_create_mach_service returned NULL\n");
        return 1;
    }

    xpc_connection_set_event_handler(conn, ^(xpc_object_t event) {
        xpc_type_t type = xpc_get_type(event);
        if (type == XPC_TYPE_ERROR) {
            const char *desc = xpc_dictionary_get_string(event, XPC_ERROR_KEY_DESCRIPTION);
            printf("[test_xpc] ERROR: %s\n", desc ? desc : "<null>");
        } else if (type == XPC_TYPE_DICTIONARY) {
            const char *payload = xpc_dictionary_get_string(event, "payload");
            printf("[test_xpc] PAYLOAD RECEIVED: %s\n", payload ? payload : "<null>");
        } else {
            printf("[test_xpc] Unknown event type received\n");
        }
    });

    xpc_connection_resume(conn);
    printf("[test_xpc] Connection resumed. Waiting 5s for events...\n");
    sleep(5);
    printf("[test_xpc] Done.\n");
    return 0;
}
