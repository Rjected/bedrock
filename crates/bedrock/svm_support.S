/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Bedrock VMX Assembly Support
 *
 * Assembly code for VMLAUNCH/VMRESUME and guest/host context switching.
 * On VM exit, HOST_RSP points to the VmxContext structure so the exit handler
 * can save guest registers before restoring the kernel stack frame.
 */

#include <linux/linkage.h>
#include <linux/objtool.h>
#include <asm/asm.h>
#include <asm/frame.h>
#include <asm/nospec-branch.h>
#include <asm/unwind_hints.h>
#include <asm/segment.h>

.text

/* Offsets in struct vmx_context (must match Rust VmxContext struct) */
#define VMXCTX_GUEST_RAX	0
#define VMXCTX_GUEST_RBX	8
#define VMXCTX_GUEST_RCX	16
#define VMXCTX_GUEST_RDX	24
#define VMXCTX_GUEST_RSI	32
#define VMXCTX_GUEST_RDI	40
#define VMXCTX_GUEST_RBP	48
#define VMXCTX_GUEST_R8		56
#define VMXCTX_GUEST_R9		64
#define VMXCTX_GUEST_R10	72
#define VMXCTX_GUEST_R11	80
#define VMXCTX_GUEST_R12	88
#define VMXCTX_GUEST_R13	96
#define VMXCTX_GUEST_R14	104
#define VMXCTX_GUEST_R15	112

#define VMXCTX_HOST_RAX		120
#define VMXCTX_HOST_RBX		128
#define VMXCTX_HOST_RCX		136
#define VMXCTX_HOST_RDX		144
#define VMXCTX_HOST_RSI		152
#define VMXCTX_HOST_RDI		160
#define VMXCTX_HOST_RBP		168
#define VMXCTX_HOST_R8		176
#define VMXCTX_HOST_R9		184
#define VMXCTX_HOST_R10		192
#define VMXCTX_HOST_R11		200
#define VMXCTX_HOST_R12		208
#define VMXCTX_HOST_R13		216
#define VMXCTX_HOST_R14		224
#define VMXCTX_HOST_R15		232

#define VMXCTX_LAUNCHED		240

/* XSAVE state pointers and mask (for FPU/SSE/AVX isolation) */
#define VMXCTX_GUEST_XSAVE_PTR	248
#define VMXCTX_HOST_XSAVE_PTR	256
#define VMXCTX_XCR0_MASK	264
#define VMXCTX_HOST_XCR0	272
#define VMXCTX_GUEST_CR2	280
#define VMXCTX_SVM_GUEST_XSAVEOPT 288

/* SVM entry: ctx, guest VMCB PA, host VMCB PA, breakpoints, PMU mask,
 * guest VMCB virtual pointer. The final argument exposes hardware writes to
 * the compiler; assembly uses the physical address. Host RSP is saved by
 * VMRUN, so the stack remains our anchor without guest-visible scratch GPRs. */
SYM_FUNC_START(svm_run_guest)
 push %rbp
 mov %rsp, %rbp
 push %r15
 push %r14
 push %r13
 push %r12
 push %rbx
 sub $88, %rsp
 mov %rdi, 0(%rsp)
 mov %rsi, 8(%rsp)
 mov %rdx, 16(%rsp)
 mov %r8, 48(%rsp)
 clgi
 /* V_INTR_MASKING uses host IF at VMRUN. GIF=0 protects host setup;
  * VMRUN enables guest GIF, so physical IRQs can terminate a long batch. */
 sti
 mov %dr7, %rax
 mov %rax, 32(%rsp)
 mov $0x400, %rax
 mov %rax, %dr7
 movq $0, 80(%rsp)
 /* Scalar entries need no hardware execution breakpoints. Guest DR access
  * is intercepted; DR7 stays disabled while host DR0-DR3 remain untouched. */
 testb $0xff, 0x560(%r9)
 jz .Lsvm_skip_breakpoint_regs_entry
 movq $1, 80(%rsp)
 mov %dr0, %rax
 mov %rax, 24(%rsp)
 mov %dr1, %rax
 mov %rax, 56(%rsp)
 mov %dr2, %rax
 mov %rax, 64(%rsp)
 mov %dr3, %rax
 mov %rax, 72(%rsp)
 mov 0(%rcx), %rax
 mov %rax, %dr0
 mov 8(%rcx), %rax
 mov %rax, %dr1
 mov 16(%rcx), %rax
 mov %rax, %dr2
 mov 24(%rcx), %rax
 mov %rax, %dr3
