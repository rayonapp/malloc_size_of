// Copyright 2016-2017 The Servo Project Developers.
//
// Licensed under the Apache License, Version 2.0
// <http://www.apache.org/licenses/LICENSE-2.0>.
// This file may not be copied, modified, or distributed
// except according to those terms.

//! A crate for measuring the heap usage of data structures in a way that
//! integrates with Firefox's memory reporting, particularly the use of
//! mozjemalloc and DMD. In particular, it has the following features.
//! - It isn't bound to a particular heap allocator.
//! - It provides traits for both "shallow" and "deep" measurement, which gives
//!   flexibility in the cases where the traits can't be used.
//! - It allows for measuring blocks even when only an interior pointer can be
//!   obtained for heap allocations, e.g. `HashSet` and `HashMap`. (This relies
//!   on the heap allocator having suitable support, which mozjemalloc has.)
//! - It allows handling of types like `Rc` and `Arc` by providing traits that
//!   are different to the ones for non-graph structures.
//!
//! Suggested uses are as follows.
//! - When possible, use the `MallocSizeOf` trait. (Deriving support is
//!   provided by the `malloc_size_of_derive` crate.)
//! - If you need an additional synchronization argument, provide a function
//!   that is like the standard trait method, but with the extra argument.
//! - If you need multiple measurements for a type, provide a function named
//!   `add_size_of` that takes a mutable reference to a struct that contains
//!   the multiple measurement fields.
//! - When deep measurement (via `MallocSizeOf`) cannot be implemented for a
//!   type, shallow measurement (via `MallocShallowSizeOf`) in combination with
//!   iteration can be a useful substitute.
//! - `Rc` and `Arc` are always tricky, which is why `MallocSizeOf` is not (and
//!   should not be) implemented for them.
//! - If an `Rc` or `Arc` is known to be a "primary" reference and can always
//!   be measured, it should be measured via the `MallocUnconditionalSizeOf`
//!   trait.
//! - If an `Rc` or `Arc` should be measured only if it hasn't been seen
//!   before, it should be measured via the `MallocConditionalSizeOf` trait.
//! - Using universal function call syntax is a good idea when measuring boxed
//!   fields in structs, because it makes it clear that the Box is being
//!   measured as well as the thing it points to. E.g.
//!   `<Box<_> as MallocSizeOf>::size_of(field, ops)`.
//!

#[cfg(feature = "beach_map")]
extern crate beach_map;
#[cfg(feature = "euclid")]
extern crate euclid;
#[cfg(feature = "hashbrown")]
extern crate hashbrown;
#[cfg(feature = "hibitset")]
extern crate hibitset;
#[cfg(feature = "lyon")]
extern crate lyon;
#[cfg(feature = "rstar")]
extern crate rstar;
#[cfg(feature = "serde")]
extern crate serde;
#[cfg(feature = "serde_bytes")]
extern crate serde_bytes;
#[cfg(feature = "serde_json")]
extern crate serde_json;
#[cfg(feature = "smallbitvec")]
extern crate smallbitvec;
#[cfg(feature = "smallvec")]
extern crate smallvec;
#[cfg(feature = "specs")]
extern crate specs;
#[cfg(feature = "string_cache")]
extern crate string_cache;
#[cfg(feature = "time")]
extern crate time;
#[cfg(feature = "url")]
extern crate url;
#[cfg(feature = "void")]
extern crate void;

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hash};
use std::mem::{align_of, size_of, MaybeUninit};
use std::num::NonZeroUsize;
use std::ops::Range;
use std::ops::{Deref, DerefMut};

#[cfg(not(target_os = "windows"))]
use std::os::raw::c_void;

#[cfg(target_os = "windows")]
extern crate winapi;

#[cfg(target_os = "windows")]
use winapi::ctypes::c_void;

#[cfg(target_os = "windows")]
use winapi::um::heapapi::{GetProcessHeap, HeapSize, HeapValidate};

#[cfg(feature = "serde_bytes")]
use self::serde_bytes::ByteBuf;

#[cfg(feature = "void")]
use self::void::Void;

#[cfg(feature = "hashbrown")]
use hashbrown::HashMap;

#[cfg(feature = "hibitset")]
use hibitset::BitSet;

#[cfg(feature = "specs")]
use specs::prelude::*;

