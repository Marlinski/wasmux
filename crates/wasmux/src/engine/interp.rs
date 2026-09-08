//! The interpreter backend: programs are run by [wasmi], inside this module.
//!
//! Slower than compiling them in, and the reason to have it anyway is that it needs nothing
//! but Rust. Native tests use it, so the whole corpus runs under `cargo test` with no C
//! toolchain in sight, and it can execute a program that was never built into the consumer,
//! which the compiled-in backend cannot.
//!
//! [wasmi]: https://docs.rs/wasmi

use super::{Backend, Exit, Instance, LoadError, Program, Suspend, TrapKind};
use crate::kernel::imports;
use crate::kernel::Ctx;
use std::ffi::c_void;
use std::sync::Arc;
use wasmi::{Caller, Engine, Extern, Func, Global, Linker, Memory, Module, Store, Val};

/// Programs the interpreter can run, keyed by name.
pub(crate) struct Interp {
    engine: Engine,
    linker: Arc<Linker<Data>>,
}

/// What the store carries: the kernel's context pointer and the handles an import needs.
struct Data {
    ctx: *mut c_void,
    memory: Option<Memory>,
    stack_pointer: Option<Global>,
    asyncify: Option<Asyncify>,
}

/// The four Asyncify controls. `Func` is `Copy`, so an import can take them out of the store
/// data and then use the store to call them.
#[derive(Clone, Copy)]
struct Asyncify {
    start_unwind: Func,
    stop_unwind: Func,
    start_rewind: Func,
    stop_rewind: Func,
}

/// Guest call depth the interpreter allows. Chosen to fit a two-megabyte host stack even in
/// an unoptimized build, which is what a test harness gives a thread.
const DEFAULT_MAX_DEPTH: usize = 2_048;

impl Interp {
    pub(crate) fn new() -> Interp {
        let mut config = wasmi::Config::default();
        // A shell recurses: its parser, its expansion, and BusyBox own applets. The default
        // of a thousand frames is not enough, and the cost of a deeper limit is only the
        // memory a deep stack would use anyway.
        // How deep a guest may recurse before it is stopped with a trap of its own rather than
        // taking the host's stack with it. The interpreter uses host stack per guest frame, so
        // this is a real ceiling and not a formality; `WASMUX_MAX_DEPTH` moves it for anyone
        // whose programs legitimately go deeper.
        let depth = std::env::var("WASMUX_MAX_DEPTH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_DEPTH);
        config.set_max_recursion_depth(depth);
        let engine = Engine::new(&config);
        let mut linker: Linker<Data> = Linker::new(&engine);
        install(&mut linker);
        Interp {
            engine,
            linker: Arc::new(linker),
        }
    }
}

/// Bind the six imports of the guest ABI.
fn install(linker: &mut Linker<Data>) {
    // Failures here would mean two imports with the same name, which cannot happen.
    let _ = linker.func_wrap(
        "wasmux",
        "syscall",
        |mut caller: Caller<'_, Data>, number: i32, args: i32| -> i32 {
            with(&mut caller, |ctx, suspend| {
                imports::syscall_entry(ctx, suspend, number as u32, args as u32) as i32
            })
        },
    );
    let _ = linker.func_wrap(
        "wasmux",
        "setjmp",
        |mut caller: Caller<'_, Data>, env: i32| -> i32 {
            with(&mut caller, |ctx, suspend| {
                imports::setjmp_entry(ctx, suspend, env as u32) as i32
            })
        },
    );
    let _ = linker.func_wrap(
        "wasmux",
        "longjmp",
        |mut caller: Caller<'_, Data>, env: i32, value: i32| {
            with(&mut caller, |ctx, suspend| {
                imports::longjmp_entry(ctx, suspend, env as u32, value)
            });
        },
    );
    let _ = linker.func_wrap(
        "wasmux",
        "init",
        |mut caller: Caller<'_, Data>, flag: i32| {
            with(&mut caller, |ctx, _| imports::init_entry(ctx, flag as u32));
        },
    );
    let _ = linker.func_wrap(
        "wasmux",
        "args",
        |mut caller: Caller<'_, Data>, buffer: i32, capacity: i32| -> i32 {
            with(&mut caller, |ctx, suspend| {
                imports::args_entry(ctx, suspend, buffer as u32, capacity as u32) as i32
            })
        },
    );
    let _ = linker.func_wrap(
        "wasmux",
        "sigfetch",
        |mut caller: Caller<'_, Data>, out: i32| -> i32 {
            with(&mut caller, |ctx, suspend| {
                imports::sigfetch_entry(ctx, suspend, out as u32) as i32
            })
        },
    );
}