.Lsvm_skip_breakpoint_regs_entry:
 mov %rdx, %rax
 vmsave %rax
	mov	VMXCTX_XCR0_MASK(%rdi), %rax
	test	%rax, %rax
	jz	.Lsvm_skip_xsave_entry

	/* Save current (host) XCR0 value */
	push	%rdi			/* Save ctx pointer */
	xor	%ecx, %ecx		/* XCR0 index = 0 */
	xgetbv				/* EDX:EAX = XCR0 */
	pop	%rdi			/* Restore ctx pointer */
	shl	$32, %rdx
	or	%rax, %rdx
	mov	%rdx, VMXCTX_HOST_XCR0(%rdi)

	/* Save host FPU/SSE state to host_xsave_ptr */
	mov	VMXCTX_XCR0_MASK(%rdi), %rax
	mov	%rax, %rdx
	shr	$32, %rdx		/* EDX = high 32 bits of mask */
	mov	VMXCTX_HOST_XSAVE_PTR(%rdi), %rcx
	xsave64	(%rcx)

	/* Set XCR0 to guest's value (xcr0_mask) before loading guest state */
	push	%rdi			/* Save ctx pointer */
	mov	VMXCTX_XCR0_MASK(%rdi), %rax
	mov	%rax, %rdx
	shr	$32, %rdx		/* EDX = high 32 bits */
	xor	%ecx, %ecx		/* XCR0 index = 0 */
	xsetbv				/* XCR0 = EDX:EAX */
	pop	%rdi			/* Restore ctx pointer */

	/* Load guest FPU/SSE state from guest_xsave_ptr */
	mov	VMXCTX_XCR0_MASK(%rdi), %rax
	mov	%rax, %rdx
	shr	$32, %rdx
	mov	VMXCTX_GUEST_XSAVE_PTR(%rdi), %rcx
	xrstor64 (%rcx)

.Lsvm_skip_xsave_entry:
 /* Preserve host counter enables; only our perf-owned GuestOnly event runs
  * in the guest. Linux 7 restricts amd_pmu_enable_virt to KVM modules. */
 cmpq $0, 48(%rsp)
 je .Lsvm_skip_pmu_entry
 mov $0xc0000301, %ecx
 rdmsr
 shl $32, %rdx
 or %rdx, %rax
 mov %rax, 40(%rsp)
 mov 48(%rsp), %rax
 btr $63, %rax
 mov %rax, %rdx
 shr $32, %rdx
 wrmsr
