#define __SYSCALL_LL_E(x) \
((union { long long ll; long l[2]; }){ .ll = x }).l[0], \
((union { long long ll; long l[2]; }){ .ll = x }).l[1]
#define __SYSCALL_LL_O(x) __SYSCALL_LL_E((x))

/* wasmux host ABI: one import for all Linux syscalls. args points to 6 longs. */
extern long
__attribute__((import_module("wasmux"), import_name("syscall")))
__wasmux_syscall(long n, long *args);

extern volatile int __wasmux_sigpending;   /* set by the host when a signal is deliverable */
void __wasmux_deliver_signals(void);

static inline long __wasmux_sc(long n, long *a)
{
	long r = __wasmux_syscall(n, a);
	if (__wasmux_sigpending) __wasmux_deliver_signals();
	return r;
}
static inline long __syscall0(long n)
{ long a[6] = {0}; return __wasmux_sc(n, a); }
static inline long __syscall1(long n, long a1)
{ long a[6] = {a1}; return __wasmux_sc(n, a); }
static inline long __syscall2(long n, long a1, long a2)
{ long a[6] = {a1,a2}; return __wasmux_sc(n, a); }
static inline long __syscall3(long n, long a1, long a2, long a3)
{ long a[6] = {a1,a2,a3}; return __wasmux_sc(n, a); }
static inline long __syscall4(long n, long a1, long a2, long a3, long a4)
{ long a[6] = {a1,a2,a3,a4}; return __wasmux_sc(n, a); }
static inline long __syscall5(long n, long a1, long a2, long a3, long a4, long a5)
{ long a[6] = {a1,a2,a3,a4,a5}; return __wasmux_sc(n, a); }
static inline long __syscall6(long n, long a1, long a2, long a3, long a4, long a5, long a6)
{ long a[6] = {a1,a2,a3,a4,a5,a6}; return __wasmux_sc(n, a); }
