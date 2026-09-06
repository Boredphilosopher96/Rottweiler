//! Mach FFI is confined here; every returned send right and VM array is owned.
use mach2::{
    exception_types::{EXC_MASK_ALL, EXC_MASK_CORPSE_NOTIFY, EXC_MASK_CRASH},
    kern_return::{KERN_INVALID_ARGUMENT, KERN_SUCCESS, kern_return_t},
    mach_port::mach_port_deallocate,
    port::{MACH_PORT_NULL, mach_port_t},
    task::{
        mach_ports_lookup, mach_ports_register, task_get_exception_ports, task_get_special_port,
        task_set_exception_ports, task_set_special_port, task_threads,
    },
    task_special_ports::{TASK_BOOTSTRAP_PORT, TASK_DEBUG_CONTROL_PORT, TASK_RESOURCE_NOTIFY_PORT},
    thread_act::thread_set_exception_ports,
    traps::mach_task_self,
    vm::mach_vm_deallocate,
};
use std::{io, ptr};

// mach2 omits this SDK function. Its ABI is identical to the task getter with
// the first argument being a thread control name (mach/thread_act.h).
unsafe extern "C" {
    fn thread_get_exception_ports(
        thread: u32,
        mask: u32,
        masks: *mut u32,
        count: *mut u32,
        handlers: *mut u32,
        behaviors: *mut i32,
        flavors: *mut i32,
    ) -> i32;
}

const EXCEPTIONS: u32 = EXC_MASK_ALL | EXC_MASK_CRASH | EXC_MASK_CORPSE_NOTIFY;
const EXCEPTION_SLOTS: usize = 32;

pub(super) fn clear() -> io::Result<()> {
    // SAFETY: mach_task_self obtains the current task's borrowed control name.
    let task = unsafe { mach_task_self() };
    let mut threads = PortArray::new(task);
    check(
        // SAFETY: both output pointers refer to live fields; success transfers the
        // kernel-allocated array and its send rights into threads' ownership.
        unsafe { task_threads(task, &raw mut threads.pointer, &raw mut threads.count) },
        "list worker threads",
    )?;
    if threads.names().len() != 1 {
        return Err(io::Error::other(
            "Mach authority clearing requires one worker thread",
        ));
    }
    let thread_name = threads.names()[0];
    check(
        // SAFETY: a zero-length registration consumes no user memory and replaces
        // every registered slot in this current task with MACH_PORT_NULL.
        unsafe { mach_ports_register(task, ptr::null_mut(), 0) },
        "clear registered ports",
    )?;
    let mut registered = PortArray::new(task);
    check(
        // SAFETY: valid output fields take ownership of the allocated array/rights.
        unsafe { mach_ports_lookup(task, &raw mut registered.pointer, &raw mut registered.count) },
        "verify registered ports",
    )?;
    if registered
        .names()
        .iter()
        .any(|port| *port != MACH_PORT_NULL)
    {
        return Err(io::Error::other("registered Mach authority remains"));
    }
    drop(registered);
    check(
        // SAFETY: scalar current-task name, valid exception mask and null handler.
        unsafe { task_set_exception_ports(task, EXCEPTIONS, MACH_PORT_NULL, 1, 0) },
        "clear task exception ports",
    )?;
    verify_exceptions(task, task, false)?;
    check(
        // SAFETY: this is the sole current thread's valid control name; null clears.
        unsafe { thread_set_exception_ports(thread_name, EXCEPTIONS, MACH_PORT_NULL, 1, 0) },
        "clear thread exception ports",
    )?;
    verify_exceptions(task, thread_name, true)?;
    for which in [
        TASK_BOOTSTRAP_PORT,
        TASK_DEBUG_CONTROL_PORT,
        TASK_RESOURCE_NOTIFY_PORT,
    ] {
        let mut existing = SendRight {
            task,
            name: MACH_PORT_NULL,
        };
        // SAFETY: a current-task slot writes one owned right into a live field.
        let present = unsafe { task_get_special_port(task, which, &raw mut existing.name) };
        // This slot exists only on kernels with CONFIG_PROC_RESOURCE_LIMITS.
        // KERN_INVALID_ARGUMENT from its getter proves the slot is unsupported.
        if which == TASK_RESOURCE_NOTIFY_PORT && present == KERN_INVALID_ARGUMENT {
            continue;
        }
        check(present, "inspect application special port")?;
        drop(existing);
        check(
            // SAFETY: these replaceable current-task slots accept a null send right.
            unsafe { task_set_special_port(task, which, MACH_PORT_NULL) },
            "clear application special port",
        )?;
        let mut port = SendRight {
            task,
            name: MACH_PORT_NULL,
        };
        check(
            // SAFETY: the live output field owns the returned send right.
            unsafe { task_get_special_port(task, which, &raw mut port.name) },
            "verify application special port",
        )?;
        if port.name != MACH_PORT_NULL {
            return Err(io::Error::other(
                "application special Mach authority remains",
            ));
        }
    }
    drop(threads);
    Ok(())
}