.Lsvm_skip_pmu_entry:
 mov 8(%rsp), %rax
 vmload %rax
 mov VMXCTX_GUEST_RBX(%rdi), %rbx
 mov VMXCTX_GUEST_RCX(%rdi), %rcx
 mov VMXCTX_GUEST_RDX(%rdi), %rdx
 mov VMXCTX_GUEST_RSI(%rdi), %rsi
 mov VMXCTX_GUEST_RBP(%rdi), %rbp
 mov VMXCTX_GUEST_R8(%rdi), %r8
 mov VMXCTX_GUEST_R9(%rdi), %r9
 mov VMXCTX_GUEST_R10(%rdi), %r10
 mov VMXCTX_GUEST_R11(%rdi), %r11
 mov VMXCTX_GUEST_R12(%rdi), %r12
 mov VMXCTX_GUEST_R13(%rdi), %r13
 mov VMXCTX_GUEST_R14(%rdi), %r14
 mov VMXCTX_GUEST_R15(%rdi), %r15
 mov VMXCTX_GUEST_RDI(%rdi), %rdi
 mov 8(%rsp), %rax
 vmrun %rax
 cli
 /* RAX on exit is the VMCB physical address, not guest RAX. */
 push %rdi
 mov 8(%rsp), %rax
 mov %rbx, VMXCTX_GUEST_RBX(%rax)
 mov %rcx, VMXCTX_GUEST_RCX(%rax)
 mov %rdx, VMXCTX_GUEST_RDX(%rax)
 mov %rsi, VMXCTX_GUEST_RSI(%rax)
 mov %rbp, VMXCTX_GUEST_RBP(%rax)
 mov %r8, VMXCTX_GUEST_R8(%rax)
 mov %r9, VMXCTX_GUEST_R9(%rax)
 mov %r10, VMXCTX_GUEST_R10(%rax)
 mov %r11, VMXCTX_GUEST_R11(%rax)
 mov %r12, VMXCTX_GUEST_R12(%rax)
 mov %r13, VMXCTX_GUEST_R13(%rax)
 mov %r14, VMXCTX_GUEST_R14(%rax)
 mov %r15, VMXCTX_GUEST_R15(%rax)
 mov 0(%rsp), %rcx
 mov %rcx, VMXCTX_GUEST_RDI(%rax)
 add $8, %rsp
 cmpq $0, 48(%rsp)
 je .Lsvm_skip_pmu_exit
 mov $0xc0000301, %ecx
 mov 40(%rsp), %rax
 mov %rax, %rdx
 shr $32, %rdx
 wrmsr
.Lsvm_skip_pmu_exit:
 mov 8(%rsp), %rax
 vmsave %rax
 mov 16(%rsp), %rax
 vmload %rax
 mov 0(%rsp), %rdi
	mov	VMXCTX_XCR0_MASK(%rdi), %rax
	test	%rax, %rax
	jz	.Lsvm_skip_xsave_exit

	/* Save guest FPU/SSE state to guest_xsave_ptr (XCR0 is still guest's value) */
	mov	%rax, %rdx
	shr	$32, %rdx		/* EDX = high 32 bits of mask */
	mov	VMXCTX_GUEST_XSAVE_PTR(%rdi), %rcx
	cmpq	$2, VMXCTX_SVM_GUEST_XSAVEOPT(%rdi)
	jne	.Lsvm_guest_xsave_full
	xsaveopt64 (%rcx)
	jmp	.Lsvm_guest_xsave_done
.Lsvm_guest_xsave_full:
	xsave64	(%rcx)
	cmpq	$1, VMXCTX_SVM_GUEST_XSAVEOPT(%rdi)
	jne	.Lsvm_guest_xsave_done
	movq	$2, VMXCTX_SVM_GUEST_XSAVEOPT(%rdi)
.Lsvm_guest_xsave_done:

	/* Restore host XCR0 before loading host FPU state */
	mov	VMXCTX_HOST_XCR0(%rdi), %rax
	mov	%rax, %rdx
	shr	$32, %rdx		/* EDX = high 32 bits */
	xor	%ecx, %ecx		/* XCR0 index = 0 */
	xsetbv				/* XCR0 = EDX:EAX */

	/* Load host FPU/SSE state from host_xsave_ptr */
	mov	VMXCTX_XCR0_MASK(%rdi), %rax
	mov	%rax, %rdx
	shr	$32, %rdx
	mov	VMXCTX_HOST_XSAVE_PTR(%rdi), %rcx
	xrstor64 (%rcx)

.Lsvm_skip_xsave_exit:
 cmpq $0, 80(%rsp)
 je .Lsvm_skip_breakpoint_regs_exit
 mov 24(%rsp), %rax
 mov %rax, %dr0
 mov 56(%rsp), %rax
 mov %rax, %dr1
 mov 64(%rsp), %rax
 mov %rax, %dr2
 mov 72(%rsp), %rax
 mov %rax, %dr3
.Lsvm_skip_breakpoint_regs_exit:
 mov 32(%rsp), %rax
 mov %rax, %dr7
 stgi
 add $88, %rsp
 pop %rbx
 pop %r12
 pop %r13
 pop %r14
 pop %r15
 pop %rbp
 xor %eax, %eax
 RET
SYM_FUNC_END(svm_run_guest)
STACK_FRAME_NON_STANDARD(svm_run_guest)