#[cfg(feature = "beach_map")]
use beach_map::{BeachMap, ID};

#[cfg(feature = "rstar")]
use rstar::{RTreeNode, RTreeObject};

#[cfg(feature = "serde_json")]
use serde_json::Value;

/// A C function that takes a pointer to a heap allocation and returns its size.
type VoidPtrToSizeFn = unsafe fn(ptr: *const c_void) -> usize;

/// A closure implementing a stateful predicate on pointers.
type VoidPtrToBoolFnMut = dyn FnMut(*const c_void) -> bool;

// Get the size of a heap block.
///
/// Ideally Rust would expose a function like this in std::rt::heap.
///
/// `unsafe` because the caller must ensure that the pointer is from jemalloc.
/// FIXME: This probably interacts badly with custom allocators:
/// https://doc.rust-lang.org/book/custom-allocators.html
pub unsafe fn heap_size_of<T>(ptr: *const T) -> usize {
    if ptr as usize <= align_of::<T>() {
        0
    } else {
        heap_size_of_impl(ptr as *const c_void)
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_family = "wasm")))]
unsafe fn heap_size_of_impl(ptr: *const c_void) -> usize {
    // The C prototype is `je_malloc_usable_size(JEMALLOC_USABLE_SIZE_CONST void *ptr)`. On some
    // platforms `JEMALLOC_USABLE_SIZE_CONST` is `const` and on some it is empty. But in practice
    // this function doesn't modify the contents of the block that `ptr` points to, so we use
    // `*const c_void` here.
    extern "C" {
        #[cfg_attr(
            any(prefixed_jemalloc, target_os = "ios", target_os = "android"),
            link_name = "je_malloc_usable_size"
        )]
        fn malloc_usable_size(ptr: *const c_void) -> usize;
    }
    malloc_usable_size(ptr)
}

#[cfg(target_os = "macos")]
unsafe fn heap_size_of_impl(ptr: *const c_void) -> usize {
    // On macos The C prototype is `malloc_size(MALLOC_SIZE_CONST void *ptr)`
    extern "C" {
        fn malloc_size(ptr: *const c_void) -> usize;
    }
    malloc_size(ptr)
}

#[cfg(target_family = "wasm")]
unsafe fn heap_size_of_impl(_ptr: *const c_void) -> usize {
    0
}

#[cfg(target_os = "windows")]
unsafe fn heap_size_of_impl(mut ptr: *const c_void) -> usize {
    let heap = GetProcessHeap();

    if HeapValidate(heap, 0, ptr) == 0 {
        ptr = *(ptr as *const *const c_void).offset(-1);
    }

    HeapSize(heap, 0, ptr) as usize
}

/// Operations used when measuring heap usage of data structures.
pub struct MallocSizeOfOps {
    /// A function that returns the size of a heap allocation.
    size_of_op: VoidPtrToSizeFn,

    /// Like `size_of_op`, but can take an interior pointer. Optional because
    /// not all allocators support this operation. If it's not provided, some
    /// memory measurements will actually be computed estimates rather than
    /// real and accurate measurements.
    enclosing_size_of_op: Option<VoidPtrToSizeFn>,

    /// Check if a pointer has been seen before, and remember it for next time.
    /// Useful when measuring `Rc`s and `Arc`s. Optional, because many places
    /// don't need it.
    have_seen_ptr_op: Option<Box<VoidPtrToBoolFnMut>>,
}

impl Default for MallocSizeOfOps {
    fn default() -> Self {
        MallocSizeOfOps {
            size_of_op: heap_size_of,
            enclosing_size_of_op: None,
            have_seen_ptr_op: None,
        }
    }
}

impl MallocSizeOfOps {
    pub fn new(
        size_of: VoidPtrToSizeFn,
        malloc_enclosing_size_of: Option<VoidPtrToSizeFn>,
        have_seen_ptr: Option<Box<VoidPtrToBoolFnMut>>,
    ) -> Self {
        MallocSizeOfOps {
            size_of_op: size_of,
            enclosing_size_of_op: malloc_enclosing_size_of,
            have_seen_ptr_op: have_seen_ptr,
        }
    }

