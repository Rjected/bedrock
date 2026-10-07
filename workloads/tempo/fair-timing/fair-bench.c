#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include "libvmcall.h"

static int mark(uint64_t id) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) { perror("clock_gettime"); return -1; }
    uint64_t ns = (uint64_t)now.tv_sec * 1000000000ULL + now.tv_nsec;
    vmcall3(HYPERCALL_READY, 0x4641495254494d45ULL, id, ns);
    return 0;
}
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    /* Both clocks bracket exactly the same spawn, bench, and wait interval. */
    if (mark(1)) return 1;
    pid_t child = fork();
    if (child < 0) { perror("fork"); return 1; }
    if (child == 0) { execvp(argv[1], argv + 1); perror("execvp"); _exit(127); }
    int status;
    while (waitpid(child, &status, 0) < 0) { if (errno != EINTR) { perror("waitpid"); return 1; } }
    if (mark(2)) return 1;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}
