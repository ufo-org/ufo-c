use std::sync::{Arc, MutexGuard};

use anyhow::Result;

use libc::c_void;
use ufos_core::{UfoCoreConfig, UfoObjectConfigPrototype, UfoPopulateError, WrappedUfoObject, UfoObject};

macro_rules! opaque_c_type {
    ($wrapper_name:ident, $wrapped_type:ty) => {
        impl $wrapper_name {
            fn wrap(t: $wrapped_type) -> Self {
                $wrapper_name {
                    ptr: Box::into_raw(Box::new(t)).cast(),
                }
            }

            fn none() -> Self {
                $wrapper_name {
                    ptr: std::ptr::null_mut(),
                }
            }

            fn deref(&self) -> Option<&$wrapped_type> {
                if self.ptr.is_null() {
                    None
                } else {
                    Some(unsafe { &*self.ptr.cast() })
                }
            }

            #[allow(dead_code)]
            fn deref_mut(&self) -> Option<&mut $wrapped_type> {
                if self.ptr.is_null() {
                    None
                } else {
                    Some(unsafe { &mut *self.ptr.cast() })
                }
            }
        }
    };

    ($wrapper_name:ident, $wrapped_type:ty, $free_name:ident) => {
        opaque_c_type!($wrapper_name, $wrapped_type);

        impl $wrapper_name {
            #[no_mangle]
            pub extern "C" fn $free_name(self) {}
        }

        impl Drop for $wrapper_name {
            fn drop(&mut self) {
                if !self.ptr.is_null() {
                    let mut the_ptr = std::ptr::null_mut();
                    unsafe {
                        std::ptr::swap(&mut the_ptr, &mut self.ptr);
                        Box::<$wrapped_type>::from_raw(the_ptr.cast());
                    }
                }
            }
        }
    };
}

#[repr(C)]
pub struct UfoCore {
    ptr: *mut c_void,
}
opaque_c_type!(UfoCore, Arc<ufos_core::UfoCore>, free_ufo_core);


impl UfoCore {
    #[no_mangle]
    pub unsafe extern "C" fn new_ufo_core(
        writeback_temp_path: *const libc::c_char,
        low_water_mark: libc::size_t,
        high_water_mark: libc::size_t,
    ) -> Self {
        std::panic::catch_unwind(|| {
            let wb = std::ffi::CStr::from_ptr(writeback_temp_path)
                .to_str().expect("invalid string")
                .to_string();
            assert!(low_water_mark < high_water_mark);
            let config = UfoCoreConfig {
                writeback_temp_path: wb,
                low_watermark: low_water_mark,
                high_watermark: high_water_mark,
            };

            let core = ufos_core::UfoCore::new(config);
            match core {
                Err(_) => Self::none(),
                Ok(core) => Self::wrap(core),
            }
        })
        .unwrap_or_else(|_| Self::none())
    }

    #[no_mangle]
    pub extern "C" fn shutdown(self) {}

    #[no_mangle]
    pub extern "C" fn is_valid(&self) -> bool {
        self.deref().is_some()
    }

    #[no_mangle]
    pub extern "C" fn get_ufo_by_address(&self, ptr: usize) -> UfoObj{
        std::panic::catch_unwind(|| {
            self.deref()
            .and_then( |core| {
                let ufo = core
                    .get_ufo_by_address(ptr).ok()?;
                Some(UfoObj::wrap(ufo))
            })
            .unwrap_or_else(UfoObj::none)
        }).unwrap_or_else(|_| UfoObj::none())
    }

    #[no_mangle]
    pub extern "C" fn new_ufo(
        &self,
        prototype: &UfoPrototype,
        ct: libc::size_t,
        callback_data: *mut c_void,
        populate: extern "C" fn(*mut c_void, libc::size_t, libc::size_t, *mut libc::c_uchar) -> i32,
    ) -> UfoObj {
        std::panic::catch_unwind(|| {
            let callback_data_int = callback_data as usize;
            let populate = move |start, end, to_populate| {
                let ret = populate(
                    callback_data_int as *mut c_void,
                    start,
                    end,
                    to_populate,
                );

                if ret != 0 {
                    Err(UfoPopulateError)
                }else{
                    Ok(())
                }
            };
            let r = self
                .deref()
                .zip(prototype.deref())
                .map(move |(core, prototype)| {
                    core.allocate_ufo(prototype.new_config(ct, Box::new(populate)))
                });
            match r {
                Some(Ok(ufo)) => UfoObj::wrap(ufo),
                _ => UfoObj::none(),
            }
        })
        .unwrap_or_else(|_| UfoObj::none())
    }
}

#[repr(C)]
pub struct UfoPrototype {
    ptr: *mut c_void,
}
opaque_c_type!(UfoPrototype, UfoObjectConfigPrototype, free_ufo_prototype);


impl UfoPrototype {
    #[no_mangle]
    pub extern "C" fn new_ufo_prototype(
        header_size: libc::size_t,
        stride: libc::size_t,
        min_load_ct: libc::size_t,
    ) -> UfoPrototype {
        std::panic::catch_unwind(|| {
            let min_load_ct = Some(min_load_ct).filter(|x| *x > 0);
            Self::wrap(UfoObjectConfigPrototype::new_prototype(
                header_size,
                stride,
                min_load_ct,
            ))
        })
        .unwrap_or_else(|_| Self::none())
    }
}

#[repr(C)]
pub struct UfoObj {
    ptr: *mut c_void,
}

opaque_c_type!(UfoObj, WrappedUfoObject);

impl UfoObj {
    fn with_ufo<F, T, E>(&self, f: F) -> Option<T>
    where
        F: FnOnce(MutexGuard<UfoObject>) -> Result<T, E>
    {
        self.deref()
            .and_then(|ufo| {
                    ufo.lock().ok()
                    .map(f)?.ok()
            })
    }

    #[no_mangle]
    pub unsafe extern "C" fn reset(&self) -> i32 {
        std::panic::catch_unwind(|| {
            self.with_ufo(|ufo| ufo.reset())
                .map(|w| w.wait())
                .map(|()| 0)
                .unwrap_or(-1)
        })
        .unwrap_or(-1)
    }

    #[no_mangle]
    pub extern "C" fn header_ptr(&self) -> *mut std::ffi::c_void {
        std::panic::catch_unwind(|| {
            self.with_ufo(|ufo| Ok::<*mut c_void, ()>(ufo.header_ptr()))
            .unwrap_or_else(|| std::ptr::null_mut())
        })
        .unwrap_or_else(|_| std::ptr::null_mut())
    }

    #[no_mangle]
    pub extern "C" fn body_ptr(&self) -> *mut std::ffi::c_void {
        std::panic::catch_unwind(|| {
            self.with_ufo(|ufo| Ok::<*mut c_void, ()>(ufo.body_ptr()))
            .unwrap_or_else(|| std::ptr::null_mut())
        })
        .unwrap_or_else(|_| std::ptr::null_mut())
    }

    #[no_mangle]
    pub extern "C" fn free_ufo(self) {
        std::panic::catch_unwind(|| {
            self.deref()
            .and_then(|ufo| ufo.lock().ok()?.free().ok())
            .map(|w| w.wait())
            .unwrap_or(())
        }).unwrap_or(())
    }
}

#[no_mangle]
pub extern "C" fn begin_ufo_log() {
    stderrlog::new()
        // .module("ufo_core")
        .verbosity(4)
        .timestamp(stderrlog::Timestamp::Millisecond)
        .init()
        .unwrap();
}