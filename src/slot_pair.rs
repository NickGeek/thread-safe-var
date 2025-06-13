// See LICENCE.md and INCLUDED_WORKS.md for licence and copyright information.

use crate::{ThreadSafeVar, ThreadSafeVarRef, Version};
use parking_lot::{Condvar, Mutex};
use rclite::Arc;
use std::cell::UnsafeCell;
use std::convert::Infallible;
use std::marker::PhantomPinned;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::Deref;
use std::pin::Pin;
use std::ptr;
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use thread_local::ThreadLocal;

struct Var<T: Sync + Send> {
	wrapper: AtomicPtr<Wrapper<T>>,
	other_slot: *mut Var<T>,
	n_readers: AtomicU32,
	/// Suppress [Unpin] so that we cannot invalidate `other_slot` by moving `Var<T>`.
	_pin: PhantomPinned,
}
unsafe impl<T: Sync + Send> Send for Var<T> {}
unsafe impl<T: Sync + Send> Sync for Var<T> {}
impl<T: Sync + Send> Drop for Var<T> {
	fn drop(&mut self) {
		unsafe { drop(Arc::from_raw(self.wrapper.load(Ordering::SeqCst))); }
	}
}

const N_VARS: usize = 2;
pub struct SlotPairTSV<T: Sync + Send> {
	vars: [Var<T>; N_VARS],
	next_version: AtomicU64,
	local_key: ThreadLocal<UnsafeCell<ManuallyDrop<Arc<Wrapper<T>>>>>,
	write_lock: Mutex<()>,
	waiter_signaler: Mutex<()>,
	waiter_cv: Condvar,
	waiting_writer_signaler: Mutex<()>,
	waiting_writer_cv: Condvar,
	/// Suppress [Unpin] because our vars are self-referential.
	_pin: PhantomPinned,
}
impl<T: Sync + Send> SlotPairTSV<T> {
	pub fn new(data: T) -> Pin<Box<Self>> {
		let mut tsv = MaybeUninit::<SlotPairTSV<T>>::uninit();
		let tsv = unsafe {
			let tsv_ptr = tsv.as_mut_ptr();
			addr_of_mut!((*tsv_ptr).next_version).write(AtomicU64::new(0));
			addr_of_mut!((*tsv_ptr).local_key).write(ThreadLocal::new());
			addr_of_mut!((*tsv_ptr).write_lock).write(Mutex::new(()));
			addr_of_mut!((*tsv_ptr).waiter_signaler).write(Mutex::new(()));
			addr_of_mut!((*tsv_ptr).waiter_cv).write(Condvar::new());
			addr_of_mut!((*tsv_ptr).waiting_writer_signaler).write(Mutex::new(()));
			addr_of_mut!((*tsv_ptr).waiting_writer_cv).write(Condvar::new());
			Self::fill_var(addr_of_mut!((*tsv_ptr).vars[0]));
			Self::fill_var(addr_of_mut!((*tsv_ptr).vars[1]));

			tsv.assume_init()
		};

		let mut tsv_final_location = Box::new(tsv);
		tsv_final_location.vars[0].other_slot = &raw mut tsv_final_location.vars[1];
		tsv_final_location.vars[1].other_slot = &raw mut tsv_final_location.vars[0];

		// Set wrapper on both slots.
		let wrapper = Arc::new(Wrapper::new(data, 0));
		for i in 0..N_VARS {
			// Safety: We do not mutate this value.
			let wrapper = wrapper.clone();
			let wrapper = unsafe { Arc::into_raw_mut(wrapper) };
			let v = &mut tsv_final_location.vars[i];
			v.wrapper.compare_exchange(ptr::null_mut(), wrapper, Ordering::SeqCst, Ordering::SeqCst)
				.expect("Failed to set initial wrapper");
		}
		assert!(wrapper.strong_count() > 1, "Wrapper should have strong count > 1 after initialization");
		tsv_final_location.next_version.fetch_add(1, Ordering::SeqCst);

		{
			let lock = tsv_final_location.waiter_signaler.lock();
			tsv_final_location.waiter_cv.notify_one();
			drop(lock);
		}

		Box::into_pin(tsv_final_location)
		// // Acquiring the write lock and immediately releasing it as a trivial memory barrier.
		// let write_lock = res.write_lock.lock();
		// drop(write_lock);
	}
	unsafe fn fill_var(var_ptr: *mut Var<T>) {
		unsafe {
			addr_of_mut!((*var_ptr).wrapper).write(AtomicPtr::new(ptr::null_mut()));
			addr_of_mut!((*var_ptr).other_slot).write(ptr::null_mut());
			addr_of_mut!((*var_ptr).n_readers).write(AtomicU32::new(0));
		}
	}

