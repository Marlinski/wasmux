#include <unistd.h>
#include <setjmp.h>
#include <errno.h>
#include "syscall.h"
#define VFORK_DEPTH 8
static jmp_buf jbs[VFORK_DEPTH];
static int depth;
jmp_buf *__wasmux_vfork_jbp(void) { return &jbs[depth < VFORK_DEPTH ? depth : VFORK_DEPTH-1]; }
pid_t __wasmux_vfork_ret(int r)
{
	if (r) { /* came back via longjmp: r is the child's pid (or -errno) */
		if (depth) depth--;
		return __syscall_ret(r == -1 ? -EAGAIN : r);
	}
	depth++;
	long v = __syscall(SYS_clone, 0x4000|0x100|17 /* CLONE_VFORK|CLONE_VM|SIGCHLD */, 0, 0, 0, 0);
	if (v < 0) { depth--; return __syscall_ret(v); }
	return 0; /* child path, running in the parent's instance */
}
_Noreturn void __wasmux_vfork_return(long pid)
{
	longjmp(jbs[depth ? depth-1 : 0], (int)pid);
}
