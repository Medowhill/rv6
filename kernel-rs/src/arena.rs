use crate::list::*;
use crate::spinlock::{Spinlock, SpinlockGuard};
use core::marker::PhantomData;
use core::mem::{self, ManuallyDrop};
use core::ops::Deref;
use core::ptr::{self, NonNull};
use core::pin::Pin;
use pin_project::pin_project;

/// A homogeneous memory allocator, equipped with the box type representing an allocation.
pub trait Arena: Sized {
    /// The value type of the allocator.
    type Data;

    /// The object handle type of the allocator.
    type Handle<'s>;

    /// The guard type for arena.
    type Guard<'s>;

    /// Find or alloc.
    fn find_or_alloc_handle<C: Fn(&Self::Data) -> bool, N: FnOnce(&mut Self::Data)>(
        &self,
        c: C,
        n: N,
    ) -> Option<Self::Handle<'_>>;

    fn find_or_alloc<C: Fn(&Self::Data) -> bool, N: FnOnce(&mut Self::Data)>(
        &self,
        c: C,
        n: N,
    ) -> Option<Rc<'_, Self, &Self>> {
        let inner = self.find_or_alloc_handle(c, n)?;
        // It is safe becuase inner has been allocated from self.
        Some(unsafe { Rc::from_unchecked(self, inner) })
    }

    /// Failable allocation.
    fn alloc_handle<F: FnOnce(&mut Self::Data)>(&self, f: F) -> Option<Self::Handle<'_>>;

    fn alloc<F: FnOnce(&mut Self::Data)>(&self, f: F) -> Option<Rc<'_, Self, &Self>> {
        let inner = self.alloc_handle(f)?;
        // It is safe becuase inner has been allocated from self.
        Some(unsafe { Rc::from_unchecked(self, inner) })
    }

    /// Duplicate a given handle, and increase the reference count.
    ///
    /// # Safety
    ///
    /// `handle` must be allocated from `self`.
    unsafe fn dup<'s>(&self, handle: &Self::Handle<'s>) -> Self::Handle<'s>;

    /// Deallocate a given handle, and finalize the referred object if there are
    /// no more handles.
    ///
    /// # Safety
    ///
    /// `handle` must be allocated from `self`.
    unsafe fn dealloc(&self, handle: Self::Handle<'_>);

    fn reacquire_after<'s, 'g: 's, F, R: 's>(guard: &'s mut Self::Guard<'g>, f: F) -> R
    where
        F: FnOnce() -> R;
}

pub trait ArenaObject {
    fn finalize<'s, A: Arena>(&'s mut self, guard: &'s mut A::Guard<'_>);
}

pub struct ArrayEntry<T> {
    refcnt: usize,
    data: T,
}

/// A homogeneous memory allocator equipped with reference counts.
pub struct ArrayArena<T, const CAPACITY: usize> {
    entries: [ArrayEntry<T>; CAPACITY],
}

/// # Safety
///
/// `ptr` is a valid pointer to `ArrayEntry<T>` and has lifetime `'s`.
pub struct ArrayPtr<'s, T> {
    ptr: NonNull<ArrayEntry<T>>,
    _marker: PhantomData<&'s T>,
}

impl<'s, T> ArrayPtr<'s, T> {
    /// # Safety
    ///
    /// `ptr` should be a valid pointer to `ArrayEntry<T>` and have lifetime `'s`.
    unsafe fn new(ptr: NonNull<ArrayEntry<T>>) -> ArrayPtr<'s, T> {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }
}

#[pin_project]
#[repr(C)]
pub struct MruEntry<T> {
    #[pin]
    list_entry: ListEntry,
    refcnt: usize,
    data: T,
}

/// A homogeneous memory allocator equipped with reference counts.
#[pin_project]
pub struct MruArena<T, const CAPACITY: usize> {
    entries: [MruEntry<T>; CAPACITY],
    #[pin]
    head: ListEntry,
}

/// # Safety
///
/// TODO
pub struct MruPtr<T> {
    ptr: NonNull<MruEntry<T>>,
    _marker: PhantomData<T>,
}

/// # Safety
///
/// `inner` is allocated from `tag`
pub struct Rc<'s, A: Arena, T: Deref<Target = A>> {
    tag: T,
    inner: ManuallyDrop<A::Handle<'s>>,
}

impl<T> ArrayEntry<T> {
    pub const fn new(data: T) -> Self {
        Self { refcnt: 0, data }
    }
}

impl<T, const CAPACITY: usize> ArrayArena<T, CAPACITY> {
    // TODO(https://github.com/kaist-cp/rv6/issues/371): unsafe...
    pub const fn new(entries: [ArrayEntry<T>; CAPACITY]) -> Self {
        Self { entries }
    }
}

impl<T> Deref for ArrayPtr<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // It is safe because of the invariant.
        unsafe { &self.ptr.as_ref().data }
    }
}