    /// Check if an allocation is empty. This relies on knowledge of how Rust
    /// handles empty allocations, which may change in the future.
    fn is_empty<T: ?Sized>(ptr: *const T) -> bool {
        // The correct condition is this:
        //   `ptr as usize <= ::std::mem::align_of::<T>()`
        // But we can't call align_of() on a ?Sized T. So we approximate it
        // with the following. 256 is large enough that it should always be
        // larger than the required alignment, but small enough that it is
        // always in the first page of memory and therefore not a legitimate
        // address.
        return ptr as *const usize as usize <= 256;
    }

    /// Call `size_of_op` on `ptr`, first checking that the allocation isn't
    /// empty, because some types (such as `Vec`) utilize empty allocations.
    pub unsafe fn malloc_size_of<T: ?Sized>(&self, ptr: *const T) -> usize {
        if MallocSizeOfOps::is_empty(ptr) {
            0
        } else {
            (self.size_of_op)(ptr as *const c_void)
        }
    }

    /// Is an `enclosing_size_of_op` available?
    pub fn has_malloc_enclosing_size_of(&self) -> bool {
        self.enclosing_size_of_op.is_some()
    }

    /// Call `enclosing_size_of_op`, which must be available, on `ptr`, which
    /// must not be empty.
    pub unsafe fn malloc_enclosing_size_of<T>(&self, ptr: *const T) -> usize {
        assert!(!MallocSizeOfOps::is_empty(ptr));
        (self.enclosing_size_of_op.unwrap())(ptr as *const c_void)
    }

    /// Call `have_seen_ptr_op` on `ptr`.
    pub fn have_seen_ptr<T>(&mut self, ptr: *const T) -> bool {
        let have_seen_ptr_op = self
            .have_seen_ptr_op
            .as_mut()
            .expect("missing have_seen_ptr_op");
        have_seen_ptr_op(ptr as *const c_void)
    }
}

/// Trait for measuring the "deep" heap usage of a data structure. This is the
/// most commonly-used of the traits.
pub trait MallocSizeOf {
    /// Measure the heap usage of all descendant heap-allocated structures, but
    /// not the space taken up by the value itself.
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

/// Trait for measuring the "shallow" heap usage of a container.
pub trait MallocShallowSizeOf {
    /// Measure the heap usage of immediate heap-allocated descendant
    /// structures, but not the space taken up by the value itself. Anything
    /// beyond the immediate descendants must be measured separately, using
    /// iteration.
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize;
}

/// Like `MallocSizeOf`, but with a different name so it cannot be used
/// accidentally with derive(MallocSizeOf). For use with types like `Rc` and
/// `Arc` when appropriate (e.g. when measuring a "primary" reference).
pub trait MallocUnconditionalSizeOf {
    /// Measure the heap usage of all heap-allocated descendant structures, but
    /// not the space taken up by the value itself.
    fn unconditional_size_of(&self, ops: &mut MallocSizeOfOps) -> usize;
}

/// `MallocUnconditionalSizeOf` combined with `MallocShallowSizeOf`.
pub trait MallocUnconditionalShallowSizeOf {
    /// `unconditional_size_of` combined with `shallow_size_of`.
    fn unconditional_shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize;
}

/// Like `MallocSizeOf`, but only measures if the value hasn't already been
/// measured. For use with types like `Rc` and `Arc` when appropriate (e.g.
/// when there is no "primary" reference).
pub trait MallocConditionalSizeOf {
    /// Measure the heap usage of all heap-allocated descendant structures, but
    /// not the space taken up by the value itself, and only if that heap usage
    /// hasn't already been measured.
    fn conditional_size_of(&self, ops: &mut MallocSizeOfOps) -> usize;
}

/// `MallocConditionalSizeOf` combined with `MallocShallowSizeOf`.
pub trait MallocConditionalShallowSizeOf {
    /// `conditional_size_of` combined with `shallow_size_of`.
    fn conditional_shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize;
}

#[cfg(not(target_family = "wasm"))]
impl MallocSizeOf for String {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        unsafe { ops.malloc_size_of(self.as_ptr()) }
    }
}

#[cfg(target_family = "wasm")]
impl MallocSizeOf for String {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        self.len() * 8
    }
}

impl<'a, T: ?Sized> MallocSizeOf for &'a T {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        // Zero makes sense for a non-owning reference.
        0
    }
}

