/*
 * CCOMPAT stage CC5 end-to-end C program.
 *
 * This is a minimal program written in C — not Rust — that talks to the RustOS
 * kernel only through the generated abi-v1 C headers (include/rustos/...) and
 * the curated System runtime / C ABI class (crt0 + the ros_sys_* syscall
 * stubs). It proves the header, the syscall stub runtime, and crt0 all agree
 * with the Rust side end to end (plans/CCOMPAT.md stage CC5).
 *
 * It exercises a representative slice of abi-v1:
 *   - a Time64 value across the §21 pre-1970 / post-2038 boundaries,
 *   - an ipc message header (magic + field round-trip),
 *   - a sysinfo request header (magic + fields),
 *   - two real syscall round-trips (cap_query, clock_get).
 *
 * crt0 calls main(); main returns an exit code; crt0 routes it through the
 * exit syscall. The kernel-side QEMU test services cap_query / clock_get and
 * asserts the exit code is EXIT_OK, i.e. that every check above passed.
 *
 * RustOS itself stays Rust-only (AGENTS.md §1); this program is *hosted* by
 * the OS, it is not part of the OS.
 */

#include <stdint.h>

#include "rustos/rustos_capability.h"
#include "rustos/rustos_syscall.h"
#include "rustos/rustos_time.h"
#include "rustos/rustos_ipc.h"
#include "rustos/rustos_sysinfo.h"

/* Exit code returned when every check passes. Distinct from crt0's reserved
 * fail-closed codes (70/71/72). The kernel-side test asserts this exact
 * value on the exit syscall. */
#define EXIT_OK 99

/* Per-check failure codes, each distinct so a failure pinpoints the stage. */
#define EXIT_TIME 81
#define EXIT_IPC 82
#define EXIT_SYSINFO 83
#define EXIT_CAP 84
#define EXIT_CLOCK 85

/* Capability id the program queries; the kernel callback asserts it saw
 * exactly this argument and answers "held". */
#define PROBE_CAP ROS_CAP_DRV_KERNEL

/* Sentinel value the kernel callback returns from clock_get, chosen to use
 * the full 64-bit width so the round-trip exercises u64 result marshalling. */
#define CLOCK_SENTINEL 0x0123456789abcdefULL

/* §21: a Time64 must represent instants outside the legacy 32-bit window in
 * its 64-bit `secs` field, on both sides of the epoch. */
static int check_time64(void) {
    /* 1900-01-01T00:00:00Z: negative seconds (before 1970). */
    ros_time64_t pre = {.secs = -2208988800LL, .nanos = 500000000u};
    /* 2100-01-01T00:00:00Z: beyond the 2038 (INT32_MAX seconds) boundary. */
    ros_time64_t post = {.secs = 4102444800LL, .nanos = 1u};

    if (pre.secs >= 0) {
        return 0;
    }
    if (post.secs <= (int64_t)INT32_MAX) {
        return 0;
    }
    /* Duration arithmetic across the epoch must not overflow the 64-bit span. */
    int64_t span = post.secs - pre.secs;
    if (span != (4102444800LL - (-2208988800LL))) {
        return 0;
    }
    /* Sub-second fields stay canonical (0..ROS_NANOS_PER_SEC). */
    if (pre.nanos >= ROS_NANOS_PER_SEC || post.nanos >= ROS_NANOS_PER_SEC) {
        return 0;
    }
    return 1;
}

/* The ipc message header round-trips its magic and fields through C storage. */
static int check_ipc_header(void) {
    ros_ipc_message_header_t header;
    header.magic = ROS_IPC_MESSAGE_HEADER_MAGIC;
    header.version = 1u;
    header.flags = 0u;
    header.endpoint = 0x1122334455667788ULL;
    header.sender = 0u;
    header.payload_len = 16u;
    header.reserved = 0u;

    if (header.magic != 0x31435049u) { /* "IPC1" little-endian */
        return 0;
    }
    if (header.endpoint != 0x1122334455667788ULL) {
        return 0;
    }
    if (header.payload_len > ROS_IPC_MESSAGE_MAX_PAYLOAD_LEN) {
        return 0;
    }
    return 1;
}

/* The sysinfo request header round-trips its magic and well-known query id. */
static int check_sysinfo_header(void) {
    ros_sysinfo_request_header_t request;
    request.magic = ROS_SYSINFO_REQUEST_MAGIC;
    request.version = ROS_SYSINFO_VERSION_V1;
    request.flags = 0u;
    request.query = ROS_SYSINFO_QUERY_UPTIME;
    request.reserved = 0u;
    request.payload_len = 0u;
    request.request_id = 1u;

    if (request.magic != 0x31495953u) { /* "SYI1" little-endian */
        return 0;
    }
    if (request.query != ROS_SYSINFO_QUERY_UPTIME) {
        return 0;
    }
    return 1;
}

/*
 * Program entry point, called by crt0 once the C runtime is set up.
 *
 * Returns EXIT_OK only if every abi-v1 check and both syscall round-trips
 * succeed; otherwise returns the per-stage failure code. crt0 routes the
 * return value through the exit syscall.
 */
int main(int argc, char **argv, char **envp) {
    (void)argc;
    (void)argv;
    (void)envp;

    if (!check_time64()) {
        return EXIT_TIME;
    }
    if (!check_ipc_header()) {
        return EXIT_IPC;
    }
    if (!check_sysinfo_header()) {
        return EXIT_SYSINFO;
    }
    /* Syscall round-trip 1: capability query. The kernel callback returns 1
     * ("held") and asserts the marshalled argument equals PROBE_CAP. */
    if (ros_sys_cap_query(PROBE_CAP) != 1u) {
        return EXIT_CAP;
    }
    /* Syscall round-trip 2: monotonic clock. The kernel callback returns the
     * 64-bit sentinel; a correct stub delivers it back intact. */
    if (ros_sys_clock_get() != CLOCK_SENTINEL) {
        return EXIT_CLOCK;
    }
    return EXIT_OK;
}
