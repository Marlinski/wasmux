#include <signal.h>
#include <stdint.h>
#include <string.h>
volatile int __wasmux_sigpending;
struct wasmux_sig { int32_t signo; uint32_t handler; uint32_t flags; int32_t pid; int32_t code; };
extern int
__attribute__((import_module("wasmux"), import_name("sigfetch")))
__wasmux_sigfetch(struct wasmux_sig *out);
void __wasmux_deliver_signals(void)
{
	struct wasmux_sig s;
	while (__wasmux_sigfetch(&s)) {
		if (s.flags & SA_SIGINFO) {
			siginfo_t si; memset(&si, 0, sizeof si);
			si.si_signo = s.signo; si.si_code = s.code; si.si_pid = s.pid;
			((void (*)(int, siginfo_t *, void *))(uintptr_t)s.handler)(s.signo, &si, 0);
		} else {
			((void (*)(int))(uintptr_t)s.handler)(s.signo);
		}
	}
}