	fn signal_writer(&self) {
		let lock = self.waiting_writer_signaler.lock();
		self.waiting_writer_cv.notify_one();
		drop(lock);
	}

}
impl<T: Sync + Send> ThreadSafeVar<T> for SlotPairTSV<T> {
	fn get(&self) -> impl ThreadSafeVarRef<T> {
		// Fast path if we've already read the value in this thread.
		if let Some(wrapper) = self.local_key.get() {
			let wrapper = unsafe { &*wrapper.get() };
			if wrapper.version == self.next_version.load(Ordering::Acquire) - 1 {
				let data = Arc::clone(&**wrapper);
				return Reader(data)
			}
		}

		loop {
			let version = self.next_version.load(Ordering::Acquire);
			assert_ne!(version, 0);
			let version = version - 1;
			// Get what we hope is still the current slot
			let slot_idx = (version & 0x1) as usize;
			let var = &self.vars[slot_idx];
			// We picked a slot, but we could just have lost against one or more
			// writers. So far nothing we've done would block any number of
			// them.
			// We increment `n_readers` for the slot we picked to keep out
			// writers that would try to set the value in the slot we picked.
			var.n_readers.fetch_add(1, Ordering::SeqCst);
			// Repeat until we win the race.
			if self.next_version.load(Ordering::Acquire) == version + 1 {
				let wrapper = var.wrapper.load(Ordering::Relaxed);
				assert!(!wrapper.is_null());
				let wrapper = ManuallyDrop::new(unsafe { Arc::from_raw(wrapper) });
				let res = Reader(Arc::clone(&*wrapper));
				if var.n_readers.fetch_sub(1, Ordering::SeqCst) == 1
					&& self.next_version.load(Ordering::Acquire) != res.version() + 1 {
					// If we were the last reader, signal a writer that it can write.
					self.signal_writer();
				}
				let thread_key = self.local_key
					.get_or_try(|| Ok::<_,Infallible>(UnsafeCell::new(ManuallyDrop::new(Arc::clone(&*wrapper))))).unwrap();
				let cached_wrapper = unsafe { &mut *thread_key.get() };
				let did_set = cached_wrapper.as_ptr() == wrapper.as_ptr();
				if !did_set {
					unsafe { ManuallyDrop::drop(cached_wrapper) }
					*cached_wrapper = ManuallyDrop::new(Arc::clone(&*wrapper));
				}
				return res;
			}
			if var.n_readers.fetch_sub(1, Ordering::SeqCst) == 1 {
				self.signal_writer();
			}
		}
	}

	fn put(&self, data: T) -> Version {
		let write_lock = self.write_lock.lock();

		// let v1 = unsafe { &*self.vars[1].other_slot };
		// let v2 = unsafe { &*self.vars[0].other_slot };

		// next_version is stable because we hold the write lock.
		let new_version = self.next_version.load(Ordering::Acquire);
		assert_ne!(0, new_version, "Wrapper version should not be 0 because it is incremented on initialisation");
		let wrapper = Arc::new(Wrapper::new(data, new_version));
		// Safety: I do not mutate this value.
		let wrapper_raw = unsafe { Arc::into_raw_mut(wrapper) };

		// Grab the next slot
		let var_idx = ((new_version + 1) & 0x1) as usize;
		let var = unsafe { &*self.vars[var_idx].other_slot };
		let old_wrapper = var.wrapper.load(Ordering::Acquire);
		assert!(!old_wrapper.is_null(), "Wrapper should not be null because it is set on initialisation");
		assert!(!ptr::eq(old_wrapper, wrapper_raw), "New wrapper should not be the same as the old one");
		// let old_wrapper = unsafe { Arc::from_raw(old_wrapper) };

		// Wait for our slot to be dormant before we write to it.
		let mut can_write_signal = self.waiting_writer_signaler.lock();
		while var.n_readers.load(Ordering::Acquire) > 0 {
			self.waiting_writer_cv.wait(&mut can_write_signal);
		}
		drop(can_write_signal);

		// Now we can write to the slot, it's not being read by anyone.
		var.wrapper.compare_exchange(old_wrapper, wrapper_raw, Ordering::SeqCst, Ordering::SeqCst).unwrap();
		self.next_version.fetch_add(1, Ordering::SeqCst);

		// Drop the old wrapper
		let old_wrapper = unsafe { Arc::from_raw(old_wrapper) };
		drop(old_wrapper);
		drop(write_lock);
		new_version
	}
}
impl<T: Sync + Send> Drop for SlotPairTSV<T> {
	fn drop(&mut self) {
		if let Some(cached_wrapper) = self.local_key.get() {
			let value = unsafe { &mut *cached_wrapper.get() };
			unsafe { ManuallyDrop::drop(value) };
			self.local_key.clear();
		}
	}
}

#[derive(Clone)]
struct Wrapper<T: Sync + Send> {
	data: T,
	version: Version,
}
impl<T: Sync + Send> Wrapper<T> {
	fn new(data: T, version: Version) -> Self {
		Self {
			data,
			version,
		}
	}
}
// Debug drop impl
// impl<T: Sync + Send> Drop for Wrapper<T> {
// 	fn drop(&mut self) {
// 		eprintln!("Dropping Wrapper with version {}", self.version);
// 	}
// }

#[derive(Clone)]
pub struct Reader<T: Sync + Send>(Arc<Wrapper<T>>);
impl<T: Sync + Send> ThreadSafeVarRef<T> for Reader<T> {
	fn version(&self) -> Version {
		self.0.version
	}
}
impl<T: Sync + Send> Deref for Reader<T> {
	type Target = T;
	fn deref(&self) -> &Self::Target {
		&self.0.data
	}
}