fn verify_exceptions(task: mach_port_t, owner: mach_port_t, thread: bool) -> io::Result<()> {
    let mut masks = [0; EXCEPTION_SLOTS];
    let mut handlers = [0; EXCEPTION_SLOTS];
    let mut behaviors = [0; EXCEPTION_SLOTS];
    let mut flavors = [0; EXCEPTION_SLOTS];
    let mut count = u32::try_from(EXCEPTION_SLOTS).map_err(io::Error::other)?;
    // SAFETY: each output buffer has count entries, owner is a live current
    // task/thread right, and the API honors the provided array capacity.
    let result = unsafe {
        if thread {
            thread_get_exception_ports(
                owner,
                EXCEPTIONS,
                masks.as_mut_ptr(),
                &raw mut count,
                handlers.as_mut_ptr(),
                behaviors.as_mut_ptr(),
                flavors.as_mut_ptr(),
            )
        } else {
            task_get_exception_ports(
                owner,
                EXCEPTIONS,
                masks.as_mut_ptr(),
                &raw mut count,
                handlers.as_mut_ptr(),
                behaviors.as_mut_ptr(),
                flavors.as_mut_ptr(),
            )
        }
    };
    check(result, "verify exception ports")?;
    let count = usize::try_from(count).map_err(io::Error::other)?;
    let returned = handlers
        .get(..count)
        .ok_or_else(|| io::Error::other("invalid Mach exception reply count"))?;
    let retained = returned.iter().any(|port| *port != MACH_PORT_NULL);
    for name in returned {
        drop(SendRight { task, name: *name });
    }
    if retained {
        Err(io::Error::other("exception Mach authority remains"))
    } else {
        Ok(())
    }
}

struct SendRight {
    task: mach_port_t,
    name: mach_port_t,
}
impl Drop for SendRight {
    fn drop(&mut self) {
        if self.name != MACH_PORT_NULL {
            // SAFETY: this object owns exactly one send user-reference returned
            // by a Mach getter; releasing it never destroys another owner's ref.
            let _ = unsafe { mach_port_deallocate(self.task, self.name) };
        }
    }
}

struct PortArray {
    task: mach_port_t,
    pointer: *mut mach_port_t,
    count: u32,
}
impl PortArray {
    fn new(task: mach_port_t) -> Self {
        Self {
            task,
            pointer: ptr::null_mut(),
            count: 0,
        }
    }
    fn names(&self) -> &[mach_port_t] {
        if self.pointer.is_null() || self.count == 0 {
            return &[];
        }
        // SAFETY: a successful Mach array getter supplies count initialized
        // entries in its returned VM allocation, kept live until this owner drops.
        unsafe { std::slice::from_raw_parts(self.pointer, self.count as usize) }
    }
}
impl Drop for PortArray {
    fn drop(&mut self) {
        for name in self.names() {
            drop(SendRight {
                task: self.task,
                name: *name,
            });
        }
        if !self.pointer.is_null() {
            let length = u64::from(self.count) * 4;
            // SAFETY: this is exactly the VM allocation transferred by the Mach
            // getter. Its send rights were released before freeing the array.
            let _ = unsafe { mach_vm_deallocate(self.task, self.pointer as usize as u64, length) };
        }
    }
}
fn check(result: kern_return_t, operation: &str) -> io::Result<()> {
    if result == KERN_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{operation}: Mach error {result}"
        )))
    }
}
