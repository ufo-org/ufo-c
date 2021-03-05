use std::io::Error;
use std::num::NonZeroUsize;
use std::result::Result;
use std::sync::{Arc, Mutex, Weak};
use std::{lazy::SyncLazy, sync::MutexGuard};

use log::{debug, error, trace};

use num::Integer;

use crate::mmap_wrapers;

use super::math::*;
use super::mmap_wrapers::*;
use super::return_checks::*;
use super::ufo_core::*;

pub static PAGE_SIZE: SyncLazy<usize> = SyncLazy::new(|| {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    assert!(page_size > 0);
    page_size as usize
});

#[derive(Debug, PartialEq, PartialOrd, Ord, Eq, Copy, Clone, Hash)]
pub struct UfoId(u64);

pub struct UfoIdGen {
    current: u64,
}

impl UfoIdGen {
    pub fn new() -> UfoIdGen {
        UfoIdGen { current: 0 }
    }

    pub(crate) fn next<P>(&mut self, is_unused: P) -> UfoId
    where
        P: Fn(&UfoId) -> bool,
    {
        let mut n = self.current;
        let mut id;
        loop {
            n = n.wrapping_add(1);
            id = UfoId(n);
            if is_unused(&id) {
                break;
            }
        }
        self.current = n;
        id
    }
}

pub struct UfoObjectConfigPrototype {
    pub(crate) header_size: usize,
    pub(crate) stride: usize,
    pub(crate) min_load_ct: Option<usize>,
}

impl UfoObjectConfigPrototype {
    pub fn new_prototype(
        header_size: usize,
        stride: usize,
        min_load_ct: Option<usize>,
    ) -> UfoObjectConfigPrototype {
        UfoObjectConfigPrototype {
            header_size,
            stride,
            min_load_ct,
        }
    }

    pub fn new_config(&self, ct: usize, populate: Box<UfoPopulateFn>) -> UfoObjectConfig {
        UfoObjectConfig::new_config(
            self.header_size,
            ct,
            self.stride,
            self.min_load_ct,
            populate,
        )
    }
}

pub struct UfoObjectConfig {
    pub(crate) populate: Box<UfoPopulateFn>,

    pub(crate) header_size_with_padding: usize,
    pub(crate) header_size: usize,

    pub(crate) stride: usize,
    pub(crate) elements_loaded_at_once: usize,
    pub(crate) element_ct: usize,
    pub(crate) true_size: usize,
}

impl UfoObjectConfig {
    pub fn new_config(
        header_size: usize,
        element_ct: usize,
        stride: usize,
        min_load_ct: Option<usize>,
        populate: Box<UfoPopulateFn>,
    ) -> UfoObjectConfig {
        let min_load_ct = min_load_ct.unwrap_or(1);
        let page_size = mmap_wrapers::get_page_size();

        /* Headers and size */
        let header_size_with_padding = up_to_nearest(header_size as usize, page_size);
        let body_size_with_padding = up_to_nearest(stride * element_ct, page_size);
        let true_size = header_size_with_padding + body_size_with_padding;

        /* loading quanta */
        let min_load_bytes = num::integer::lcm(page_size, stride * min_load_ct);
        let elements_loaded_at_once = min_load_bytes / stride;
        assert!(elements_loaded_at_once * stride == min_load_bytes);

        UfoObjectConfig {
            header_size,
            stride,

            header_size_with_padding,
            true_size,

            elements_loaded_at_once,
            element_ct,

            populate,
        }
    }
}

pub(crate) struct UfoOffset {
    base_addr: usize,
    stride: usize,
    header_bytes: usize,
    absolute_offset_bytes: usize,
}

impl UfoOffset {
    pub fn from_addr(ufo: &UfoObject, addr: *const libc::c_void) -> UfoOffset {
        let addr = addr as usize;
        let base_addr = ufo.mmap.as_ptr() as usize;
        let absolute_offset_bytes = addr
            .checked_sub(base_addr)
            .unwrap_or_else(|| panic!("Addr less than base {} < {}", addr, base_addr));
        let header_bytes = ufo.config.header_size_with_padding;

        assert!(
            header_bytes <= absolute_offset_bytes,
            "Cannot offset into the header"
        );

        UfoOffset {
            base_addr,
            stride: ufo.config.stride,
            header_bytes,
            absolute_offset_bytes,
        }
    }

    pub fn absolute_offset(&self) -> usize {
        self.absolute_offset_bytes
    }

    pub fn offset_from_header(&self) -> usize {
        self.absolute_offset_bytes - self.header_bytes
    }

