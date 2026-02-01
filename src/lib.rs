// See LICENCE.md and INCLUDED_WORKS.md for licence and copyright information.

pub use slot_pair::{SlotPairTSV, Reader};
use std::ops::Deref;

mod slot_pair;
mod thread_local;

pub type Version = u64;

pub trait ThreadSafeVar<T: Send + 'static> {
	fn get(&self) -> impl ThreadSafeVarRef<T>;

	/// # Safety
	/// This method has a logical race condition: if another thread calls set()
	/// between your read and this set(), your write will silently overwrite theirs.
	/// Prefer `compare_and_set` or `update` for safe concurrent modifications.
	unsafe fn set(&self, data: T) -> Version;

	/// Atomically sets the value only if current version matches expected_version.
	/// Returns Ok(new_version) on success, Err((data, current_version)) on mismatch.
	fn compare_and_set(&self, data: T, expected_version: Version) -> Result<Version, (T, Version)>;

	/// Read-modify-write with automatic retry on conflict.
	/// Calls closure with current value, attempts to set result.
	/// Retries until successful.
	fn update<F>(&self, f: F) -> Version
	where
		F: Fn(&T) -> T;

	/// Read-modify-write with automatic retry on conflict.
	/// Calls closure with current value, attempts to set result.
	/// Retries until successful.
	fn update_async<F, FR>(&self, f: F) -> impl Future<Output = Version> + Send
	where
		F: Fn(&T) -> FR + Send,
		FR: Future<Output = T> + Send;
}

pub trait ThreadSafeVarRef<T: Send>: Deref<Target = T> + Clone + Sync + Send + 'static {
	fn version(&self) -> Version;
	fn get(&self) -> &T;
}

#[cfg(test)]
mod tests {
	use rclite::Arc;
	use super::*;

	#[test]
	fn can_read() {
		let msg = "Hi!".to_string();
		let tsv = SlotPairTSV::new(msg);
		assert_eq!("Hi!", tsv.get().deref());
		assert_eq!("Hi!", tsv.get().deref());
		assert_eq!(0, tsv.get().version());
	}

	#[test]
	fn can_write() {
		let tsv = SlotPairTSV::new("Hi!".to_string());
		assert_eq!(0, tsv.get().version());
		assert_eq!("Hi!", *tsv.get());
		assert_eq!("Hi!", *tsv.get());

		let new_version = unsafe { tsv.set("Hello!".to_string()) };
		assert_eq!(1, new_version);
		assert_eq!("Hello!", *tsv.get());
		assert_eq!("Hello!", *tsv.get());

		let new_version = unsafe { tsv.set("Bye!".to_string()) };
		assert_eq!(2, new_version);
		assert_eq!("Bye!", *tsv.get());
		assert_eq!("Bye!", *tsv.get());
	}

	#[test]
	fn can_read_across_threads() {
		let tsv = Arc::new(SlotPairTSV::new("Hi!".to_string()));
		assert_eq!(0, tsv.get().version());
		assert_eq!("Hi!", *tsv.get());
		assert_eq!("Hi!", *tsv.get());
		let tsv_ = tsv.clone();
		let jh1 = std::thread::spawn(move || {
			assert_eq!(0, tsv_.get().version());
			let reader = tsv_.get();
			assert_eq!("Hi!", *reader);
			assert_eq!("Hi!", *reader);
		});
		let tsv_ = tsv.clone();
		let jh2 = std::thread::spawn(move || {
			assert_eq!(0, tsv_.get().version());
			let reader = tsv_.get();
			assert_eq!("Hi!", *reader);
			assert_eq!("Hi!", *reader);
		});
		jh1.join().unwrap();
		jh2.join().unwrap();

		let new_version = unsafe { tsv.set("Hello!".to_string()) };
		assert_eq!(1, new_version);
		assert_eq!("Hello!", *tsv.get());
		assert_eq!("Hello!", *tsv.get());

		let new_version = unsafe { tsv.set("Bye!".to_string()) };
		assert_eq!(2, new_version);
		assert_eq!("Bye!", *tsv.get());
		assert_eq!("Bye!", *tsv.get());
	}
	#[test]
	fn can_read_and_write_across_threads() {
		let tsv = Arc::new(SlotPairTSV::new("Hi!".to_string()));
		assert_eq!(0, tsv.get().version());
		assert_eq!("Hi!", *tsv.get());
		assert_eq!("Hi!", *tsv.get());
		let tsv_ = tsv.clone();
		let jh1 = std::thread::spawn(move || {
			assert_eq!(0, tsv_.get().version());
			let reader = tsv_.get();
			assert_eq!("Hi!", *reader);
			assert_eq!("Hi!", *reader);
		});
		let tsv_ = tsv.clone();
		let jh2 = std::thread::spawn(move || {
			loop {
				// Wait until a thread sets it to "Hello!"
				let val = &*tsv_.get();
				if val == "Hello!" {
					unsafe { tsv_.set("Bye!".to_string()) };
					break;
				}
			}
		});
		jh1.join().unwrap();

		let new_version = unsafe { tsv.set("Hello!".to_string()) };
		assert_eq!(1, new_version);
		// Don't assert "Hello!" here - jh2 is racing to change it to "Bye!"
		jh2.join().unwrap();
		assert_eq!("Bye!", *tsv.get());
		assert_eq!("Bye!", *tsv.get());
	}
	#[test]
	#[cfg(not(miri))] // Needs better concurrency support in Miri
	fn can_read_and_write_with_contention() {
		let tsv = Arc::new(SlotPairTSV::new(0i32));
		let tsv_ = tsv.clone();
		let writer1 = std::thread::spawn(move || {
			while *tsv_.get() > -100_000 {
				if let Some(n) = tsv_.get().checked_sub(1) {
					unsafe { tsv_.set(n) };
				}
			}
		});

		let tsv_ = tsv.clone();
		let reader = std::thread::spawn(move || {
			let mut last = 0;
			while *tsv_.get() > -100_000 {
				let val = *tsv_.get();
				assert!(last >= val, "TSV should always decrease. Last: {}, Current: {}", last, val);
				last = val;
			}
		});
		writer1.join().unwrap();
		reader.join().unwrap();
	}

