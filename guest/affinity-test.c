/* SPDX-License-Identifier: GPL-2.0 */
/* Exercise guest affinity syscalls under KVM, independently of SVM stepping. */
#ifndef EXPECT_NCPUS
#define EXPECT_NCPUS 8
#endif
static long syscall3(long number, long a, long b, long c)
{
    long result;
    __asm__ volatile("syscall" : "=a"(result)
        : "a"(number), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return result;
}
static void finish(int success)
{
    static const char pass[] = "GUEST_AFFINITY_PASS\n";
    static const char fail[] = "GUEST_AFFINITY_FAIL\n";
    syscall3(1, 1, (long)(success ? pass : fail), sizeof(pass) - 1);
    syscall3(172, 3, 0, 0); /* iopl: QEMU's isa-debug-exit test device. */
    unsigned int value = success ? 16 : 17;
    __asm__ volatile("outl %0, %1" :: "a"(value), "Nd"((unsigned short)0xf4));
    for (;;) __asm__ volatile("ud2");
}
void _start(void)
{
    unsigned long mask[16] = {0};
    if (syscall3(204, 0, sizeof(mask), (long)mask) < 8 ||
            mask[0] != ((1UL << EXPECT_NCPUS) - 1))
        finish(0);
    for (int i = 1; i < 16; i++) if (mask[i]) finish(0);
    mask[0] = 1UL << (EXPECT_NCPUS - 1);
    if (syscall3(203, 0, sizeof(mask), (long)mask)) finish(0);
    mask[0] = 0;
    if (syscall3(204, 0, sizeof(mask), (long)mask) < 8 ||
            mask[0] != ((1UL << EXPECT_NCPUS) - 1))
        finish(0);
    if (syscall3(203, 2147483647, sizeof(mask), (long)mask) != -3)
        finish(0);
    finish(1);
}