/// Run one import handler with the kernel's context and a [`Suspend`] over this caller.
fn with<R>(caller: &mut Caller<'_, Data>, body: impl FnOnce(&mut Ctx, &mut dyn Suspend) -> R) -> R {
    let pointer = caller.data().ctx;
    let mut suspend = CallerSuspend { caller };
    // SAFETY: the pointer was handed to `instantiate` by the kernel, which owns the `Ctx` for
    // longer than the instance, and an import only runs while that process is scheduled.
    let ctx = unsafe { &mut *(pointer as *mut Ctx) };
    body(ctx, &mut suspend)
}

struct CallerSuspend<'a, 'b> {
    caller: &'a mut Caller<'b, Data>,
}

impl Suspend for CallerSuspend<'_, '_> {
    fn mem(&mut self) -> (*mut u8, usize) {
        match self.caller.data().memory {
            Some(memory) => {
                let bytes = memory.data_mut(&mut *self.caller);
                (bytes.as_mut_ptr(), bytes.len())
            }
            None => (core::ptr::NonNull::dangling().as_ptr(), 0),
        }
    }

    fn mem_grow(&mut self, pages: u64) -> Option<u64> {
        let memory = self.caller.data().memory?;
        memory.grow(&mut *self.caller, pages).ok()
    }

    fn stack_pointer(&mut self) -> u32 {
        match self.caller.data().stack_pointer {
            Some(global) => global.get(&*self.caller).i32().unwrap_or(0) as u32,
            None => 0,
        }
    }

    fn set_stack_pointer(&mut self, value: u32) {
        if let Some(global) = self.caller.data().stack_pointer {
            let _ = global.set(&mut *self.caller, Val::I32(value as i32));
        }
    }

    fn start_unwind(&mut self, data: u32) {
        if let Some(asyncify) = self.caller.data().asyncify {
            let _ =
                asyncify
                    .start_unwind
                    .call(&mut *self.caller, &[Val::I32(data as i32)], &mut []);
        }
    }

    fn stop_rewind(&mut self) {
        if let Some(asyncify) = self.caller.data().asyncify {
            let _ = asyncify.stop_rewind.call(&mut *self.caller, &[], &mut []);
        }
    }
}

impl Backend for Interp {
    fn builtin(&self, name: &str) -> Option<Arc<dyn Program>> {
        let bytes = crate::programs::image_bytes(name)?;
        self.load(name, bytes).ok()
    }

    fn load(&self, _name: &str, bytes: &[u8]) -> Result<Arc<dyn Program>, LoadError> {
        if !bytes.starts_with(b"\0asm") {
            return Err(LoadError::NotWasm);
        }
        let module =
            Module::new(&self.engine, bytes).map_err(|e| LoadError::Engine(e.to_string()))?;
        Ok(Arc::new(InterpProgram {
            engine: self.engine.clone(),
            linker: self.linker.clone(),
            module,
        }))
    }

    fn builtin_names(&self) -> Vec<&'static str> {
        crate::programs::layout().iter().map(|l| l.image).collect()
    }
}