impl<T> Drop for ArrayPtr<'_, T> {
    fn drop(&mut self) {
        // HACK(@efenniht): we really need linear type here:
        // https://github.com/rust-lang/rfcs/issues/814
        panic!("ArrayPtr must never drop: use ArrayArena::dealloc instead.");
    }
}

impl<T: 'static + ArenaObject + Unpin, const CAPACITY: usize> Arena for Spinlock<ArrayArena<T, CAPACITY>> {
    type Data = T;
    type Handle<'s> = ArrayPtr<'s, T>;
    type Guard<'s> = SpinlockGuard<'s, ArrayArena<T, CAPACITY>>;

    fn find_or_alloc_handle<C: Fn(&Self::Data) -> bool, N: FnOnce(&mut Self::Data)>(
        &self,
        c: C,
        n: N,
    ) -> Option<Self::Handle<'_>> {
        let mut this = self.lock();

        let mut empty: *mut ArrayEntry<T> = ptr::null_mut();
        for entry in &mut this.entries {
            if entry.refcnt != 0 {
                if c(&entry.data) {
                    entry.refcnt += 1;
                    // It is safe because entry is a part of self, whose lifetime is 's.
                    return Some(unsafe { ArrayPtr::new(NonNull::from(entry)) });
                }
            } else if empty.is_null() {
                empty = entry;
                break;
            }
        }

        if empty.is_null() {
            return None;
        }

        // It is safe because empty is a one of this.entries.
        let entry = unsafe { &mut *empty };
        entry.refcnt = 1;
        n(&mut entry.data);
        // It is safe because entry is a part of self, whose lifetime is 's.
        Some(unsafe { ArrayPtr::new(NonNull::from(entry)) })
    }

    fn alloc_handle<F: FnOnce(&mut Self::Data)>(&self, f: F) -> Option<Self::Handle<'_>> {
        let mut this = self.lock();

        for entry in &mut this.entries {
            if entry.refcnt == 0 {
                entry.refcnt = 1;
                f(&mut entry.data);
                // It is safe because entry is a part of self, whose lifetime is 's.
                return Some(unsafe { ArrayPtr::new(NonNull::from(entry)) });
            }
        }

        None
    }

    /// # Safety
    ///
    /// `handle` must be allocated from `self`.
    unsafe fn dup<'s>(&self, handle: &Self::Handle<'s>) -> Self::Handle<'s> {
        let mut _this = self.lock();

        // TODO(https://github.com/kaist-cp/rv6/issues/369)
        // Make a ArrayArena trait and move this there.
        // It is safe becuase of the invariant of ArrayPtr.
        unsafe { (*handle.ptr.as_ptr()).refcnt += 1 };
        Self::Handle::<'s> {
            ptr: handle.ptr,
            _marker: PhantomData,
        }
    }

    /// # Safety
    ///
    /// `handle` must be allocated from `self`.
    unsafe fn dealloc(&self, mut handle: Self::Handle<'_>) {
        let mut this = self.lock();

        // It is safe becuase of the invariant of ArrayPtr.
        let entry = unsafe { handle.ptr.as_mut() };
        if entry.refcnt == 1 {
            entry.data.finalize::<Self>(&mut this);
        }

        entry.refcnt -= 1;
        mem::forget(handle);
    }

    fn reacquire_after<'s, 'g: 's, F, R: 's>(guard: &'s mut Self::Guard<'g>, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        guard.reacquire_after(f)
    }
}

impl<T> MruEntry<T> {
    pub const fn new(data: T) -> Self {
        Self {
            refcnt: 0,
            data,
            list_entry: unsafe { ListEntry::new() },
        }
    }
}

impl<T, const CAPACITY: usize> MruArena<T, CAPACITY> {
    // TODO(https://github.com/kaist-cp/rv6/issues/371): unsafe...
    pub const fn new(entries: [MruEntry<T>; CAPACITY]) -> Self {
        Self {
            entries,
            head: unsafe { ListEntry::new() },
        }
    }

    pub fn init(self: Pin<&mut Self>) {
        let mut this = self.project();

        this.head.as_mut().init();
        // TODO: Pin of array's element?
        for entry in this.entries {
            unsafe {
                // Safe since `MruEntry` is accessed only through `Pin`.
                let entry = Pin::new_unchecked(&mut entry.list_entry);
                this.head.as_mut().prepend(entry);
            }
        }
    }
}

impl<T> Deref for MruPtr<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &self.ptr.as_ref().data }
    }
}

impl<T> Drop for MruPtr<T> {
    fn drop(&mut self) {
        // HACK(@efenniht): we really need linear type here:
        // https://github.com/rust-lang/rfcs/issues/814
        panic!("MruPtr must never drop: use MruArena::dealloc instead.");
    }
}

impl<T: 'static + ArenaObject, const CAPACITY: usize> Spinlock<MruArena<T, CAPACITY>> {
    // TODO(https://github.com/kaist-cp/rv6/issues/369)
    // A workarond for https://github.com/Gilnaa/memoffset/issues/49.
    // Assumes `list_entry` is located at the beginning of `MruEntry`.
    const LIST_ENTRY_OFFSET: usize = 0;
    // const LIST_ENTRY_OFFSET: usize = offset_of!(MruEntry<T>, list_entry);
}

