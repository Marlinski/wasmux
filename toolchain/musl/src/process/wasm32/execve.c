#include <unistd.h>
#include "syscall.h"
_Noreturn void __wasmux_vfork_return(long pid);
int execve(const char *path, char *const argv[], char *const envp[])
{
	long r = __syscall(SYS_execve, path, argv, envp);
	if (r > 0) __wasmux_vfork_return(r); /* we were a vfork child: back to the parent */
	return __syscall_ret(r);
}
