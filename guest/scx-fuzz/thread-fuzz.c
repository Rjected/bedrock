// SPDX-License-Identifier: GPL-2.0
//
// Opt a command, and every process it spawns, into the in-kernel
// concurrency-fuzz scheduler by running it under SCHED_EXT.
//
// Usage: thread-fuzz <command> [args...]
//
// thread-fuzz sets its OWN scheduling policy to SCHED_EXT and then execs the
// given command. Scheduling policy is inherited across fork/exec, so every
// descendant of the command is governed by the fuzzing scheduler too, with no
// per-process opt-in. This is the manual-registration path used while we
// dogfood the scheduler: a workload opts in by wrapping the process it wants
// fuzzed, e.g. `thread-fuzz /usr/local/bin/queue`, and leaves everything else
// on the stock scheduler.
//
// It execs the target directly, so nothing in between resets the scheduling
// policy and plain inheritance suffices.
//
// The scheduler must already be attached (scx-init, at boot) with
// SCX_OPS_SWITCH_PARTIAL, so only the SCHED_EXT tasks thread-fuzz creates are
// governed while everything else stays on the stock scheduler. Setting
// SCHED_EXT needs CAP_SYS_NICE; the fuzzed workload's container runs privileged.
//
// Determinism is unchanged: the schedule is drawn from bedrock's getrandom
// stream (see scx-init.c), a pure function of the fuzzer input under the single
// vCPU + emulated TSC.
//
// Staying in SCHED_EXT. Under SCX_OPS_SWITCH_PARTIAL a task that changes its own
// policy (e.g. to SCHED_IDLE or SCHED_OTHER) silently leaves sched_ext for the
// fair class, which always runs ahead of sched_ext tasks: on the guest's single
// CPU one busy fair thread then starves every SCHED_EXT thread indefinitely
// (Linux 6.18 has no DL server for sched_ext). reth does exactly this: it moves
// its tracing-appender thread to SCHED_IDLE (reth crates/tasks/src/utils.rs,
// deprioritize_background_threads), and at shutdown that thread spins in
// sched_yield ahead of all 60+ node threads until podman SIGKILLs the node --
// a harness artifact, not a node bug. (sched_ext's stall watchdog does flag
// such starved tasks, but its exit path runs from an irq_work, which needs a
// self-IPI that bedrock's emulated APIC drops, so the scheduler is never
// disabled and the stall persists.) So before exec, thread-fuzz installs a
// seccomp filter (inherited by the whole process tree) that keeps every task
// in SCHED_EXT:
//
//   sched_setscheduler(pid, policy, param)
//       allowed only if policy is SCHED_EXT (optionally | SCHED_RESET_ON_FORK);
//       any other policy fails with EPERM.
//   sched_setattr(pid, attr, flags)
//       always EPERM: the policy is inside *attr and seccomp cannot dereference
//       pointers, so it cannot tell a SCHED_EXT request from a SCHED_IDLE one.
//       glibc never uses it (pthread_setschedparam & co. go through
//       sched_setscheduler/sched_setparam), and callers already have to cope
//       with EPERM from it when unprivileged.
//   sched_setparam, nice/setpriority
//       untouched: they change priority/weight within the current class, never
//       the policy.
//
// EPERM is what an unprivileged process gets anyway, so callers handle it: reth
// logs the failure at debug level and keeps going. Both the x86-64 (and x32)
// and i386 syscall numbers are covered. Set THREAD_FUZZ_SECCOMP=0 to skip the
// filter (debugging only: the node can then escape the fuzzing scheduler).

#define _GNU_SOURCE
#include <errno.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sched.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SCHED_EXT
#define SCHED_EXT 7
#endif
#ifndef SCHED_RESET_ON_FORK
#define SCHED_RESET_ON_FORK 0x40000000
#endif