#[cfg(not(target_family = "wasm"))]
impl<T: ?Sized> MallocShallowSizeOf for Box<T> {
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        unsafe { ops.malloc_size_of(&**self) }
    }
}

#[cfg(target_family = "wasm")]
impl<T: ?Sized> MallocShallowSizeOf for Box<T> {
    fn shallow_size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

impl<T: MallocSizeOf + ?Sized> MallocSizeOf for Box<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.shallow_size_of(ops) + (**self).size_of(ops)
    }
}

impl<T: MallocSizeOf> MallocSizeOf for [T; 1] {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self[0].size_of(ops)
    }
}

impl<T: MallocSizeOf> MallocSizeOf for [T; 2] {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self[0].size_of(ops) + self[1].size_of(ops)
    }
}

impl<T: MallocSizeOf> MallocSizeOf for [T; 3] {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self[0].size_of(ops) + self[1].size_of(ops)
    }
}

impl<T: MallocSizeOf> MallocSizeOf for [T; 4] {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self[0].size_of(ops) + self[1].size_of(ops)
    }
}

impl<T: MallocSizeOf> MallocSizeOf for [T; 5] {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self[0].size_of(ops) + self[1].size_of(ops)
    }
}

impl<T: MallocSizeOf> MallocSizeOf for [T; 6] {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self[0].size_of(ops) + self[1].size_of(ops)
    }
}

impl MallocSizeOf for () {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

impl<T1, T2> MallocSizeOf for (T1, T2)
where
    T1: MallocSizeOf,
    T2: MallocSizeOf,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.0.size_of(ops) + self.1.size_of(ops)
    }
}

impl<T1, T2, T3> MallocSizeOf for (T1, T2, T3)
where
    T1: MallocSizeOf,
    T2: MallocSizeOf,
    T3: MallocSizeOf,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.0.size_of(ops) + self.1.size_of(ops) + self.2.size_of(ops)
    }
}

impl<T1, T2, T3, T4> MallocSizeOf for (T1, T2, T3, T4)
where
    T1: MallocSizeOf,
    T2: MallocSizeOf,
    T3: MallocSizeOf,
    T4: MallocSizeOf,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.0.size_of(ops) + self.1.size_of(ops) + self.2.size_of(ops) + self.3.size_of(ops)
    }
}

impl<T: MallocSizeOf> MallocSizeOf for Option<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        if let Some(val) = self.as_ref() {
            val.size_of(ops)
        } else {
            0
        }
    }
}

impl<T: MallocSizeOf, E: MallocSizeOf> MallocSizeOf for Result<T, E> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        match *self {
            Ok(ref x) => x.size_of(ops),
            Err(ref e) => e.size_of(ops),
        }
    }
}

impl<T: MallocSizeOf + Copy> MallocSizeOf for std::cell::Cell<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.get().size_of(ops)
    }
}

impl<T: MallocSizeOf> MallocSizeOf for std::cell::RefCell<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.borrow().size_of(ops)
    }
}

impl<'a, B: ?Sized + ToOwned> MallocSizeOf for std::borrow::Cow<'a, B>
where
    B::Owned: MallocSizeOf,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        match *self {
            std::borrow::Cow::Borrowed(_) => 0,
            std::borrow::Cow::Owned(ref b) => b.size_of(ops),
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl<T: MallocSizeOf> MallocSizeOf for [T] {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = unsafe { ops.malloc_size_of(self.as_ptr()) };
        for elem in self.iter() {
            n += elem.size_of(ops);
        }
        n
    }
}

#[cfg(target_family = "wasm")]
impl<T: MallocSizeOf> MallocSizeOf for [T] {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = 0;
        for elem in self.iter() {
            n += elem.size_of(ops);
        }
        n
    }
}

#[cfg(feature = "serde_bytes")]
impl MallocShallowSizeOf for ByteBuf {
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        unsafe { ops.malloc_size_of(self.as_ptr()) }
    }
}

#[cfg(feature = "serde_bytes")]
impl MallocSizeOf for ByteBuf {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = self.shallow_size_of(ops);
        for elem in self.iter() {
            n += elem.size_of(ops);
        }
        n
    }
}

#[cfg(not(target_family = "wasm"))]
impl<T> MallocShallowSizeOf for Vec<T> {
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        unsafe { ops.malloc_size_of(self.as_ptr()) }
    }
}

