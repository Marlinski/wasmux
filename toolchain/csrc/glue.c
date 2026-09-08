/* The seam between the wasm2c-generated program images and the Rust kernel.
 *
 * Three jobs, and nothing else:
 *
 *   1. Publish a vtable per image, so Rust can drive any program through one shape and needs
 *      no knowledge of wasm2c's generated names or of `wasm_rt_memory_t`'s layout.
 *   2. Forward the six `wasmux` imports to Rust.
 *   3. Enter the guest with wasm2c's trap catcher armed, so a guest trap becomes a return
 *      code, which the kernel turns into one dead process rather than a dead component.
 *
 * The image list is generated: `build-archive.sh` writes `wasmux-images.h`, which includes
 * each program's header and defines WASMUX_IMAGE_TABLE. Adding a program touches no C here.
 */
#include <stddef.h>
#include <stdint.h>

#include "wasm-rt.h"
#include "wasm-rt-impl.h"
#include "wasm-rt-exceptions.h"

#include "wasmux-images.h"

typedef struct wasmux_image {
    const char *name;
    size_t inst_size;
    void (*instantiate)(void *, struct w2c_wasmux *);
    void (*free_inst)(void *);
    wasm_rt_memory_t *(*memory)(void *);
    u32 *(*stack_pointer)(void *);
    void (*start)(void *);
    void (*asy_start_unwind)(void *, u32);
    void (*asy_stop_unwind)(void *);
    void (*asy_start_rewind)(void *, u32);
    void (*asy_stop_rewind)(void *);
    u32 (*asy_get_state)(void *);
} wasmux_image;

/* wasm2c mangles export names, so `__stack_pointer` becomes `0x5F_stack_pointer` and
 * `_start` becomes `0x5Fstart`. The casts are to one uniform `void *` instance argument. */
#define IMG(N)                                                                                 \
    {                                                                                          \
        #N, sizeof(w2c_##N),                                                                   \
        (void (*)(void *, struct w2c_wasmux *))wasm2c_##N##_instantiate,                       \
        (void (*)(void *))wasm2c_##N##_free,                                                   \
        (wasm_rt_memory_t * (*)(void *)) w2c_##N##_memory,                                     \
        (u32 * (*)(void *)) w2c_##N##_0x5F_stack_pointer,                                      \
        (void (*)(void *))w2c_##N##_0x5Fstart,                                                 \
        (void (*)(void *, u32))w2c_##N##_asyncify_start_unwind,                                \
        (void (*)(void *))w2c_##N##_asyncify_stop_unwind,                                      \
        (void (*)(void *, u32))w2c_##N##_asyncify_start_rewind,                                \
        (void (*)(void *))w2c_##N##_asyncify_stop_rewind,                                      \
        (u32(*)(void *))w2c_##N##_asyncify_get_state                                           \
    }

static const wasmux_image images[] = { WASMUX_IMAGE_TABLE };

const wasmux_image *wasmux_images(size_t *count) {
    *count = sizeof(images) / sizeof(images[0]);
    return images;
}

/* ---- the six imports; Rust decides what they mean ---- */

extern u32 rs_syscall(void *env, u32 number, u32 args);
extern u32 rs_setjmp(void *env, u32 jmp_buf);
extern void rs_longjmp(void *env, u32 jmp_buf, u32 value);
extern u32 rs_args(void *env, u32 buffer, u32 capacity);
extern void rs_init(void *env, u32 signal_flag);
extern u32 rs_sigfetch(void *env, u32 out);

u32 w2c_wasmux_syscall(struct w2c_wasmux *env, u32 number, u32 args) {
    return rs_syscall(env, number, args);
}
u32 w2c_wasmux_setjmp(struct w2c_wasmux *env, u32 jmp_buf) { return rs_setjmp(env, jmp_buf); }
void w2c_wasmux_longjmp(struct w2c_wasmux *env, u32 jmp_buf, u32 value) {
    rs_longjmp(env, jmp_buf, value);
}
u32 w2c_wasmux_args(struct w2c_wasmux *env, u32 buffer, u32 capacity) {
    return rs_args(env, buffer, capacity);
}
void w2c_wasmux_init(struct w2c_wasmux *env, u32 signal_flag) { rs_init(env, signal_flag); }
u32 w2c_wasmux_sigfetch(struct w2c_wasmux *env, u32 out) { return rs_sigfetch(env, out); }

/* ---- runtime accessors, so `wasm_rt_memory_t` stays out of Rust ---- */

void wasmux_rt_init(void) { wasm_rt_init(); }
uint8_t *wasmux_mem_data(wasm_rt_memory_t *m) { return m->data; }
uint64_t wasmux_mem_size(wasm_rt_memory_t *m) { return m->size; }
uint64_t wasmux_mem_grow(wasm_rt_memory_t *m, uint64_t pages) {
    return wasm_rt_grow_memory(m, pages);
}

/* ---- entering the guest ---- */

/* `wasm_rt_impl_try` returns 0 on the way in and the trap code on the way back, so the guard
 * has to be the first thing in the function and the guest call the last. */
int wasmux_instantiate(const wasmux_image *img, void *inst, void *env) {
    int code = wasm_rt_impl_try();
    if (code) return code;
    img->instantiate(inst, (struct w2c_wasmux *)env);
    return 0;
}

int wasmux_run_start(const wasmux_image *img, void *inst) {
    int code = wasm_rt_impl_try();
    if (code) return code;
    img->start(inst);
    return 0;
}

/* Collapse wasm2c's trap enum to the three kinds a Linux wait status can express. Kept here
 * because this is where the enum is declared; Rust only sees 0, 1, 2 or 3. */
int wasmux_trap_class(int code) {
    switch ((wasm_rt_trap_t)code) {
        case WASM_RT_TRAP_NONE:
            return 0;
        case WASM_RT_TRAP_OOB:
        case WASM_RT_TRAP_UNALIGNED:
            return 1;
        case WASM_RT_TRAP_EXHAUSTION:
            return 2;
        default:
            return 3;
    }
}

const char *wasmux_trap_name(int code) { return wasm_rt_strerror((wasm_rt_trap_t)code); }