// Syscall numbers, spelled out so the filter does not depend on which ABI the
// headers describe. x32 uses the x86-64 numbers plus __X32_SYSCALL_BIT, which
// the filter masks off.
#define X32_SYSCALL_BIT 0x40000000u
#define NR64_sched_setscheduler 144
#define NR64_sched_setattr 314
#define NR32_sched_setscheduler 156
#define NR32_sched_setattr 351

#define LD_ABS(field) \
	BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, field))
// Low 32 bits of syscall argument 1 (little-endian); the policy is an int.
#define LD_ARG1_LO \
	BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[1]))
#define RET(v) BPF_STMT(BPF_RET | BPF_K, (v))

// Install the keep-SCHED_EXT filter described at the top of this file.
static int install_sched_filter(void)
{
	// Jump offsets are relative to the next instruction; the [n] labels are
	// instruction indices. ALLOW is [13], EPERM is [14].
	struct sock_filter f[] = {
		/* [0] */ LD_ABS(arch),
		/* [1] */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 0, 4),
		/* x86-64 / x32: [2..5] */
		/* [2] */ LD_ABS(nr),
		/* [3] */ BPF_STMT(BPF_ALU | BPF_AND | BPF_K, ~X32_SYSCALL_BIT),
		/* [4] */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, NR64_sched_setscheduler, 5, 0),
		/* [5] */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, NR64_sched_setattr, 8, 7),
		/* [6] */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_I386, 0, 6),
		/* i386: [7..9] */
		/* [7] */ LD_ABS(nr),
		/* [8] */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, NR32_sched_setscheduler, 1, 0),
		/* [9] */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, NR32_sched_setattr, 4, 3),
		/* sched_setscheduler: allow only SCHED_EXT[|SCHED_RESET_ON_FORK] */
		/* [10] */ LD_ARG1_LO,
		/* [11] */ BPF_STMT(BPF_ALU | BPF_AND | BPF_K, ~(__u32)SCHED_RESET_ON_FORK),
		/* [12] */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SCHED_EXT, 0, 1),
		/* [13] */ RET(SECCOMP_RET_ALLOW),
		/* [14] */ RET(SECCOMP_RET_ERRNO | EPERM),
	};
	struct sock_fprog prog = {
		.len = sizeof(f) / sizeof(f[0]),
		.filter = f,
	};

	// The node runs as root in a privileged container, so this works without
	// no_new_privs; fall back to setting it (which needs no privilege) only if
	// the plain install is refused, to leave setuid/file-cap execs alone.
	if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog) == 0)
		return 0;
	if (errno != EACCES)
		return -1;
	if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0)
		return -1;
	return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog);
}

int main(int argc, char **argv)
{
	struct sched_param p = { .sched_priority = 0 };

	if (argc < 2) {
		fprintf(stderr, "usage: %s <command> [args...]\n", argv[0]);
		return 2;
	}

	// Raw syscall, not the glibc wrapper: some libc versions reject an
	// unknown policy value (SCHED_EXT == 7) before the syscall.
	if (syscall(SYS_sched_setscheduler, 0, SCHED_EXT, &p) != 0) {
		fprintf(stderr, "thread-fuzz: sched_setscheduler(SCHED_EXT): %s\n",
			strerror(errno));
		return 1;
	}

	// Keep the whole tree in SCHED_EXT (see top of file). Fail closed: a node
	// that can drop out of the fuzzing scheduler reintroduces the starvation
	// artifact, so a missing filter is an error unless explicitly opted out.
	const char *opt = getenv("THREAD_FUZZ_SECCOMP");
	if (!(opt && strcmp(opt, "0") == 0) && install_sched_filter() != 0) {
		fprintf(stderr, "thread-fuzz: seccomp filter: %s "
			"(THREAD_FUZZ_SECCOMP=0 skips it)\n", strerror(errno));
		return 1;
	}

	// Descendants inherit SCHED_EXT; only returns here if exec fails.
	execvp(argv[1], &argv[1]);

	fprintf(stderr, "thread-fuzz: exec %s: %s\n", argv[1], strerror(errno));
	return 127;
}
