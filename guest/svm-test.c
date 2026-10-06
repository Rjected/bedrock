/* SPDX-License-Identifier: GPL-2.0 */
/* Linux integration guest: snapshot, clock syscall and controlled randomness. */
#include "libvmcall.h"
static volatile vmcall_u64 report[512] __attribute__((aligned(4096)));
static const char identifier[] = "svm-linux-test";

static long syscall3(long number, long a, long b, long c)
{
    long result;
    __asm__ volatile("syscall" : "=a"(result)
        : "a"(number), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return result;
}

void _start(void)
{
    (void)*(volatile const char *)identifier; /* Fault in the identifier page. */
    report[0] = 1; /* Fault in the feedback page before registration. */
    if (vmcall_register_feedback_buffer((const void *)report, sizeof(report),
            identifier, sizeof(identifier) - 1) != 0)
    {
        vmcall_shutdown(); /* Host rejects shutdown before the snapshot. */
        for (;;) __asm__ volatile("ud2");
    }
    vmcall_snapshot();
    report[6] = syscall3(228, 1, (long)report, 0); /* CLOCK_MONOTONIC */
    report[7] = syscall3(318, (long)&report[2], 16, 0); /* getrandom */
    vmcall_u64 random, seed;
    __asm__ volatile("rdrand %0" : "=r"(random) : : "cc");
    __asm__ volatile("rdseed %0" : "=r"(seed) : : "cc");
    report[4] = random;
    report[5] = seed;
    vmcall_shutdown();
    for (;;) __asm__ volatile("hlt");
}
