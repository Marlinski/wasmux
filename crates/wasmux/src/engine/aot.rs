//! Programs compiled into the consumer's own module, ahead of time.
//!
//! `wasm2c` translates each guest program to C, that C is compiled for the consumer's target
//! and archived into `bin/libwasmux-images.a`, and `build.rs` links the archive in. A guest
//! function then *is* a function in the consumer's module: no interpreter loop, no dispatch
//! table, roughly two to three times native. One process is one heap-allocated instance
//! struct plus its own linear memory.
//!
//! The price is that the set of programs is fixed when the consumer is built, which for an
//! agent is the point rather than the cost: nothing runs that was not shipped. It also means
//! the archive is target-specific, so `bin/libwasmux-images.a` is built for one target and a
//! consumer on another runs `toolchain/build-archive.sh` again.
//!
//! Rust knows almost nothing about the generated code. Each image is reached through a vtable
//! of C function pointers (see `toolchain/csrc/glue.c`), instances are opaque blobs sized by
//! the C side, and `wasm_rt_memory_t` never appears in Rust at all. That keeps this module
//! stable across wabt releases: when the generated ABI moves, only the glue moves with it.

use std::alloc::Layout;
use std::ffi::{c_char, c_void, CStr};
use std::sync::Arc;

use crate::engine::{Backend, Exit, Instance, LoadError, Program, Suspend, TrapKind};
use crate::kernel::{imports, Ctx};

/// The vtable for one program image, laid out by `toolchain/csrc/glue.c`.
///
/// Every entry takes the instance blob as its first argument, exactly as the generated code
/// does. `instantiate` additionally takes the env pointer that comes back to every import.
#[repr(C)]
struct Image {
    name: *const c_char,
    inst_size: usize,
    instantiate: unsafe extern "C" fn(*mut c_void, *mut c_void),
    free_inst: unsafe extern "C" fn(*mut c_void),
    memory: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    stack_pointer: unsafe extern "C" fn(*mut c_void) -> *mut u32,
    start: unsafe extern "C" fn(*mut c_void),
    asy_start_unwind: unsafe extern "C" fn(*mut c_void, u32),
    asy_stop_unwind: unsafe extern "C" fn(*mut c_void),
    asy_start_rewind: unsafe extern "C" fn(*mut c_void, u32),
    asy_stop_rewind: unsafe extern "C" fn(*mut c_void),
    asy_get_state: unsafe extern "C" fn(*mut c_void) -> u32,
}

// The table is `static const` on the C side: shared immutably, never written.
unsafe impl Sync for Image {}
unsafe impl Send for Image {}

unsafe extern "C" {
    fn wasmux_images(count: *mut usize) -> *const Image;
    fn wasmux_rt_init();
    /// 0, or a trap code, so a trap during data-segment setup is an error and not an abort.
    fn wasmux_instantiate(image: *const Image, inst: *mut c_void, env: *mut c_void) -> i32;
    /// 0, or a trap code. Arms wasm2c's trap catcher, so a guest trap kills one process.
    fn wasmux_run_start(image: *const Image, inst: *mut c_void) -> i32;
    /// Collapses `wasm_rt_trap_t` to the three kinds a wait status can carry, in C where the
    /// enum is declared.
    fn wasmux_trap_class(code: i32) -> i32;
    fn wasmux_mem_data(memory: *mut c_void) -> *mut u8;
    fn wasmux_mem_size(memory: *mut c_void) -> u64;
    /// The previous page count, or `u64::MAX` if the growth was refused.
    fn wasmux_mem_grow(memory: *mut c_void, pages: u64) -> u64;
}

impl Image {
    fn name(&self) -> &'static str {
        // SAFETY: a NUL-terminated literal from the static table, valid for the process.
        unsafe { CStr::from_ptr(self.name) }.to_str().unwrap_or("?")
    }
}

/// The programs linked into this build.
pub(crate) struct Aot {
    images: &'static [Image],
}

impl Aot {
    pub(crate) fn new() -> Self {
        // wasm2c's runtime needs one initialisation before any instance exists. Several
        // sandboxes may be built concurrently, so this happens exactly once.
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe { wasmux_rt_init() });

        let mut count = 0usize;
        // SAFETY: returns the address and length of a static table.
        let images = unsafe {
            let first = wasmux_images(&mut count);
            if first.is_null() || count == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(first, count)
            }
        };
        Aot { images }
    }
}

impl Backend for Aot {
    fn builtin(&self, name: &str) -> Option<Arc<dyn Program>> {
        let image = self.images.iter().find(|image| image.name() == name)?;
        Some(Arc::new(Builtin(image)))
    }

    fn builtin_names(&self) -> Vec<&'static str> {
        self.images.iter().map(Image::name).collect()
    }
}

