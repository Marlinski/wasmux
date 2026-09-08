#include <errno.h>
/* No threads yet: every process is single-threaded. */
int __clone(int (*func)(void *), void *stack, int flags, void *arg, ...) { return -ENOSYS; }