#[cfg(target_family = "wasm")]
impl<T> MallocShallowSizeOf for Vec<T> {
    fn shallow_size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        self.capacity() * std::mem::size_of::<T>()
    }
}

impl<T: MallocSizeOf> MallocSizeOf for Vec<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = self.shallow_size_of(ops);
        for elem in self.iter() {
            n += elem.size_of(ops);
        }
        n
    }
}

impl<T> MallocShallowSizeOf for std::collections::VecDeque<T> {
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        if ops.has_malloc_enclosing_size_of() {
            if let Some(front) = self.front() {
                // The front element is an interior pointer.
                unsafe { ops.malloc_enclosing_size_of(&*front) }
            } else {
                // This assumes that no memory is allocated when the VecDeque is empty.
                0
            }
        } else {
            // An estimate.
            self.capacity() * size_of::<T>()
        }
    }
}

impl<T: MallocSizeOf> MallocSizeOf for std::collections::VecDeque<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = self.shallow_size_of(ops);
        for elem in self.iter() {
            n += elem.size_of(ops);
        }
        n
    }
}

#[cfg(all(feature = "smallvec", not(target_family = "wasm")))]
impl<A: smallvec::Array> MallocShallowSizeOf for smallvec::SmallVec<A> {
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        if self.spilled() {
            unsafe { ops.malloc_size_of(self.as_ptr()) }
        } else {
            0
        }
    }
}

#[cfg(all(feature = "smallvec", target_family = "wasm"))]
impl<A: smallvec::Array> MallocShallowSizeOf for smallvec::SmallVec<A> {
    fn shallow_size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        if self.spilled() {
            size_of::<A>()
        } else {
            0
        }
    }
}

#[cfg(feature = "smallvec")]
impl<A> MallocSizeOf for smallvec::SmallVec<A>
where
    A: smallvec::Array,
    A::Item: MallocSizeOf,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = self.shallow_size_of(ops);
        for elem in self.iter() {
            n += elem.size_of(ops);
        }
        n
    }
}

impl<T, S> MallocShallowSizeOf for std::collections::HashSet<T, S>
where
    T: Eq + Hash,
    S: BuildHasher,
{
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        if ops.has_malloc_enclosing_size_of() {
            // The first value from the iterator gives us an interior pointer.
            // `ops.malloc_enclosing_size_of()` then gives us the storage size.
            // This assumes that the `HashSet`'s contents (values and hashes)
            // are all stored in a single contiguous heap allocation.
            self.iter()
                .next()
                .map_or(0, |t| unsafe { ops.malloc_enclosing_size_of(t) })
        } else {
            // An estimate.
            self.capacity() * (size_of::<T>() + size_of::<usize>())
        }
    }
}

impl<T, S> MallocSizeOf for std::collections::HashSet<T, S>
where
    T: Eq + Hash + MallocSizeOf,
    S: BuildHasher,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = self.shallow_size_of(ops);
        for t in self.iter() {
            n += t.size_of(ops);
        }
        n
    }
}

impl<K, V, S> MallocShallowSizeOf for std::collections::HashMap<K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        // See the implementation for std::collections::HashSet for details.
        if ops.has_malloc_enclosing_size_of() {
            self.values()
                .next()
                .map_or(0, |v| unsafe { ops.malloc_enclosing_size_of(v) })
        } else {
            self.capacity() * (size_of::<V>() + size_of::<K>() + size_of::<usize>())
        }
    }
}

impl<K, V, S> MallocSizeOf for std::collections::HashMap<K, V, S>
where
    K: Eq + Hash + MallocSizeOf,
    V: MallocSizeOf,
    S: BuildHasher,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = self.shallow_size_of(ops);
        for (k, v) in self.iter() {
            n += k.size_of(ops);
            n += v.size_of(ops);
        }
        n
    }
}

