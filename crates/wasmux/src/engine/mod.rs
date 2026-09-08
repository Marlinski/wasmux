//! What it takes to run a guest program, and the two ways wasmux does it.
//!
//! A guest program is a `wasm32-linux` module: position-independent code compiled against
//! wasmux's musl, importing six functions from the `wasmux` module (see `docs/ABI.md`) and
//! instrumented with Binaryen's Asyncify pass. Everything the kernel needs from whatever runs
//! it is in [`Instance`], which is deliberately small:
//!
//! * memory, because syscall arguments are pointers into it;
//! * the shadow stack pointer, because Asyncify saves locals but not globals;
//! * the four Asyncify controls, because that is how a process is suspended and resumed;
//! * `_start`, because that is how it is entered.
//!
//! Two backends implement it and behave identically, which the test suite asserts by running
//! the whole corpus through both:
//!
//! | | [`Aot`](aot::Aot) (default) | [`Interp`](interp::Interp) (`interp` feature) |
//! |---|---|---|
//! | how a program runs | translated to C by wasm2c, compiled into the consumer | decoded and interpreted by wasmi |
//! | speed versus native | 2 to 3x | 10 to 20x |
//! | what it needs to build | a prebuilt archive for the consumer's target | nothing but Rust |
//!
//! Both run the same images from `bin/`, and both are fixed at build time: a sandbox's
//! programs come from the layout, not from the [`Vfs`](crate::Vfs), so nothing the guest can
//! write becomes something the guest can execute. [`Backend::load`] is where that would
//! change, and it is deliberately not wired to `execve`.
//!
//! Neither backend needs threads, and neither uses the host stack for guest frames beyond the
//! depth fuse described in `docs/DESIGN.md`.

use crate::errno::Errno;

// A build with no backend at all cannot run anything, and the reason is always one of two
// mistakes. Saying so here beats a link that succeeds and a sandbox that refuses to start.
#[cfg(all(
    not(feature = "interp"),
    not(all(feature = "aot", target_arch = "wasm32", wasmux_aot_linked))
))]
compile_error!(
    "wasmux has no usable backend in this configuration. The `aot` backend needs a wasm32 \
     target and bin/libwasmux-images.a built for it; on any other target, or without the \
     archive, the `interp` feature is what runs the programs. Enable `interp` (it is on by \
     default and costs 98 bytes when the archive is present), or build the archive with \
     toolchain/build-archive.sh. See docs/INTEGRATION.md."
);

// The compiled-in programs are wasm32 objects, so that backend exists only there.
#[cfg(all(feature = "aot", target_arch = "wasm32", wasmux_aot_linked))]
pub(crate) mod aot;
#[cfg(feature = "interp")]
pub(crate) mod interp;

/// The backend for this build: the compiled-in programs where they exist, the interpreter
/// otherwise.
///
/// A `wasm32` build with the `aot` feature and the prebuilt archive present gets the fast
/// path. Everything else, a native test run in particular, gets the interpreter.
pub(crate) fn default_backend() -> std::sync::Arc<dyn Backend> {
    #[cfg(all(feature = "aot", target_arch = "wasm32", wasmux_aot_linked))]
    {
        return std::sync::Arc::new(aot::Aot::new());
    }
    #[cfg(feature = "interp")]
    #[allow(unreachable_code)]
    {
        return std::sync::Arc::new(interp::Interp::new());
    }
    #[allow(unreachable_code)]
    {
        std::sync::Arc::new(NoBackend)
    }
}

/// Used when a build has no backend at all, so the error says so plainly.
pub(crate) struct NoBackend;

impl Backend for NoBackend {
    fn builtin(&self, _name: &str) -> Option<std::sync::Arc<dyn Program>> {
        None
    }
}

/// Why a program could not be loaded or instantiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// No program of that name is available in this build.
    NotFound(String),
    /// The bytes are not a wasm module, or not one this build can run.
    NotWasm,
    /// The module is missing something the ABI requires, named here.
    MissingExport(&'static str),
    /// The module declares an incompatible guest ABI version.
    AbiMismatch {
        /// What the module was built for.
        found: u32,
        /// What this build of wasmux speaks.
        expected: u32,
    },
    /// The engine refused it, with its own words.
    Engine(String),
    /// Instantiating it would exceed the sandbox's memory limit.
    OutOfMemory,
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::NotFound(name) => write!(f, "no such program: {name}"),
            LoadError::NotWasm => write!(f, "not a wasm module"),
            LoadError::MissingExport(name) => write!(f, "module does not export {name}"),
            LoadError::AbiMismatch { found, expected } => {
                write!(
                    f,
                    "guest ABI {found} is not the {expected} this build speaks"
                )
            }
            LoadError::Engine(msg) => write!(f, "{msg}"),
            LoadError::OutOfMemory => write!(f, "out of memory"),
        }
    }
}

impl std::error::Error for LoadError {}

impl LoadError {
    /// What the guest's `execve` should report.
    #[allow(dead_code)]
    pub(crate) fn errno(&self) -> Errno {
        match self {
            LoadError::NotFound(_) => Errno::NOENT,
            LoadError::OutOfMemory => Errno::NOMEM,
            _ => Errno::NOEXEC,
        }
    }
}

