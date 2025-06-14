// See LICENCE.md and INCLUDED_WORKS.md for licence and copyright information.

pub use slot_pair::{SlotPairTSV, Reader};
use std::ops::Deref;

mod slot_pair;

pub type Version = u64;

pub trait ThreadSafeVar<T: Send> {
	fn get(&self) -> impl ThreadSafeVarRef<T>;
	fn put(&self, data: T) -> Version;
}

pub trait ThreadSafeVarRef<T: Send>: Deref<Target = T> {
	fn version(&self) -> Version;
}

#[cfg(test)]
mod tests {
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

		let new_version = tsv.put("Hello!".to_string());
		assert_eq!(1, new_version);
		assert_eq!("Hello!", *tsv.get());
		assert_eq!("Hello!", *tsv.get());

		let new_version = tsv.put("Bye!".to_string());
		assert_eq!(2, new_version);
		assert_eq!("Bye!", *tsv.get());
		assert_eq!("Bye!", *tsv.get());
	}
}