// PhantomData is always 0.
impl<T> MallocSizeOf for std::marker::PhantomData<T> {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

// XXX: we don't want MallocSizeOf to be defined for Rc and Arc. If negative
// trait bounds are ever allowed, this code should be uncommented.
// (We do have a compile-fail test for this:
// rc_arc_must_not_derive_malloc_size_of.rs)
//impl<T> !MallocSizeOf for Arc<T> { }
//impl<T> !MallocShallowSizeOf for Arc<T> { }

/// If a mutex is stored directly as a member of a data type that is being measured,
/// it is the unique owner of its contents and deserves to be measured.
///
/// If a mutex is stored inside of an Arc value as a member of a data type that is being measured,
/// the Arc will not be automatically measured so there is no risk of overcounting the mutex's
/// contents.
impl<T: MallocSizeOf> MallocSizeOf for std::sync::Mutex<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        (*self.lock().unwrap()).size_of(ops)
    }
}

#[cfg(feature = "smallbitvec")]
impl MallocSizeOf for smallbitvec::SmallBitVec {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        if let Some(ptr) = self.heap_ptr() {
            unsafe { ops.malloc_size_of(ptr) }
        } else {
            0
        }
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, Unit> MallocSizeOf for euclid::Length<T, Unit> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.0.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, Src, Dst> MallocSizeOf for euclid::Scale<T, Src, Dst> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.0.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, U> MallocSizeOf for euclid::Point2D<T, U> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.x.size_of(ops) + self.y.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, U> MallocSizeOf for euclid::Rect<T, U> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.origin.size_of(ops) + self.size.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, U> MallocSizeOf for euclid::SideOffsets2D<T, U> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.top.size_of(ops)
            + self.right.size_of(ops)
            + self.bottom.size_of(ops)
            + self.left.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, U> MallocSizeOf for euclid::Size2D<T, U> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.width.size_of(ops) + self.height.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, Src, Dst> MallocSizeOf for euclid::Transform2D<T, Src, Dst> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.m11.size_of(ops)
            + self.m12.size_of(ops)
            + self.m21.size_of(ops)
            + self.m22.size_of(ops)
            + self.m31.size_of(ops)
            + self.m32.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, Src, Dst> MallocSizeOf for euclid::Transform3D<T, Src, Dst> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.m11.size_of(ops)
            + self.m12.size_of(ops)
            + self.m13.size_of(ops)
            + self.m14.size_of(ops)
            + self.m21.size_of(ops)
            + self.m22.size_of(ops)
            + self.m23.size_of(ops)
            + self.m24.size_of(ops)
            + self.m31.size_of(ops)
            + self.m32.size_of(ops)
            + self.m33.size_of(ops)
            + self.m34.size_of(ops)
            + self.m41.size_of(ops)
            + self.m42.size_of(ops)
            + self.m43.size_of(ops)
            + self.m44.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf, U> MallocSizeOf for euclid::Vector2D<T, U> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.x.size_of(ops) + self.y.size_of(ops)
    }
}

#[cfg(feature = "euclid")]
impl<T: MallocSizeOf> MallocSizeOf for euclid::Angle<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.radians.size_of(ops)
    }
}

#[cfg(feature = "void")]
impl MallocSizeOf for Void {
    #[inline]
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        void::unreachable(*self)
    }
}

#[cfg(feature = "string_cache")]
impl<Static: string_cache::StaticAtomSet> MallocSizeOf for string_cache::Atom<Static> {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

/// For use on types where size_of() returns 0.
#[macro_export]
macro_rules! malloc_size_of_is_0(
    ($($ty:ty),+) => (
        $(
            impl $crate::MallocSizeOf for $ty {
                #[inline(always)]
                fn size_of(&self, _: &mut $crate::MallocSizeOfOps) -> usize {
                    0
                }
            }
        )+
    );
    ($($ty:ident<$($gen:ident),+>),+) => (
        $(
        impl<$($gen: $crate::MallocSizeOf),+> $crate::MallocSizeOf for $ty<$($gen),+> {
            #[inline(always)]
            fn size_of(&self, _: &mut $crate::MallocSizeOfOps) -> usize {
                0
            }
        }
        )+
    );
);

malloc_size_of_is_0!(bool, char, str);
malloc_size_of_is_0!(u8, u16, u32, u64, u128, usize);
malloc_size_of_is_0!(i8, i16, i32, i64, i128, isize);
malloc_size_of_is_0!(f32, f64);
malloc_size_of_is_0!(NonZeroUsize);
malloc_size_of_is_0!(std::sync::atomic::AtomicBool);
malloc_size_of_is_0!(std::sync::atomic::AtomicIsize);
malloc_size_of_is_0!(std::sync::atomic::AtomicUsize);

malloc_size_of_is_0!(Range<u8>, Range<u16>, Range<u32>, Range<u64>, Range<usize>);
malloc_size_of_is_0!(Range<i8>, Range<i16>, Range<i32>, Range<i64>, Range<isize>);
malloc_size_of_is_0!(Range<f32>, Range<f64>);

#[cfg(feature = "url")]
impl MallocSizeOf for url::Host {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        match *self {
            url::Host::Domain(ref s) => s.size_of(ops),
            _ => 0,
        }
    }
}

