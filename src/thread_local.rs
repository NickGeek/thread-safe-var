use std::ffi::c_void;
use libc::{c_int, pthread_getspecific, pthread_key_create, pthread_key_delete, pthread_key_t, pthread_setspecific};
use std::marker::PhantomData;
use std::mem::MaybeUninit;

pub struct ThreadLocal<T: Send + Sync> {
	key: pthread_key_t,
	_data: PhantomData<T>,
}
unsafe impl<T: Send + Sync> Send for ThreadLocal<T> {}
unsafe impl<T: Send + Sync> Sync for ThreadLocal<T> {}
impl<T: Send + Sync> ThreadLocal<T> {
	/// Creates a thread-local for the given type [T]. The thread-local variable
	/// is valid for either the lifetime of the thread or until the thread-local is dropped,
	/// whichever comes first. If the thread-local is dropped, getting it will return [None].
	/// The pointer returned by [get] is valid for the lifetime of the thread-local variable,
	/// but it is not valid across threads and may be invalidated if the thread-local variable
	/// is dropped.
	/// 
	/// # Panics
	/// - If there are too many thread-local variables, specified by `PTHREAD_KEYS_MAX`.
	pub fn new() -> Self {
		let mut res = MaybeUninit::<Self>::uninit();
		let raw = res.as_mut_ptr();
		unsafe {
			c_err_to_res(pthread_key_create(&raw mut (*raw).key, Some(Self::thread_local_destructor)))
				.expect("Failed to create thread-local key");
			(&raw mut (*res.as_mut_ptr())._data).write(PhantomData);
			res.assume_init()
		}
	}

	pub fn get(&self) -> Option<*const T> {
		self.get_raw().map(|ptr| ptr as *const T)
	}
	fn get_raw(&self) -> Option<*mut T> {
		let data = unsafe { pthread_getspecific(self.key) as *mut T };
		if data.is_null() {
			None
		} else {
			Some(data)
		}
	}

	pub fn set(&self, data: T) -> Result<(), c_int> {
		let old = self.get_raw();

		// This might need to be pinned for safety in the future, we'll see.
		let data = Box::new(data);
		c_err_to_res(unsafe { pthread_setspecific(self.key, Box::into_raw(data) as *const _) })?;

		if let Some(old) = old {
			drop(unsafe { Box::from_raw(old) });
		}
		Ok(())
	}

	extern "C" fn thread_local_destructor(ptr: *mut c_void) {
		// This is called when the thread-local variable is deleted.
		// We can safely drop the Box here.
		eprintln!("thread local destructor called");
		if !ptr.is_null() {
			drop(unsafe { Box::from_raw(ptr as *mut T) });
		}
	}
}
impl<T: Send + Sync> Drop for ThreadLocal<T> {
	fn drop(&mut self) {
		if let Some(data) = self.get_raw() {
			drop(unsafe { Box::from_raw(data) });
		}
		c_err_to_res(unsafe { pthread_key_delete(self.key) }).unwrap();
	}
}

fn c_err_to_res(res: c_int) -> Result<(), c_int> {
	if res == 0 {
		Ok(())
	} else {
		Err(res)
	}
}
