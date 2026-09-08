/* Fallback for programs whose main() takes no arguments (clang leaves that name alone). */
extern int __wasmux_main_void(void) __asm__("main");
__attribute__((__weak__)) int __main_argc_argv(int argc, char **argv) { return __wasmux_main_void(); }
