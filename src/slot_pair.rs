// See LICENCE.md and INCLUDED_WORKS.md for licence and copyright information.

use crate::thread_local::ThreadLocal;
use crate::{ThreadSafeVar, ThreadSafeVarRef, Version};
use parking_lot::{Condvar, Mutex};
use rclite::Arc;
use std::fmt::Debug;
use std::hash::Hash;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

struct Var<T: Sync + Send + 'static> {
	wrapper: AtomicPtr<Wrapper<T>>,
	n_readers: AtomicU32,
}
impl<T: Sync + Send> Drop for Var<T> {
	fn drop(&mut self) {
		unsafe { drop(Arc::from_raw(self.wrapper.load(Ordering::SeqCst))); }
	}
}

pub struct SlotPairTSV<T: Sync + Send + 'static>(InnerSlotPairTSV<T>);
impl<T: Sync + Send> SlotPairTSV<T> {
	pub fn new(data: T) -> Self {
		Self(InnerSlotPairTSV::new(data))
	}
}
impl<T: Sync + Send> ThreadSafeVar<T> for SlotPairTSV<T> {
	#[allow(refining_impl_trait)]
	fn get(&self) -> Reader<T> {
		// Fast path if we've already read the value in this thread.
		if let Some(wrapper) = self.0.local_key.get() {
			// Safety: wrapper should be valid because we immediately clone it to ensure stability.
			let wrapper = unsafe { &*wrapper }.clone();
			if wrapper.version == self.0.next_version.load(Ordering::Acquire) - 1 {
				return Reader(wrapper)
			}
		}

		loop {
			let version = self.0.next_version.load(Ordering::Acquire);
			assert_ne!(version, 0);
			let version = version - 1;
			// Get what we hope is still the current slot
			let slot_idx = (version & 0x1) as usize;
			let var = &self.0.vars[slot_idx];
			// We picked a slot, but we could just have lost against one or more
			// writers. So far nothing we've done would block any number of
			// them.
			// We increment `n_readers` for the slot we picked to keep out
			// writers that would try to set the value in the slot we picked.
			var.n_readers.fetch_add(1, Ordering::SeqCst);
			// Repeat until we win the race.
			if self.0.next_version.load(Ordering::Acquire) == version + 1 {
				let wrapper = var.wrapper.load(Ordering::Relaxed);
				assert!(!wrapper.is_null());
				let wrapper = ManuallyDrop::new(unsafe { Arc::from_raw(wrapper) });
				let res = Reader(Arc::clone(&*wrapper));
				if var.n_readers.fetch_sub(1, Ordering::SeqCst) == 1
					&& self.0.next_version.load(Ordering::Acquire) != res.version() + 1 {
					// If we were the last reader, signal a writer that it can write.
					self.0.signal_writer();
				}
				self.0.local_key.set(Arc::clone(&*wrapper)).unwrap();
				return res;
			}
			if var.n_readers.fetch_sub(1, Ordering::SeqCst) == 1 {
				self.0.signal_writer();
			}
		}
	}

	fn set(&self, data: T) -> Version {
		let write_lock = self.0.write_lock.lock();

		// next_version is stable because we hold the write lock.
		let new_version = self.0.next_version.load(Ordering::Acquire);
		assert_ne!(0, new_version, "Wrapper version should not be 0 because it is incremented on initialisation");
		let wrapper = Arc::new(Wrapper::new(data, new_version));
		// Safety: I do not mutate this value.
		let wrapper_raw = unsafe { Arc::into_raw_mut(wrapper) };

		// Grab the next slot
		let other_var_idx = (new_version & 0x1) as usize;
		let var = &self.0.vars[other_var_idx];
		let old_wrapper = var.wrapper.load(Ordering::Acquire);
		assert!(!old_wrapper.is_null(), "Wrapper should not be null because it is set on initialisation");
		assert!(!ptr::eq(old_wrapper, wrapper_raw), "New wrapper should not be the same as the old one");

		// Wait for our slot to be dormant before we write to it.
		let mut can_write_signal = self.0.waiting_writer_signaler.lock();
		while var.n_readers.load(Ordering::Acquire) > 0 {
			self.0.waiting_writer_cv.wait(&mut can_write_signal);
		}
		drop(can_write_signal);

		// Now we can write to the slot, it's not being read by anyone.
		var.wrapper.compare_exchange(old_wrapper, wrapper_raw, Ordering::SeqCst, Ordering::SeqCst).unwrap();
		self.0.next_version.fetch_add(1, Ordering::SeqCst);

		// Drop the old wrapper
		let old_wrapper = unsafe { Arc::from_raw(old_wrapper) };
		drop(old_wrapper);
		drop(write_lock);
		new_version
	}
}
impl<T: Sync + Send + Debug> Debug for SlotPairTSV<T> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.get().fmt(f)
	}
}
impl<T: Sync + Send + PartialEq> PartialEq for SlotPairTSV<T> {
	fn eq(&self, other: &Self) -> bool {
		self.get().eq(&other.get())
	}
}
impl<T: Sync + Send + Eq> Eq for SlotPairTSV<T> {}
impl<T: Sync + Send + PartialOrd> PartialOrd for SlotPairTSV<T> {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		self.get().partial_cmp(&other.get())
	}
}
impl<T: Sync + Send + Ord> Ord for SlotPairTSV<T> {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.get().cmp(&other.get())
	}
}
impl<T: Sync + Send + Hash> Hash for SlotPairTSV<T> {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.get().hash(state);
	}
}