struct InterpProgram {
    engine: Engine,
    linker: Arc<Linker<Data>>,
    module: Module,
}

impl Program for InterpProgram {
    fn instantiate(&self, ctx: *mut c_void) -> Result<Box<dyn Instance>, LoadError> {
        let data = Data {
            ctx,
            memory: None,
            stack_pointer: None,
            asyncify: None,
        };
        let mut store = Store::new(&self.engine, data);
        let instance = self
            .linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| LoadError::Engine(e.to_string()))?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(LoadError::MissingExport("memory"))?;
        let start = instance
            .get_func(&store, "_start")
            .ok_or(LoadError::MissingExport("_start"))?;
        let func = |name: &'static str| {
            instance
                .get_export(&store, name)
                .and_then(Extern::into_func)
                .ok_or(LoadError::MissingExport(name))
        };
        let asyncify = Asyncify {
            start_unwind: func("asyncify_start_unwind")?,
            stop_unwind: func("asyncify_stop_unwind")?,
            start_rewind: func("asyncify_start_rewind")?,
            stop_rewind: func("asyncify_stop_rewind")?,
        };
        let stack_pointer = instance
            .get_export(&store, "__stack_pointer")
            .and_then(Extern::into_global);
        let data = store.data_mut();
        data.memory = Some(memory);
        data.stack_pointer = stack_pointer;
        data.asyncify = Some(asyncify);
        Ok(Box::new(InterpInstance {
            store,
            memory,
            start,
            asyncify,
            stack_pointer,
        }))
    }
}

struct InterpInstance {
    store: Store<Data>,
    memory: Memory,
    start: Func,
    asyncify: Asyncify,
    stack_pointer: Option<Global>,
}

impl Instance for InterpInstance {
    fn mem(&self) -> (*mut u8, usize) {
        let bytes = self.memory.data(&self.store);
        (bytes.as_ptr().cast_mut(), bytes.len())
    }

    fn mem_size(&self) -> u64 {
        self.memory.data(&self.store).len() as u64
    }

    fn mem_grow(&mut self, pages: u64) -> Option<u64> {
        self.memory.grow(&mut self.store, pages).ok()
    }

    fn stack_pointer(&self) -> u32 {
        match self.stack_pointer {
            Some(global) => global.get(&self.store).i32().unwrap_or(0) as u32,
            None => 0,
        }
    }

    fn set_stack_pointer(&mut self, value: u32) {
        if let Some(global) = self.stack_pointer {
            let _ = global.set(&mut self.store, Val::I32(value as i32));
        }
    }

    fn run_start(&mut self) -> Exit {
        match self.start.call(&mut self.store, &[], &mut []) {
            Ok(()) => Exit::Returned,
            Err(error) => Exit::Trap(classify(&error)),
        }
    }

    fn asy_start_unwind(&mut self, data: u32) {
        let _ = self
            .asyncify
            .start_unwind
            .call(&mut self.store, &[Val::I32(data as i32)], &mut []);
    }
    fn asy_stop_unwind(&mut self) {
        let _ = self
            .asyncify
            .stop_unwind
            .call(&mut self.store, &[], &mut []);
    }
    fn asy_start_rewind(&mut self, data: u32) {
        let _ = self
            .asyncify
            .start_rewind
            .call(&mut self.store, &[Val::I32(data as i32)], &mut []);
    }
    fn asy_stop_rewind(&mut self) {
        let _ = self
            .asyncify
            .stop_rewind
            .call(&mut self.store, &[], &mut []);
    }
}

/// Turn a wasmi error into the wait status the guest deserves.
fn classify(error: &wasmi::Error) -> TrapKind {
    let text = error.to_string();
    if text.contains("out of bounds") {
        TrapKind::OutOfBounds
    } else if text.contains("call stack exhausted") || text.contains("stack overflow") {
        TrapKind::Exhaustion
    } else {
        TrapKind::Fault
    }
}