impl<T: 'static + ArenaObject, const CAPACITY: usize> Arena for Spinlock<MruArena<T, CAPACITY>> {
    type Data = T;
    type Handle<'s> = MruPtr<T>;
    type Guard<'s> = SpinlockGuard<'s, MruArena<T, CAPACITY>>;

    fn find_or_alloc_handle<'s, C: Fn(&Self::Data) -> bool, N: FnOnce(&mut Self::Data)>(
        &'s self,
        c: C,
        n: N,
    ) -> Option<Self::Handle<'s>> {
        let this = self.lock();

        let mut list_entry = this.head.next();
        let mut empty = ptr::null_mut();
        while list_entry as *const _ != &this.head as *const _ {
            let entry = unsafe {
                &mut *((list_entry as *const _ as usize - Self::LIST_ENTRY_OFFSET)
                    as *mut MruEntry<T>)
            };
            if c(&entry.data) {
                entry.refcnt += 1;
                return Some(Self::Handle::<'s> {
                    ptr: NonNull::from(entry),
                    _marker: PhantomData,
                });
            } else if entry.refcnt == 0 {
                empty = entry;
            }
            list_entry = list_entry.next();
        }

        if empty.is_null() {
            return None;
        }

        let entry = unsafe { &mut *empty };
        entry.refcnt = 1;
        n(&mut entry.data);
        Some(Self::Handle::<'s> {
            ptr: NonNull::from(entry),
            _marker: PhantomData,
        })
    }

    fn alloc_handle<'s, F: FnOnce(&mut Self::Data)>(&'s self, f: F) -> Option<Self::Handle<'s>> {
        let this = self.lock();

        let mut list_entry = this.head.prev();
        while list_entry as *const _ != &this.head as *const _ {
            let entry = unsafe {
                &mut *((list_entry as *const _ as usize - Self::LIST_ENTRY_OFFSET)
                    as *mut MruEntry<T>)
            };
            if entry.refcnt == 0 {
                entry.refcnt = 1;
                f(&mut entry.data);
                return Some(Self::Handle::<'s> {
                    ptr: NonNull::from(entry),
                    _marker: PhantomData,
                });
            }
            list_entry = list_entry.prev();
        }

        None
    }

    /// # Safety
    ///
    /// `handle` must be allocated from `self`.
    unsafe fn dup<'s>(&self, handle: &Self::Handle<'s>) -> Self::Handle<'s> {
        let mut _this = self.lock();

        // TODO(https://github.com/kaist-cp/rv6/issues/369)
        // Make a MruArena trait and move this there.
        unsafe { (*handle.ptr.as_ptr()).refcnt += 1 };
        Self::Handle::<'s> {
            ptr: handle.ptr,
            _marker: PhantomData,
        }
    }

    /// # Safety
    ///
    /// `handle` must be allocated from `self`.
    unsafe fn dealloc(&self, mut handle: Self::Handle<'_>) {
        let mut this = self.lock();

        // Safe since `MruEntry` is accessed only through `Pin`.
        let mut entry = unsafe { Pin::new_unchecked(handle.ptr.as_mut()).project() };
        if *entry.refcnt == 1 {
            entry.data.finalize::<Self>(&mut this);
        }
        *entry.refcnt -= 1;

        if *entry.refcnt == 0 {
            entry.list_entry.as_mut().remove();
            this.get_pin().project().head.prepend(entry.list_entry);
        }

        mem::forget(handle);
    }

    fn reacquire_after<'s, 'g: 's, F, R: 's>(guard: &'s mut Self::Guard<'g>, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        guard.reacquire_after(f)
    }
}

impl<'s, A: Arena, T: Deref<Target = A>> Deref for Rc<'s, A, T> {
    type Target = A::Handle<'s>;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<'s, A: Arena, T: Deref<Target = A>> Drop for Rc<'s, A, T> {
    fn drop(&mut self) {
        // It is safe because inner is allocated from tag.
        unsafe { self.tag.dealloc(ManuallyDrop::take(&mut self.inner)) };
    }
}

impl<'s, A: Arena, T: Deref<Target = A>> Rc<'s, A, T> {
    /// # Safety
    ///
    /// `inner` must be allocated from `tag`
    pub unsafe fn from_unchecked(tag: T, inner: A::Handle<'s>) -> Self {
        let inner = ManuallyDrop::new(inner);
        Self { tag, inner }
    }
}

impl<'s, A: Arena, T: Clone + Deref<Target = A>> Clone for Rc<'s, A, T> {
    fn clone(&self) -> Self {
        let tag = self.tag.clone();
        // It is safe because inner is allocated from tag.
        let inner = ManuallyDrop::new(unsafe { tag.dup(&self.inner) });
        Self { tag, inner }
    }
}