const N_VARS: usize = 2;
pub struct InnerSlotPairTSV<T: Sync + Send + 'static> {
	vars: [Var<T>; N_VARS],
	next_version: AtomicU64,
	local_key: ThreadLocal<Arc<Wrapper<T>>>,
	write_lock: Mutex<()>,
	waiting_writer_signaler: Mutex<()>,
	waiting_writer_cv: Condvar,
}
impl<T: Sync + Send> InnerSlotPairTSV<T> {
	pub fn new(data: T) -> Self {
		let mut tsv = Self {
			vars: [
				Var {
					wrapper: AtomicPtr::new(ptr::null_mut()),
					n_readers: AtomicU32::new(0),
				},
				Var {
					wrapper: AtomicPtr::new(ptr::null_mut()),
					n_readers: AtomicU32::new(0),
				},
			],
			next_version: AtomicU64::new(0),
			local_key: ThreadLocal::new(),
			write_lock: Mutex::new(()),
			waiting_writer_signaler: Mutex::new(()),
			waiting_writer_cv: Condvar::new(),
		};

		// Set wrapper on both slots.
		let wrapper = Arc::new(Wrapper::new(data, 0));
		for i in 0..N_VARS {
			// Safety: We do not mutate this value.
			let wrapper = wrapper.clone();
			let wrapper = unsafe { Arc::into_raw_mut(wrapper) };
			let v = &mut tsv.vars[i];
			v.wrapper.compare_exchange(ptr::null_mut(), wrapper, Ordering::SeqCst, Ordering::SeqCst)
				.expect("Failed to set initial wrapper");
		}
		assert!(wrapper.strong_count() > 1, "Wrapper should have strong count > 1 after initialization");
		tsv.next_version.fetch_add(1, Ordering::SeqCst);
		tsv
	}

	fn signal_writer(&self) {
		let lock = self.waiting_writer_signaler.lock();
		self.waiting_writer_cv.notify_one();
		drop(lock);
	}

}
impl<T: Sync + Send> Drop for InnerSlotPairTSV<T> {
	fn drop(&mut self) {}
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
#[cfg(feature = "verbose")]
impl<T: Sync + Send> Drop for Wrapper<T> {
	fn drop(&mut self) {
		eprintln!("Dropping Wrapper with version {}", self.version);
	}
}

pub struct Reader<T: Sync + Send + 'static>(Arc<Wrapper<T>>);
impl<T: Sync + Send + 'static> Clone for Reader<T> {
	fn clone(&self) -> Self {
		Self(Arc::clone(&self.0))
	}
}
impl<T: Sync + Send + 'static> ThreadSafeVarRef<T> for Reader<T> {
	fn version(&self) -> Version {
		self.0.version
	}
	fn get(&self) -> &T { self }
}
impl<T: Sync + Send> Deref for Reader<T> {
	type Target = T;
	fn deref(&self) -> &Self::Target {
		&self.0.data
	}
}
impl<T: Sync + Send + Debug> Debug for Reader<T> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.0.data.fmt(f)
	}
}
impl<T: Sync + Send + PartialEq> PartialEq for Reader<T> {
	fn eq(&self, other: &Self) -> bool {
		self.0.data == other.0.data
	}
}
impl<T: Sync + Send + Eq> Eq for Reader<T> {}
impl<T: Sync + Send + PartialOrd> PartialOrd for Reader<T> {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		self.0.data.partial_cmp(&other.0.data)
	}
}
impl<T: Sync + Send + Ord> Ord for Reader<T> {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.0.data.cmp(&other.0.data)
	}
}
impl<T: Sync + Send + Hash> Hash for Reader<T> {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.0.data.hash(state);
	}
}
#[cfg(feature = "verbose")]
impl<T: Sync + Send> Drop for Reader<T> {
	fn drop(&mut self) {
		eprintln!("Dropping Reader with version {} (refs = {})", self.0.version, self.0.strong_count());
	}
}
