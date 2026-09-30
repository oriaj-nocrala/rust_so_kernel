
.section .text
.global __mlibc_start_thread
__mlibc_start_thread:
	pop %rdi
	pop %rsi
	pop %rdx
	call __mlibc_enter_thread

# long __constanos_clone(unsigned long flags, void *stack, int *ptid, int *ctid, void *tls)
# Linux's clone(2) x86-64 register order (ctid goes in r10). The child returns 0 with rsp = stack; there it must not return
# into the caller's frame, so it goes to __mlibc_start_thread, which pops what sys_prepare_stack pushed.
.global __constanos_clone
__constanos_clone:
	mov %rcx, %r10
	mov $56, %eax
	syscall
	test %rax, %rax
	jnz 1f
	xor %ebp, %ebp
	jmp __mlibc_start_thread
1:	ret

# A signal handler returns here (sa_restorer): rt_sigreturn(15) with rsp at the ucontext.
.global __mlibc_signal_restore
.type __mlibc_signal_restore, @function
__mlibc_signal_restore:
.global __mlibc_signal_restore_rt
.type __mlibc_signal_restore_rt, @function
__mlibc_signal_restore_rt:
	mov $15, %rax
	syscall
	ud2

.section .note.GNU-stack,"",%progbits