#[cfg(feature = "time")]
malloc_size_of_is_0!(time::Duration);
#[cfg(feature = "time")]
malloc_size_of_is_0!(time::Tm);

/// Measurable that defers to inner value and used to verify MallocSizeOf implementation in a
/// struct.
#[derive(Clone)]
pub struct Measurable<T: MallocSizeOf>(pub T);

impl<T: MallocSizeOf> Deref for Measurable<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: MallocSizeOf> DerefMut for Measurable<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: MallocSizeOf> MallocSizeOf for MaybeUninit<T> {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        std::mem::size_of::<T>()
    }
}

impl<K, V> MallocShallowSizeOf for BTreeMap<K, V> {
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        // See the implementation for std::collections::HashSet for details.
        if ops.has_malloc_enclosing_size_of() {
            self.values()
                .next()
                .map_or(0, |v| unsafe { ops.malloc_enclosing_size_of(v) })
        } else {
            self.iter().size_hint().0 * (size_of::<V>() + size_of::<K>() + size_of::<usize>())
        }
    }
}

impl<V: MallocSizeOf> MallocSizeOf for BTreeSet<V> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        // very raw estimation
        let mut size = 0;
        for v in self.iter() {
            size += v.size_of(ops);
        }
        size
    }
}

impl<V> MallocShallowSizeOf for BTreeSet<V> {
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        // See the implementation for std::collections::HashSet for details.
        if ops.has_malloc_enclosing_size_of() {
            self.iter()
                .next()
                .map_or(0, |v| unsafe { ops.malloc_enclosing_size_of(v) })
        } else {
            self.iter().size_hint().0 * (size_of::<V>() + size_of::<usize>())
        }
    }
}

impl<K: MallocSizeOf, V: MallocSizeOf> MallocSizeOf for BTreeMap<K, V> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        // very raw estimation
        let mut size = 0;
        for (k, v) in self.iter() {
            size += k.size_of(ops) + v.size_of(ops);
        }
        size
    }
}

#[cfg(feature = "hashbrown")]
impl<K, V, S> MallocShallowSizeOf for HashMap<K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    fn shallow_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        // See the implementation for std::collections::HashSet for details.
        if ops.has_malloc_enclosing_size_of() {
            self.values()
                .next()
                .map_or(0, |v| unsafe { ops.malloc_enclosing_size_of(v) })
        } else {
            self.capacity() * (size_of::<V>() + size_of::<K>() + size_of::<usize>())
        }
    }
}

#[cfg(feature = "hashbrown")]
impl<K: MallocSizeOf, V: MallocSizeOf, S> MallocSizeOf for HashMap<K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        let mut n = self.shallow_size_of(ops);
        for (k, v) in self.iter() {
            n += k.size_of(ops);
            n += v.size_of(ops);
        }
        n
    }
}

#[cfg(feature = "hibitset")]
impl MallocSizeOf for BitSet {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.layer0_as_slice().size_of(ops)
            + self.layer1_as_slice().size_of(ops)
            + self.layer2_as_slice().size_of(ops)
    }
}

#[cfg(feature = "specs")]
impl MallocSizeOf for Entity {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

#[cfg(feature = "specs")]
impl MallocSizeOf for ComponentEvent {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

#[cfg(feature = "specs")]
impl<T: MallocSizeOf> MallocSizeOf for ReaderId<T> {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        8
    }
}

#[cfg(feature = "beach_map")]
impl<K: MallocSizeOf, V: MallocSizeOf> MallocSizeOf for BeachMap<K, V> {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

#[cfg(feature = "beach_map")]
impl<K: MallocSizeOf> MallocSizeOf for ID<K> {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

#[cfg(feature = "lyon")]
impl<T: MallocSizeOf> MallocSizeOf for lyon::lyon_tessellation::VertexBuffers<T, u32> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.vertices.size_of(ops) + self.indices.size_of(ops)
    }
}

#[cfg(feature = "lyon")]
impl<T: MallocSizeOf> MallocSizeOf for lyon::lyon_tessellation::VertexBuffers<T, u16> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.vertices.size_of(ops) + self.indices.size_of(ops)
    }
}

