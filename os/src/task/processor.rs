//!Implementation of [`Processor`] and Intersection of control flow
//!
//! Here, the continuous operation of user apps in CPU is maintained,
//! the current running state of CPU is recorded,
//! and the replacement and transfer of control flow of different applications are executed.

use super::__switch;
use super::{fetch_task, TaskStatus};
use super::{TaskContext, TaskControlBlock};
use crate::config::MAX_SYSCALL_NUM;
use crate::mm::{MapPermission, PhysPageNum, VPNRange, VirtAddr, VirtPageNum};
use crate::sync::UPSafeCell;
use crate::syscall::TaskInfo;
use crate::timer::get_time_ms;
use crate::trap::TrapContext;
use alloc::sync::Arc;
use lazy_static::*;

/// Processor management structure
pub struct Processor {
    ///The task currently executing on the current processor
    current: Option<Arc<TaskControlBlock>>,

    ///The basic control flow of each core, helping to select and switch process
    idle_task_cx: TaskContext,
}

impl Processor {
    ///Create an empty Processor
    pub fn new() -> Self {
        Self {
            current: None,
            idle_task_cx: TaskContext::zero_init(),
        }
    }

    ///Get mutable reference to `idle_task_cx`
    fn get_idle_task_cx_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_task_cx as *mut _
    }

    ///Get current task in moving semanteme
    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }

    ///Get current task in cloning semanteme
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }

    /// Update current 'Running' task's system call times
    pub fn update_syscall_times(&self, syscall_id: usize) {
        let current = self.current().unwrap();
        let mut inner = current.inner_exclusive_access();
        inner.syscall_times[syscall_id] += 1;
    }

    /// Get current task's ppn by giving vpn
    pub fn get_ppn_by_vpn(&self, vpn: VirtPageNum) -> Option<PhysPageNum> {
        let current = self.current().unwrap();
        let ppn = current
            .inner_exclusive_access()
            .memory_set
            .translate(vpn.into())
            .map(|entry| entry.ppn());
        ppn
    }

    /// Get the current task's info
    pub fn get_task_info(&self, ti: *mut TaskInfo) {
        let current = self.current().unwrap();
        let inner = current.inner_exclusive_access();
        unsafe {
            (*ti).status = inner.task_status;
            (*ti).syscall_times = inner.syscall_times;
            (*ti).time = get_time_ms() - inner.start_time;
        }
    }

    /// Map memory from current 'Running' task's memory set
    pub fn map_task_memory(&self, start_va: VirtAddr, end_va: VirtAddr, port: usize) -> isize {
        let current = self.current().unwrap();
        let mut inner = current.inner_exclusive_access();
        let vpn_range = VPNRange::new(
            VirtPageNum::from(start_va), 
            end_va.ceil(),
        );

        for vpn in vpn_range {
            if let Some(pte) = inner.memory_set.translate(vpn) {
                if pte.is_valid() {
                    error!("[kernel] MMAP: {:?} point to an valid page", vpn);
                    return -1;
                }
            }
        }

        let perm = MapPermission::from_bits((port as u8) << 1).unwrap() | MapPermission::U;
        inner.memory_set.insert_framed_area(start_va, end_va, perm);
        0
    }

    /// Unmap memory from current 'Running' task's memory set
    pub fn unmap_task_memory(&self, start_va: VirtAddr, end_va: VirtAddr) -> isize {
        let current = self.current().unwrap();
        let mut inner = current.inner_exclusive_access();
        let vpn_range = VPNRange::new(
            VirtPageNum::from(start_va), 
            end_va.ceil(),
        );

        for vpn in vpn_range {
            if let Some(pte) = inner.memory_set.translate(vpn) {
                if !pte.is_valid() {
                    error!("[kernel] UNMMAP: {:?} point to an valid page", vpn);
                    return -1;
                }
            }
        }

        inner.memory_set.remove_area_with_start_vpn(vpn_range.get_start());
        0
    }
}

lazy_static! {
    pub static ref PROCESSOR: UPSafeCell<Processor> = unsafe { UPSafeCell::new(Processor::new()) };
}

///The main part of process execution and scheduling
///Loop `fetch_task` to get the process that needs to run, and switch the process through `__switch`
pub fn run_tasks() {
    loop {
        let mut processor = PROCESSOR.exclusive_access();
        if let Some(task) = fetch_task() {
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
            // access coming task TCB exclusively
            let mut task_inner = task.inner_exclusive_access();
            let next_task_cx_ptr = &task_inner.task_cx as *const TaskContext;
            task_inner.task_status = TaskStatus::Running;
            task_inner.update_stride();
            // release coming task_inner manually
            drop(task_inner);
            // release coming task TCB manually
            processor.current = Some(task);
            // release processor manually
            drop(processor);
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
        } else {
            warn!("no tasks available in run_tasks");
        }
    }
}

/// Get current task through take, leaving a None in its place
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().take_current()
}

/// Get a copy of the current task
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().current()
}

/// Get the current user token(addr of page table)
pub fn current_user_token() -> usize {
    let task = current_task().unwrap();
    task.get_user_token()
}

///Get the mutable reference to trap context of current task
pub fn current_trap_cx() -> &'static mut TrapContext {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .get_trap_cx()
}

///Return to idle control flow for new scheduling
pub fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let mut processor = PROCESSOR.exclusive_access();
    let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
    drop(processor);
    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
}

/// Update current 'Running' task's system call times
pub fn update_syscall_times(syscall_id: usize) {
    if syscall_id >= MAX_SYSCALL_NUM {
        return;
    }
    PROCESSOR.exclusive_access().update_syscall_times(syscall_id);
}

/// Get current task's ppn by giving vpn
pub fn ppn_by_vpn(vpn: VirtPageNum) -> Option<PhysPageNum> {
    PROCESSOR.exclusive_access().get_ppn_by_vpn(vpn)
}

/// Get current task's info
pub fn get_task_info(ti: *mut TaskInfo) {
    PROCESSOR.exclusive_access().get_task_info(ti);
}

/// Map memory from current 'Running' task's memory set
pub fn task_mmap(start: usize, len: usize, port: usize) -> isize {
    let start_va = VirtAddr::from(start);
    if !start_va.aligned() {
        error!("Expect the start address to be aligned by page size, but {:#x}", start);
        return -1
    }
    if port > 0b1000 || port == 0 {
        error!("Invalid mmap permission flag: {:#b}", port);
        return -1
    }
    let end_va = VirtAddr::from(start + len);
    PROCESSOR.exclusive_access().map_task_memory(start_va, end_va, port)
}

/// Unmap memory from current 'Running' task's memory set
pub fn task_munmap(start: usize, len: usize) -> isize {
    let start_va = VirtAddr::from(start);
    if !start_va.aligned() {
        error!("Expect the start address to be aligned by page size, but {:#x}", start);
        return -1
    }

    let end_va = VirtAddr::from(start + len);
    PROCESSOR.exclusive_access().unmap_task_memory(start_va, end_va)
}