    pub fn as_ptr_int(&self) -> usize {
        self.base_addr + self.absolute_offset()
    }

    pub fn as_index_floor(&self) -> usize {
        self.offset_from_header().div_floor(&self.stride)
    }

    pub fn down_to_nearest_n_relative_to_header(&self, nearest: usize) -> UfoOffset {
        let offset = self.offset_from_header();
        let offset = down_to_nearest(offset, nearest);

        let absolute_offset_bytes = self.header_bytes + offset;

        UfoOffset {
            absolute_offset_bytes,
            ..*self
        }
    }

    // pub fn add_bytes(&self, bytes: isize) -> UfoOffset{
    //     let absolute_offset_bytes = (
    //         if bytes.is_negative() {
    //             self.absolute_offset().checked_sub(bytes.abs() as usize)
    //         }else{
    //             self.absolute_offset().checked_add(bytes as usize)
    //         }
    //     ).unwrap();
    //     UfoOffset{
    //         absolute_offset_bytes,
    //         ..*self
    //     }
    // }

    // pub fn add_index(&self, index: usize) -> UfoOffset{
    //     self.add_bytes((index * self.stride) as isize)
    // }
}

pub(crate) struct UfoChunk {
    ufo_id: UfoId,
    object: Weak<Mutex<UfoObject>>,
    offset: UfoOffset,
    length: Option<NonZeroUsize>,
    hash: blake3::Hash,
}

impl UfoChunk {
    pub fn new(
        arc: &WrappedUfoObject,
        object: &MutexGuard<UfoObject>,
        offset: UfoOffset,
        initial_data: &[u8],
    ) -> UfoChunk {
        UfoChunk {
            ufo_id: object.id,
            object: Arc::downgrade(arc),
            offset,
            length: NonZeroUsize::new(initial_data.len()),
            hash: blake3::hash(initial_data),
        }
    }

    fn with_slice<F, V>(&self, obj: &UfoObject, f: F) -> Option<V>
    where
        F: FnOnce(&[u8]) -> V,
    {
        self.length.and_then(|length| {
            obj.mmap
                .with_slice(self.offset.absolute_offset(), length.get(), f)
        })
    }

    pub fn free_and_writeback_dirty(&mut self) -> Result<usize, Error> {
        if let Some(length) = self.length {
            let length = length.get();
            if let Some(obj) = self.object.upgrade() {
                let mut obj = obj.lock().unwrap();

                trace!(target: "ufo_object", "free chunk {:?}@{} ({}b)",
                    self.ufo_id, self.offset.absolute_offset() , length
                );

                let calculated_hash = obj
                    .mmap
                    .with_slice(self.offset.absolute_offset(), length, blake3::hash)
                    .unwrap(); // it should never be possible for this to fail
                trace!(target: "ufo_object", "writeback hash matches {}", self.hash == calculated_hash);
                if self.hash != calculated_hash {
                    let o = &mut *obj;
                    o.writeback(self)?;
                }

                unsafe {
                    let ptr = obj.mmap.as_ptr().add(self.offset.absolute_offset());
                    // MADV_DONTNEED has the exact semantics we want, no other advice would work for us
                    check_return_zero(libc::madvise(ptr.cast(), length, libc::MADV_DONTNEED))?;
                }
            }
            self.length = None;
            Ok(length)
        } else {
            Ok(0)
        }
    }

    pub fn mark_freed(&mut self) {
        self.length = None;
    }

    pub fn ufo_id(&self) -> UfoId {
        self.ufo_id
    }

    pub fn size(&self) -> usize {
        self.length.map(NonZeroUsize::get).unwrap_or(0)
    }
}

pub type UfoPopulateFn = dyn Fn(usize, usize, *mut u8) + Sync + Send;
pub(crate) struct UfoFileWriteback {
    mmap: MmapFd,
    total_bytes: usize,
    bitmap_bytes: usize,
}

impl UfoFileWriteback {
    pub fn new(cfg: &UfoObjectConfig, core: &Arc<UfoCore>) -> Result<UfoFileWriteback, Error> {
        let page_size = *PAGE_SIZE;

        let chunk_ct = cfg.element_ct.div_ceil(&cfg.elements_loaded_at_once);
        assert!(chunk_ct * cfg.elements_loaded_at_once >= cfg.element_ct);

        let bitmap_bytes = chunk_ct.div_ceil(&8); /*8 bits per byte*/
        // Now we want to get the bitmap bytes up to the next multiple of the page size
        let bitmap_bytes = up_to_nearest(bitmap_bytes, page_size);
        assert!(bitmap_bytes * 8 >= chunk_ct);
        assert!(bitmap_bytes.trailing_zeros() >= page_size.trailing_zeros());

        let data_bytes = cfg.element_ct * cfg.stride;
        let total_bytes = bitmap_bytes + data_bytes;

        let temp_file = unsafe { OpenFile::temp(core.config.writeback_temp_path, total_bytes) }?;

        let mmap = MmapFd::new(
            total_bytes,
            &[MemoryProtectionFlag::Read, MemoryProtectionFlag::Write],
            &[MmapFlag::Shared],
            None,
            temp_file,
            0,
        )?;

        Ok(UfoFileWriteback {
            mmap,
            total_bytes,
            bitmap_bytes,
        })
    }
}