#[cfg(feature = "rstar")]
impl<T: MallocSizeOf + RTreeObject> MallocSizeOf for rstar::ParentNode<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.children().size_of(ops)
    }
}

#[cfg(feature = "rstar")]
impl<T: MallocSizeOf + RTreeObject> MallocSizeOf for rstar::RTreeNode<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        match self {
            RTreeNode::Leaf(child) => child.size_of(ops),
            RTreeNode::Parent(children) => children.size_of(ops),
        }
    }
}

#[cfg(feature = "rstar")]
impl<T: MallocSizeOf + RTreeObject> MallocSizeOf for rstar::RTree<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.root().size_of(ops)
    }
}

#[cfg(feature = "serde_json")]
impl MallocSizeOf for serde_json::Value {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 0,
            Value::Number(_) => 0,
            Value::String(s) => s.size_of(ops),
            Value::Array(a) => a.size_of(ops),
            Value::Object(o) => o.size_of(ops),
        }
    }
}

#[cfg(feature = "arrayvec")]
impl<const T: usize> MallocSizeOf for arrayvec::ArrayString<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
       T
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::{smallvec, SmallVec};

    #[test]
    fn test_boxed_slice() {
        // we must use the shallow size as the slice implementation
        // does not distinguish between boxed slices or slices referencing
        // data on the stack
        let boxed_slice: Box<[i32; 3]> = Box::new([1, 2, 3]);
        let mut ops = MallocSizeOfOps::default();
        #[cfg(target_os = "macos")]
        assert_eq!(boxed_slice.size_of(&mut ops), 3 * 4 + 4);
        #[cfg(target_os = "windows")]
        assert_eq!(boxed_slice.size_of(&mut ops), 3 * 4);

        let slice = &[1, 2, 3];
        let mut ops = MallocSizeOfOps::default();
        assert_eq!(slice.size_of(&mut ops), 0);
    }

    #[test]
    fn test_bit_set() {
        let mut bit_set = BitSet::new();
        let mut ops = MallocSizeOfOps::default();
        bit_set.add(1);
        assert_eq!(bit_set.size_of(&mut ops), 96);
    }

    #[test]
    fn test_large_bit_set() {
        let mut bit_set = BitSet::new();
        let mut ops = MallocSizeOfOps::default();
        for i in 0..100_000 {
            bit_set.add(i);
        }
        assert_eq!(bit_set.size_of(&mut ops), 16672);
    }

    #[test]
    fn test_small_vec() {
        let mut ops = MallocSizeOfOps::default();
        let small_vec: Box<SmallVec<[u32; 4]>> = Box::new(smallvec![1, 2, 3, 4]);
        assert_eq!(small_vec.size_of(&mut ops), 32);
    }

    #[test]
    fn test_euclid_angle() {
        let mut ops = MallocSizeOfOps::default();
        let angle = euclid::Angle::<f64>::radians(1.0);
        assert_eq!(angle.size_of(&mut ops), 0);
        #[cfg(target_os = "windows")]
        assert_eq!(Box::new(angle).size_of(&mut ops), 8);
        #[cfg(target_os = "macos")]
        assert_eq!(Box::new(angle).size_of(&mut ops), 16);
    }

    #[test]
    fn test_rstar() {
        let mut ops = MallocSizeOfOps::default();
        let mut tree = rstar::RTree::new();
        tree.insert([0.1, 0.0f32]);
        tree.insert([0.2, 0.1]);
        tree.insert([0.3, 0.0]);
        assert_eq!(tree.size_of(&mut ops), 336);
    }

    #[test]
    fn test_json_value() {
        let mut ops = MallocSizeOfOps::default();
        let v = serde_json::Value::from("hello");
        assert_eq!(v.size_of(&mut ops), 5);
    }
}