	#[test]
	fn compare_and_set_success() {
		let tsv = SlotPairTSV::new("Hi!".to_string());
		assert_eq!(0, tsv.get().version());

		// Should succeed when version matches
		let result = tsv.compare_and_set("Hello!".to_string(), 0);
		assert!(result.is_ok());
		assert_eq!(1, result.unwrap());
		assert_eq!("Hello!", *tsv.get());
		assert_eq!(1, tsv.get().version());
	}

	#[test]
	fn compare_and_set_failure() {
		let tsv = SlotPairTSV::new("Hi!".to_string());
		assert_eq!(0, tsv.get().version());

		// Should fail when version doesn't match
		let result = tsv.compare_and_set("Hello!".to_string(), 999);
		assert!(result.is_err());
		let (returned_data, current_version) = result.unwrap_err();
		assert_eq!("Hello!", returned_data);
		assert_eq!(0, current_version);
		// Value should be unchanged
		assert_eq!("Hi!", *tsv.get());
		assert_eq!(0, tsv.get().version());
	}

	#[test]
	fn update_basic() {
		let tsv = SlotPairTSV::new(10i32);
		assert_eq!(10, *tsv.get());

		let new_version = tsv.update(|x| x + 5);
		assert_eq!(1, new_version);
		assert_eq!(15, *tsv.get());

		let new_version = tsv.update(|x| x * 2);
		assert_eq!(2, new_version);
		assert_eq!(30, *tsv.get());
	}

	#[test]
	#[cfg(not(miri))] // Needs better concurrency support in Miri
	fn update_with_contention() {
		let tsv = Arc::new(SlotPairTSV::new(0i32));
		let n_threads = 4;
		let increments_per_thread = 1000;

		let handles: Vec<_> = (0..n_threads)
			.map(|_| {
				let tsv = tsv.clone();
				std::thread::spawn(move || {
					for _ in 0..increments_per_thread {
						tsv.update(|x| x + 1);
					}
				})
			})
			.collect();

		for h in handles {
			h.join().unwrap();
		}

		// All increments should be accounted for
		assert_eq!(n_threads * increments_per_thread, *tsv.get());
	}

	#[tokio::test]
	async fn update_async_basic() {
		let tsv = SlotPairTSV::new(10i32);
		assert_eq!(10, *tsv.get());

		let new_version = tsv.update_async(|x| { let x = *x; async move { x + 5 } }).await;
		assert_eq!(1, new_version);
		assert_eq!(15, *tsv.get());

		let new_version = tsv.update_async(|x| { let x = *x; async move { x * 2 } }).await;
		assert_eq!(2, new_version);
		assert_eq!(30, *tsv.get());
	}

	#[tokio::test]
	#[cfg(not(miri))]
	async fn update_async_with_contention() {
		use std::sync::atomic::{AtomicI32, Ordering};

		let tsv = Arc::new(SlotPairTSV::new(0i32));
		let n_tasks = 4;
		let increments_per_task = 100;

		// Track how many times closures were called (should be >= n_tasks * increments_per_task due to retries)
		let closure_calls = Arc::new(AtomicI32::new(0));

		let handles: Vec<_> = (0..n_tasks)
			.map(|_| {
				let tsv = tsv.clone();
				let closure_calls = closure_calls.clone();
				std::thread::spawn(move || {
					let rt = tokio::runtime::Builder::new_current_thread()
						.build()
						.unwrap();
					rt.block_on(async {
						for _ in 0..increments_per_task {
							tsv.update_async(|x| {
								closure_calls.fetch_add(1, Ordering::Relaxed);
								let x = *x;
								async move { x + 1 }
							}).await;
						}
					});
				})
			})
			.collect();

		for h in handles {
			h.join().unwrap();
		}

		// All increments should be accounted for
		assert_eq!(n_tasks * increments_per_task, *tsv.get());
		// Closure should have been called at least n_tasks * increments_per_task times
		assert!(closure_calls.load(Ordering::Relaxed) >= n_tasks * increments_per_task);
	}
}
