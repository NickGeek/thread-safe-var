// See LICENCE.md and INCLUDED_WORKS.md for licence and copyright information.

pub use slot_pair::{SlotPairTSV, Reader};
use std::ops::Deref;

mod slot_pair;
mod thread_local;

pub type Version = u64;

pub trait ThreadSafeVar<T: Send + 'static> {
	fn get(&self) -> impl ThreadSafeVarRef<T>;
	fn set(&self, data: T) -> Version;
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

		let new_version = tsv.set("Hello!".to_string());
		assert_eq!(1, new_version);
		assert_eq!("Hello!", *tsv.get());
		assert_eq!("Hello!", *tsv.get());

		let new_version = tsv.set("Bye!".to_string());
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

		let new_version = tsv.set("Hello!".to_string());
		assert_eq!(1, new_version);
		assert_eq!("Hello!", *tsv.get());
		assert_eq!("Hello!", *tsv.get());

		let new_version = tsv.set("Bye!".to_string());
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
					tsv_.set("Bye!".to_string());
					break;
				}
			}
		});
		jh1.join().unwrap();

		let new_version = tsv.set("Hello!".to_string());
		assert_eq!(1, new_version);
		assert_eq!("Hello!", *tsv.get());
		assert_eq!("Hello!", *tsv.get());
		jh2.join().unwrap();
		assert_eq!("Bye!", *tsv.get());
		assert_eq!("Bye!", *tsv.get());
	}
}
