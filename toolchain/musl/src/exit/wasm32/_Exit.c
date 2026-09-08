#include <stdlib.h>
#include "syscall.h"
_Noreturn void __wasmux_vfork_return(long pid);
_Noreturn void _Exit(int ec)
{
	long r = __syscall(SYS_exit_group, ec);
	if (r > 0) __wasmux_vfork_return(r); /* vfork child exiting without exec */
	for (;;) __syscall(SYS_exit, ec);
}