//TODO: self destruct on drop, needs a weak link to the core
pub struct UfoHandle {
    pub(crate) core: Weak<UfoCore>,
    pub(crate) id: UfoId,
    pub(crate) ptr: *mut std::ffi::c_void,
    pub(crate) header_offset: usize,
    pub(crate) body_offset: usize,
}

impl UfoHandle {
    pub fn header_ptr(&self) -> *mut std::ffi::c_void {
        (self.ptr as usize + self.header_offset) as *mut std::ffi::c_void
    }

    pub fn body_ptr(&self) -> *mut std::ffi::c_void {
        (self.ptr as usize + self.body_offset) as *mut std::ffi::c_void
    }

    pub fn reset(&self) -> anyhow::Result<()> {
        let wait_group = crossbeam::sync::WaitGroup::new();
        let core = match self.core.upgrade() {
            None => anyhow::bail!("Ufo Core shutdown"),
            Some(x) => x,
        };

        core.msg_send
            .send(UfoInstanceMsg::Reset(wait_group.clone(), self.id))
            .map_err(|_| anyhow::anyhow!("Cannot reset UFO, pipe broken"))?;

        wait_group.wait();
        Ok(())
    }

    fn free_impl(&self) -> anyhow::Result<()> {
        let wait_group = crossbeam::sync::WaitGroup::new();
        let core = match self.core.upgrade() {
            None => anyhow::bail!("Ufo Core shutdown"),
            Some(x) => x,
        };

        core.msg_send
            .send(UfoInstanceMsg::Free(wait_group.clone(), self.id))
            .map_err(|_| anyhow::anyhow!("Cannot free UFO, pipe broken"))?;

        wait_group.wait();
        Ok(())
    }

    pub fn free(self) -> anyhow::Result<()> {
        self.free_impl()
    }
}

impl Drop for UfoHandle {
    fn drop(&mut self) {
        if let Err(e) = self.free_impl() {
            error!(target: "ufo_object", "error on free {}", e);
        }
    }
}

unsafe impl Send for UfoHandle {}

pub(crate) struct UfoObject {
    pub(crate) id: UfoId,
    pub(crate) config: UfoObjectConfig,
    pub(crate) mmap: BaseMmap,
    pub(crate) writeback_util: UfoFileWriteback,
}

impl UfoObject {
    fn writeback(&mut self, chunk: &UfoChunk) -> Result<(), Error> {
        let wb_ptr = self.writeback_util.mmap.as_ptr();
        let offset = self.writeback_util.bitmap_bytes + chunk.offset.offset_from_header();
        let length = chunk.length.unwrap().get(); // in a writeback the length must be valid
        let writeback_arr = unsafe { std::slice::from_raw_parts_mut(wb_ptr.add(offset), length) };
        chunk
            .with_slice(self, |live_data| {
                debug!(target: "ufo_object", "writeback {:?}@{:#x}:{} → {:#x}",
                    chunk.ufo_id(),
                    self.mmap.as_ptr() as usize + chunk.offset.absolute_offset(),
                    length,
                    wb_ptr as usize + offset
                );
                assert!(live_data.len() == writeback_arr.len());
                writeback_arr.copy_from_slice(live_data)
            })
            .map(Ok)
            .unwrap_or_else(|| {
                Err(Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "Chunk not valid",
                ))
            })
    }

    pub fn reset(&mut self) -> anyhow::Result<()> {
        {
            let ptr = self.mmap.as_ptr();
            let length = self.config.true_size;
            unsafe {
                check_return_zero(libc::madvise(ptr.cast(), length, libc::MADV_DONTNEED))?;
            }
        }

        {
            let ptr = self.writeback_util.mmap.as_ptr();
            let length = self.writeback_util.total_bytes;
            unsafe {
                check_return_zero(libc::madvise(ptr.cast(), length, libc::MADV_DONTNEED))?;
            }
        }

        Ok(())
    }
}
