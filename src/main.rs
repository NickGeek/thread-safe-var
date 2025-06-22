use thread_safe_var::{SlotPairTSV, ThreadSafeVar, ThreadSafeVarRef};

fn main() {
	let msg = "Hi!".to_string();
	let tsv = SlotPairTSV::new(msg);
	assert_eq!("Hi!", &*tsv.get());
	assert_eq!("Hi!", &*tsv.get());
	assert_eq!(0, tsv.get().version());
}