/// One linked image, instantiable once per process.
struct Builtin(&'static Image);

impl Program for Builtin {
    fn instantiate(&self, ctx: *mut c_void) -> Result<Box<dyn Instance>, LoadError> {
        Ok(Box::new(AotInstance::new(self.0, ctx)?))
    }
}

/// What an import call is handed: enough to find both the kernel's process state and the
/// instance the call came out of.
///
/// Boxed by [`AotInstance`], so its address is stable for the life of the process, and it is
/// the only thing the C side ever learns about the kernel.
#[repr(C)]
struct Env {
    /// The kernel's [`Ctx`] for this process. Owned by the kernel, outlives the instance.
    ctx: *mut c_void,
    image: &'static Image,
    inst: *mut c_void,
}

/// One live process: the generated instance struct, its memory, and its env.
struct AotInstance {
    env: Box<Env>,
    layout: Layout,
}

impl AotInstance {
    fn new(image: &'static Image, ctx: *mut c_void) -> Result<Self, LoadError> {
        // The generated struct's size and alignment are the C side's business; 16 covers
        // every scalar wasm2c stores in it.
        let layout = Layout::from_size_align(image.inst_size.max(16), 16)
            .map_err(|_| LoadError::Engine("bad instance layout".into()))?;
        // SAFETY: a non-zero layout. Zeroed because the generated code assumes it.
        let inst = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<c_void>();
        if inst.is_null() {
            return Err(LoadError::OutOfMemory);
        }
        let mut env = Box::new(Env { ctx, image, inst });
        let env_ptr = (&raw mut *env).cast::<c_void>();

        // SAFETY: a freshly zeroed blob of the size the image asked for, and an env pointer
        // that lives as long as the instance does. Traps are caught and reported.
        let code = unsafe { wasmux_instantiate(image, inst, env_ptr) };
        if code != 0 {
            // SAFETY: nothing was successfully constructed, so only the allocation is freed.
            unsafe { std::alloc::dealloc(inst.cast::<u8>(), layout) };
            return Err(if trap_kind(code) == Some(TrapKind::OutOfBounds) {
                LoadError::OutOfMemory
            } else {
                LoadError::Engine("the program trapped while starting up".into())
            });
        }
        Ok(AotInstance { env, layout })
    }

    fn memory(&self) -> *mut c_void {
        // SAFETY: the image's own accessor, on the instance it was made for.
        unsafe { (self.env.image.memory)(self.env.inst) }
    }
}

impl Drop for AotInstance {
    fn drop(&mut self) {
        // SAFETY: constructed successfully, dropped once. `free_inst` releases the linear
        // memory and the tables; the blob itself is ours.
        unsafe {
            (self.env.image.free_inst)(self.env.inst);
            std::alloc::dealloc(self.env.inst.cast::<u8>(), self.layout);
        }
    }
}

impl Instance for AotInstance {
    fn mem(&self) -> (*mut u8, usize) {
        raw_mem(self.memory())
    }

    fn mem_size(&self) -> u64 {
        // SAFETY: a live memory belonging to this instance.
        unsafe { wasmux_mem_size(self.memory()) }
    }

    fn mem_grow(&mut self, pages: u64) -> Option<u64> {
        grow(self.memory(), pages)
    }

    fn stack_pointer(&self) -> u32 {
        // SAFETY: the address of the instance's `__stack_pointer` global.
        unsafe { *(self.env.image.stack_pointer)(self.env.inst) }
    }

    fn set_stack_pointer(&mut self, value: u32) {
        // SAFETY: as above, and we hold `&mut self`.
        unsafe { *(self.env.image.stack_pointer)(self.env.inst) = value }
    }

    fn run_start(&mut self) -> Exit {
        // SAFETY: `wasmux_run_start` arms wasm2c's trap catcher before entering the guest, so
        // a trap returns a code here instead of aborting the module.
        let code = unsafe { wasmux_run_start(self.env.image, self.env.inst) };
        match trap_kind(code) {
            None => Exit::Returned,
            Some(kind) => Exit::Trap(kind),
        }
    }

    fn asy_start_unwind(&mut self, data: u32) {
        // SAFETY: an Asyncify export of this instance, called between guest entries.
        unsafe { (self.env.image.asy_start_unwind)(self.env.inst, data) }
    }

    fn asy_stop_unwind(&mut self) {
        // SAFETY: as above.
        unsafe { (self.env.image.asy_stop_unwind)(self.env.inst) }
    }

    fn asy_start_rewind(&mut self, data: u32) {
        // SAFETY: as above.
        unsafe { (self.env.image.asy_start_rewind)(self.env.inst, data) }
    }

    fn asy_stop_rewind(&mut self) {
        // SAFETY: as above.
        unsafe { (self.env.image.asy_stop_rewind)(self.env.inst) }
    }
}

/// The guest's memory as a base and a length. Refetched on every use: growing may move it.
fn raw_mem(memory: *mut c_void) -> (*mut u8, usize) {
    // SAFETY: `memory` came from an image's accessor on a live instance.
    unsafe { (wasmux_mem_data(memory), wasmux_mem_size(memory) as usize) }
}

fn grow(memory: *mut c_void, pages: u64) -> Option<u64> {
    // SAFETY: as above. `u64::MAX` is the refusal sentinel, not a page count.
    let previous = unsafe { wasmux_mem_grow(memory, pages) };
    if previous == u64::MAX {
        None
    } else {
        Some(previous)
    }
}

/// `None` when the call returned normally.
fn trap_kind(code: i32) -> Option<TrapKind> {
    if code == 0 {
        return None;
    }
    // SAFETY: a pure function over an integer.
    Some(match unsafe { wasmux_trap_class(code) } {
        0 => return None,
        1 => TrapKind::OutOfBounds,
        2 => TrapKind::Exhaustion,
        _ => TrapKind::Fault,
    })
}

/// The guest as seen from inside one of its own import calls.
///
/// The AOT backend has no borrow of a store to thread through, so this is just the env: the
/// vtable plus the instance. Unlike the interpreter's version there is nothing to re-enter,
/// which is why an import here costs a function call and a couple of loads.
struct AotSuspend<'a>(&'a mut Env);

