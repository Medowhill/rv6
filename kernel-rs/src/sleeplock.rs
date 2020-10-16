//! Sleeping locks
use crate::proc::{myproc, WaitChannel};
use crate::spinlock::{RawSpinlock, Spinlock};
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

pub struct SleepLockGuard<'s, T> {
    lock: &'s SleeplockWIP<T>,
    _marker: PhantomData<*const ()>,
}

// Do not implement Send; lock must be unlocked by the CPU that acquired it.
unsafe impl<'s, T: Sync> Sync for SleepLockGuard<'s, T> {}

/// Long-term locks for processes
pub struct SleeplockWIP<T> {
    spinlock: Spinlock<i32>,
    data: UnsafeCell<T>,
    /// WaitChannel saying spinlock is relased.
    waitchannel: WaitChannel,
}

unsafe impl<T: Send> Sync for SleeplockWIP<T> {}

impl<T> SleeplockWIP<T> {
    pub const fn new(name: &'static str, data: T) -> Self {
        Self {
            spinlock: Spinlock::new(name, -1),
            data: UnsafeCell::new(data),
            waitchannel: WaitChannel::new(),
        }
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }

    // TODO: This should be removed after `WaitChannel::sleep` gets refactored to take
    // `SpinLockGuard`.
    #[allow(clippy::while_immutable_condition)]
    pub unsafe fn lock(&self) -> SleepLockGuard<'_, T> {
        let mut guard = self.spinlock.lock();
        while *guard != -1 {
            self.waitchannel.sleep(guard.raw() as *mut RawSpinlock);
        }
        *guard = (*myproc()).pid;
        SleepLockGuard {
            lock: self,
            _marker: PhantomData,
        }
    }

    /// # Safety
    ///
    /// `self` must not be shared by other threads. Use this function only in the middle of
    /// refactoring.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut_unchecked(&self) -> &mut T {
        &mut *self.data.get()
    }

    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }
}

impl<T> SleepLockGuard<'_, T> {
    pub fn raw(&self) -> usize {
        self.lock as *const _ as usize
    }
}

impl<T> Drop for SleepLockGuard<'_, T> {
    fn drop(&mut self) {
        let mut guard = self.lock.spinlock.lock();
        *guard = -1;
        self.lock.waitchannel.wakeup();
        drop(guard);
    }
}

impl<T> Deref for SleepLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SleepLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

/// Long-term locks for processes
pub struct Sleeplock {
    /// Is the lock held?
    locked: u32,

    /// spinlock protecting this sleep lock
    lk: RawSpinlock,

    /// For debugging:  

    /// Name of lock.
    name: &'static str,

    /// Process holding lock
    pid: i32,

    /// WaitChannel saying lk is relased.
    waitchannel: WaitChannel,
}

impl Sleeplock {
    // TODO: transient measure
    pub const fn zeroed() -> Self {
        Self {
            locked: 0,
            lk: RawSpinlock::zeroed(),
            name: "",
            pid: 0,
            waitchannel: WaitChannel::new(),
        }
    }

    pub unsafe fn new(name: &'static str) -> Self {
        let mut lk = Self::zeroed();

        lk.lk.initlock("sleep lock");
        lk.name = name;
        lk.locked = 0;
        lk.pid = 0;

        lk
    }

    pub fn initlock(&mut self, name: &'static str) {
        (*self).lk.initlock("sleep lock");
        (*self).name = name;
        (*self).locked = 0;
        (*self).pid = 0;
    }

    pub unsafe fn acquire(&mut self) {
        (*self).lk.acquire();
        while (*self).locked != 0 {
            (*self).waitchannel.sleep(&mut (*self).lk);
        }
        (*self).locked = 1;
        (*self).pid = (*myproc()).pid;
        (*self).lk.release();
    }

    pub unsafe fn release(&mut self) {
        (*self).lk.acquire();
        (*self).locked = 0;
        (*self).pid = 0;
        (*self).waitchannel.wakeup();
        (*self).lk.release();
    }

    pub unsafe fn holding(&mut self) -> bool {
        (*self).lk.acquire();
        let r = (*self).locked != 0 && (*self).pid == (*myproc()).pid;
        (*self).lk.release();
        r
    }
}