/// How a call into the guest ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    /// `_start` returned. Either the program is finished or an import unwound its stack; the
    /// kernel tells them apart by what the import recorded.
    Returned,
    /// The guest trapped: out of bounds, `unreachable`, bad indirect call, depth fuse. The
    /// process is dead; nothing else is affected.
    Trap(TrapKind),
}

/// Why a guest trapped, in the terms a wait status can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrapKind {
    /// Memory access outside the guest's own linear memory.
    OutOfBounds,
    /// Call depth past the fuse, which exists so that this is a process death rather than a
    /// stack overflow of the whole component.
    Exhaustion,
    /// `unreachable`, a failed assertion, an unrepresentable conversion, division by zero.
    Fault,
}

impl TrapKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TrapKind::OutOfBounds => "out of bounds memory access",
            TrapKind::Exhaustion => "call stack exhausted",
            TrapKind::Fault => "illegal instruction",
        }
    }
}

/// A guest process's execution context: its memory, its stack pointer, its Asyncify state.
///
/// One instance is one process. Dropping it frees the process's memory.
pub(crate) trait Instance {
    /// Base pointer and length of the guest's linear memory.
    ///
    /// Refetched on every use, because growing may move it. The kernel never holds the result
    /// across a [`Instance::mem_grow`].
    fn mem(&self) -> (*mut u8, usize);

    /// Current size in bytes.
    fn mem_size(&self) -> u64;

    /// Add `pages` 64 KiB pages, returning the previous page count, or `None` if refused.
    fn mem_grow(&mut self, pages: u64) -> Option<u64>;

    /// The `__stack_pointer` global. Asyncify does not save globals, so the kernel keeps it
    /// alongside each `jmp_buf` and restores it on a `longjmp`.
    fn stack_pointer(&self) -> u32;

    /// Set the `__stack_pointer` global.
    fn set_stack_pointer(&mut self, value: u32);

    /// Call `_start`, or resume it if a rewind is in progress.
    fn run_start(&mut self) -> Exit;

    /// Begin saving the call stack into the guest's Asyncify area at `data`.
    fn asy_start_unwind(&mut self, data: u32);
    /// Finish saving.
    fn asy_stop_unwind(&mut self);
    /// Begin restoring the call stack from `data`.
    fn asy_start_rewind(&mut self, data: u32);
    /// Finish restoring. Called from inside the import that is being resumed.
    fn asy_stop_rewind(&mut self);
}

/// A loaded program, ready to be instantiated as many times as there are processes running it.
pub(crate) trait Program: Send + Sync {
    /// Create one process's execution context.
    ///
    /// `ctx` is an opaque pointer the backend must hand back to every import call; it is the
    /// kernel's per-process state and outlives the instance.
    fn instantiate(&self, ctx: *mut core::ffi::c_void) -> Result<Box<dyn Instance>, LoadError>;
}

/// Where programs come from.
pub(crate) trait Backend: Send + Sync {
    /// Look up a program that is compiled into this build, by name.
    fn builtin(&self, name: &str) -> Option<std::sync::Arc<dyn Program>>;

    /// Load a program from a module's bytes, if this backend can.
    ///
    /// The kernel does not call this: a sandbox's programs come from the layout, so nothing
    /// the guest can write becomes something it can execute. It exists because the
    /// interpreter builds its own built-ins through it, and because it is the one place an
    /// integrator who *wants* loadable programs would have to change.
    #[allow(dead_code)]
    fn load(&self, name: &str, bytes: &[u8]) -> Result<std::sync::Arc<dyn Program>, LoadError> {
        let _ = (name, bytes);
        Err(LoadError::NotWasm)
    }

    /// Names of the programs compiled into this build.
    #[allow(dead_code)]
    fn builtin_names(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

/// The guest ABI version this build of wasmux speaks. Bumped when the syscall table, the
/// import list or the argument layout changes; see `docs/ABI.md`.
pub const ABI_VERSION: u32 = 1;

/// The guest as seen from *inside* one of its own import calls.
///
/// [`Instance`] is the scheduler's view, taken between calls into the guest. This is the
/// other side: while a guest is executing, an import handler holds whatever calling context
/// the backend requires (wasmi hands it a `Caller`; the AOT backend a pair of pointers), and
/// only these five operations are needed through it. Keeping them apart is what lets the
/// syscall layer be one piece of generic code rather than one per backend.
pub(crate) trait Suspend {
    /// Base pointer and length of the guest's memory.
    fn mem(&mut self) -> (*mut u8, usize);
    /// Add `pages` 64 KiB pages, returning the previous page count, or `None` if refused.
    /// This is how `brk` and `mmap` are served, so it must work from inside an import.
    fn mem_grow(&mut self, pages: u64) -> Option<u64>;
    /// The `__stack_pointer` global.
    fn stack_pointer(&mut self) -> u32;
    /// Set the `__stack_pointer` global.
    fn set_stack_pointer(&mut self, value: u32);
    /// Save the call stack into the Asyncify area at `data` and return out of the guest.
    fn start_unwind(&mut self, data: u32);
    /// Stop a rewind that has just reached this import.
    fn stop_rewind(&mut self);
}