impl Suspend for AotSuspend<'_> {
    fn mem(&mut self) -> (*mut u8, usize) {
        // SAFETY: the instance is executing, so its memory is live.
        raw_mem(unsafe { (self.0.image.memory)(self.0.inst) })
    }

    fn mem_grow(&mut self, pages: u64) -> Option<u64> {
        // SAFETY: as above. wasm2c permits growth from inside an import.
        grow(unsafe { (self.0.image.memory)(self.0.inst) }, pages)
    }

    fn stack_pointer(&mut self) -> u32 {
        // SAFETY: the address of this instance's `__stack_pointer` global.
        unsafe { *(self.0.image.stack_pointer)(self.0.inst) }
    }

    fn set_stack_pointer(&mut self, value: u32) {
        // SAFETY: as above.
        unsafe { *(self.0.image.stack_pointer)(self.0.inst) = value }
    }

    fn start_unwind(&mut self, data: u32) {
        // SAFETY: as above. The guest returns out of `_start` once this import returns.
        unsafe { (self.0.image.asy_start_unwind)(self.0.inst, data) }
    }

    fn stop_rewind(&mut self) {
        // SAFETY: as above, from inside the import the rewind was heading for.
        unsafe { (self.0.image.asy_stop_rewind)(self.0.inst) }
    }
}

/// Run one import handler, with the kernel's context and a [`Suspend`] over this instance.
///
/// # Safety
///
/// `env` must be the pointer the glue was given at instantiation, and the call must come from
/// inside the guest, which is the only place the generated code calls an import from.
unsafe fn with<R>(env: *mut c_void, body: impl FnOnce(&mut Ctx, &mut dyn Suspend) -> R) -> R {
    // SAFETY: the caller guarantees `env` is our boxed `Env`, and `ctx` is the kernel's
    // per-process state, which it keeps alive for longer than the instance and does not touch
    // while the guest is running.
    let env = unsafe { &mut *env.cast::<Env>() };
    let ctx = unsafe { &mut *env.ctx.cast::<Ctx>() };
    let mut suspend = AotSuspend(env);
    body(ctx, &mut suspend)
}

// The six imports of the `wasmux` module. The glue forwards each one here by name; every
// decision about what they mean lives in `kernel::imports`, shared with the interpreter.

/// # Safety
/// Called only by the generated code, with the env pointer from instantiation.
#[unsafe(no_mangle)]
unsafe extern "C" fn rs_syscall(env: *mut c_void, number: u32, args: u32) -> u32 {
    unsafe { with(env, |ctx, s| imports::syscall_entry(ctx, s, number, args)) }
}

/// # Safety
/// As [`rs_syscall`].
#[unsafe(no_mangle)]
unsafe extern "C" fn rs_setjmp(env: *mut c_void, jmp_buf: u32) -> u32 {
    unsafe { with(env, |ctx, s| imports::setjmp_entry(ctx, s, jmp_buf)) }
}

/// # Safety
/// As [`rs_syscall`].
#[unsafe(no_mangle)]
unsafe extern "C" fn rs_longjmp(env: *mut c_void, jmp_buf: u32, value: u32) {
    unsafe {
        with(env, |ctx, s| {
            imports::longjmp_entry(ctx, s, jmp_buf, value as i32)
        })
    }
}

/// # Safety
/// As [`rs_syscall`].
#[unsafe(no_mangle)]
unsafe extern "C" fn rs_init(env: *mut c_void, signal_flag: u32) {
    unsafe { with(env, |ctx, _| imports::init_entry(ctx, signal_flag)) }
}

/// # Safety
/// As [`rs_syscall`].
#[unsafe(no_mangle)]
unsafe extern "C" fn rs_args(env: *mut c_void, buffer: u32, capacity: u32) -> u32 {
    unsafe { with(env, |ctx, s| imports::args_entry(ctx, s, buffer, capacity)) }
}

/// # Safety
/// As [`rs_syscall`].
#[unsafe(no_mangle)]
unsafe extern "C" fn rs_sigfetch(env: *mut c_void, out: u32) -> u32 {
    unsafe { with(env, |ctx, s| imports::sigfetch_entry(ctx, s, out)) }
}
