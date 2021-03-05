use std::result::Result;
use std::sync::{Arc, Mutex};
use std::{alloc, ffi::c_void};
use std::{
    borrow::BorrowMut,
    collections::{HashMap, VecDeque},
    sync::MutexGuard,
};
use std::{cmp::min, io::Error, ops::Deref};

use log::{debug, info, trace, warn};

use crossbeam::channel::{Receiver, Sender};
use crossbeam::sync::WaitGroup;
use segment_map::{Segment, SegmentMap};
use userfaultfd::{ReadWrite, Uffd};

use crate::ufo_objects::UfoHandle;

use super::mmap_wrapers::*;
use super::ufo_objects::*;

pub(crate) enum UfoInstanceMsg {
    Shutdown(WaitGroup),
    Allocate(promissory::Fulfiller<UfoHandle>, UfoObjectConfig),
    Reset(WaitGroup, UfoId),
    Free(WaitGroup, UfoId),
}

struct UfoWriteBuffer {
    ptr: *mut u8,
    size: usize,
}

/// To avoid allocating memory over and over again for population functions keep around an
/// allocated block and just grow it to the needed capacity
impl UfoWriteBuffer {
    fn new() -> UfoWriteBuffer {
        UfoWriteBuffer {
            size: 0,
            ptr: std::ptr::null_mut(),
        }
    }

    unsafe fn ensure_capcity(&mut self, capacity: usize) -> *mut u8 {
        if self.size < capacity {
            let layout = alloc::Layout::from_size_align(self.size, *PAGE_SIZE).unwrap();
            let new_ptr = alloc::realloc(self.ptr, layout, capacity);

            if new_ptr.is_null() {
                alloc::handle_alloc_error(layout);
            } else {
                self.ptr = new_ptr;
                self.size = capacity;
            }
        }
        self.ptr
    }

    unsafe fn slice(&self) -> &[u8] {
        std::slice::from_raw_parts(self.ptr, self.size)
    }
    // unsafe fn slice_mut(&self) -> &mut [u8] {
    //     std::slice::from_raw_parts_mut(self.ptr, self.size)
    // }
}

struct UfoChunks {
    loaded_chunks: VecDeque<UfoChunk>,
    used_memory: usize,
    config: Arc<UfoCoreConfig>,
}

impl UfoChunks {
    fn new(config: Arc<UfoCoreConfig>) -> UfoChunks {
        UfoChunks {
            loaded_chunks: VecDeque::new(),
            used_memory: 0,
            config,
        }
    }

    fn add(&mut self, chunk: UfoChunk) {
        self.used_memory += chunk.size();
        self.loaded_chunks.push_back(chunk);
    }

    fn drop_ufo_chunks(&mut self, ufo_id: UfoId) {
        let chunks = &mut self.loaded_chunks;
        chunks
            .iter_mut()
            .filter(|c| c.ufo_id() == ufo_id)
            .for_each(UfoChunk::mark_freed);
        self.used_memory = chunks.iter().map(UfoChunk::size).sum();
    }

    fn free_until_low_water_mark(&mut self) -> anyhow::Result<usize> {
        debug!(target: "ufo_core", "Freeing memory");
        let low_water_mark = self.config.low_watermark;
        while self.used_memory > low_water_mark {
            match self.loaded_chunks.pop_front().borrow_mut() {
                None => anyhow::bail!("nothing to free"),
                Some(chunk) => {
                    let size = chunk.free_and_writeback_dirty()?;
                    self.used_memory -= size;
                }
            }
        }
        Ok(self.used_memory)
    }
}

pub struct UfoCoreConfig {
    pub writeback_temp_path: &'static str,
    pub high_watermark: usize,
    pub low_watermark: usize,
}

pub(crate) type WrappedUfoObject = Arc<Mutex<UfoObject>>;

pub(crate) struct UfoCoreState {
    object_id_gen: UfoIdGen,

    objects_by_id: HashMap<UfoId, WrappedUfoObject>,
    objects_by_segment: SegmentMap<usize, WrappedUfoObject>,

    loaded_chunks: UfoChunks,
}

pub(crate) struct UfoCore {
    uffd: Uffd,
    pub config: Arc<UfoCoreConfig>,

    pub msg_send: Sender<UfoInstanceMsg>,
    // msg_recv: Receiver<UfoInstanceMsg>,
    state: Mutex<UfoCoreState>,
}

impl UfoCore {
    pub(crate) fn new(config: UfoCoreConfig) -> Result<Arc<UfoCore>, Error> {
        // If this fails then there is nothing we should even try to do about it honestly
        let uffd = userfaultfd::UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(false)
            .create()
            .unwrap();

        let config = Arc::new(config);
        // We want zero capacity so that when we shut down there isn't a chance of any messages being lost
        // TODO CMYK 2021.03.04: find a way to close the channel but still clear the queue
        let (send, recv) = crossbeam::channel::bounded(0);

        let state = Mutex::new(UfoCoreState {
            object_id_gen: UfoIdGen::new(),

            loaded_chunks: UfoChunks::new(Arc::clone(&config)),
            objects_by_id: HashMap::new(),
            objects_by_segment: SegmentMap::new(),
        });

        let core = Arc::new(UfoCore {
            uffd,
            config,
            msg_send: send,
            // msg_recv: recv,
            state,
        });

        trace!(target: "ufo_core", "starting threads");
        let pop_core = Arc::clone(&core);
        std::thread::Builder::new()
            .name("Ufo Core".to_string())
            .spawn(move || UfoCore::populate_loop(pop_core))?;

        let msg_core = Arc::clone(&core);
        std::thread::Builder::new()
            .name("Ufo Msg".to_string())
            .spawn(move || UfoCore::msg_loop(msg_core, recv))?;

        Ok(core)
    }

    fn get_locked_state(&self) -> anyhow::Result<MutexGuard<UfoCoreState>> {
        match self.state.lock() {
            Err(_) => Err(anyhow::Error::msg("broken lock")),
            Ok(l) => Ok(l),
        }
    }

    fn ensure_capcity(config: &UfoCoreConfig, state: &mut UfoCoreState, to_load: usize) {
        assert!(to_load + config.low_watermark < config.high_watermark);
        if to_load + state.loaded_chunks.used_memory > config.high_watermark {
            state.loaded_chunks.free_until_low_water_mark().unwrap();
        }
    }

    fn populate_loop(this: Arc<UfoCore>) {
        trace!(target: "ufo_core", "Started pop loop");
        fn populate_impl(core: &UfoCore, buffer: &mut UfoWriteBuffer, addr: *mut c_void) {
            // this is needed to actually unlock the mutex lock
            fn droplockster<T>(_lock: MutexGuard<T>) {}

            let state = &mut *core.get_locked_state().unwrap();

            let ptr_int = addr as usize;

            // blindly unwrap here because if we get a message for an address we don't have then it is explodey time
            // clone the arc so we aren't borrowing the state
            let ufo_arc = state.objects_by_segment.get(&ptr_int).unwrap().clone();
            let ufo = ufo_arc.lock().unwrap();

            let fault_offset = UfoOffset::from_addr(ufo.deref(), addr);

            let config = &ufo.config;

            let load_size = config.elements_loaded_at_once * config.stride;

            let populate_offset = fault_offset.down_to_nearest_n_relative_to_header(load_size);

            let start = populate_offset.as_index_floor();
            let end = start + config.elements_loaded_at_once;
            let pop_end = min(end, config.element_ct);

            let copy_size = min(
                load_size,
                config.true_size - populate_offset.absolute_offset(),
            );

            debug!(target: "ufo_core", "fault at {}, populate {} bytes at {:#x}",
                start, (pop_end-start) * config.stride, populate_offset.as_ptr_int());

            // unlock the ufo before freeing because that might need to grab the lock on the ufo
            droplockster(ufo);

            // Before we perform the load ensure that there is capacity
            UfoCore::ensure_capcity(&core.config, state, load_size);

            // Reacquire our lock and the config
            let ufo = ufo_arc.lock().unwrap();
            let config = &ufo.config;

            let raw_data = ufo.writeback_util
                .try_readback(&populate_offset)
                .unwrap_or_else(||{
                    trace!(target: "ufo_core", "data ready");
                    unsafe { 
                        buffer.ensure_capcity(load_size);
                        (config.populate)(start, pop_end, buffer.ptr);
                        &buffer.slice()[0..load_size]
                    }
                });
            trace!(target: "ufo_core", "data ready");

            unsafe {
                core.uffd.copy(
                    raw_data.as_ptr().cast(),
                    populate_offset.as_ptr_int() as *mut c_void,
                    copy_size,
                    true,
                )
                .expect("unable to populate range");
            }
            
            assert!(raw_data.len() == load_size);
            let chunk = UfoChunk::new(&ufo_arc, &ufo, populate_offset, raw_data);
            state.loaded_chunks.add(chunk);
        }

        let uffd = &this.uffd;
        let mut buffer = UfoWriteBuffer::new();

        loop {
            match uffd.read_event() {
                Ok(Some(event)) => match event {
                    userfaultfd::Event::Pagefault { rw: _, addr } => 
                        populate_impl(&*this, &mut buffer, addr),
                    e => panic!("Recieved an event we did not register for {:?}", e),
                },
                Ok(None) => {
                    /*huh*/
                    warn!(target: "ufo_core", "huh")
                }
                Err(userfaultfd::Error::SystemError(e))
                    if e.as_errno() == Some(nix::errno::Errno::EBADF) =>
                {
                    info!(target: "ufo_core", "closing uffd loop on ebadf");
                    return /*done*/;
                }
                Err(userfaultfd::Error::ReadEof) => {
                    info!(target: "ufo_core", "closing uffd loop");
                    return /*done*/;
                }
                err => {
                    err.expect("uffd read error");
                }
            }
        }
    }

    fn msg_loop(this: Arc<UfoCore>, recv: Receiver<UfoInstanceMsg>) {
        trace!(target: "ufo_core", "Started msg loop");
        fn allocate_impl(
            this: &Arc<UfoCore>,
            config: UfoObjectConfig,
        ) -> anyhow::Result<UfoHandle> {
            info!(target: "ufo_object", "new Ufo {{
                header_size: {},
                stride: {},
                header_size_with_padding: {},
                true_size: {},
    
                elements_loaded_at_once: {},
                element_ct: {},
             }}",
                config.header_size,
                config.stride,

                config.header_size_with_padding,
                config.true_size,

                config.elements_loaded_at_once,
                config.element_ct,
            );

            let state = &mut *this.get_locked_state()?;

            let id_map = &state.objects_by_id;
            let id_gen = &mut state.object_id_gen;

            let id = id_gen.next(|k| {
                trace!(target: "ufo_core", "testing id {:?}", k);
                !id_map.contains_key(k)
            });

            debug!(target: "ufo_core", "allocate {:?}: {} elements with stride {} [pad|header⋮body] [{}|{}⋮{}]",
                id,
                config.element_ct,
                config.stride,
                config.header_size_with_padding -config.header_size,
                config.header_size,
                config.stride * config.element_ct,
            );

            let mmap = BaseMmap::new(
                config.true_size,
                &[MemoryProtectionFlag::Read, MemoryProtectionFlag::Write],
                &[MmapFlag::Anonymous, MmapFlag::Private, MmapFlag::NoReserve],
                None,
            )
            .expect("Mmap Error");

            let mmap_ptr = mmap.as_ptr();
            let true_size = config.true_size;
            let mmap_base = mmap_ptr as usize;
            let segment = Segment::new(mmap_base, mmap_base + true_size);

            debug!(target: "ufo_core", "mmapped {:#x} - {:#x}", mmap_base, mmap_base + true_size);

            let writeback = UfoFileWriteback::new(id, &config, this)?;
            this.uffd.register(mmap_ptr.cast(), true_size)?;

            //Pre-zero the header, that isn't part of our populate duties
            if config.header_size_with_padding > 0 {
                unsafe {
                    this.uffd
                        .zeropage(mmap_ptr.cast(), config.header_size_with_padding, true)
                }?;
            }

            let c_ptr = mmap.as_ptr().cast();
            let header_offset = config.header_size_with_padding - config.header_size;
            let body_offset = config.header_size_with_padding;
            let ufo = UfoObject {
                id,
                config,
                mmap,
                writeback_util: writeback,
            };

            let ufo = Arc::new(Mutex::new(ufo));

            state.objects_by_id.insert(id, ufo.clone());
            state.objects_by_segment.insert(segment, ufo);

            Ok(UfoHandle {
                core: Arc::downgrade(this),
                id,
                ptr: c_ptr,
                header_offset,
                body_offset,
            })
        }

        fn reset_impl(this: &Arc<UfoCore>, ufo_id: UfoId) -> anyhow::Result<()> {
            let state = &mut *this.get_locked_state()?;

            let ufo = &mut *(state
                .objects_by_id
                .get(&ufo_id)
                .map(Ok)
                .unwrap_or_else(|| Err(anyhow::anyhow!("unknown ufo")))?
                .lock()
                .map_err(|_| anyhow::anyhow!("lock poisoned"))?);

            debug!(target: "ufo_core", "resetting {:?}", ufo.id);

            ufo.reset()?;

            state.loaded_chunks.drop_ufo_chunks(ufo_id);

            Ok(())
        }

        fn free_impl(this: &Arc<UfoCore>, ufo_id: UfoId) -> anyhow::Result<()> {
            let state = &mut *this.get_locked_state()?;
            let ufo = state
                .objects_by_id
                .remove(&ufo_id)
                .map(Ok)
                .unwrap_or_else(|| Err(anyhow::anyhow!("No such Ufo")))?;
            let ufo = ufo.lock().map_err(|_| anyhow::anyhow!("Broken Ufo Lock"))?;

            debug!(target: "ufo_core", "freeing {:?}", ufo.id);

            let mmap_base = ufo.mmap.as_ptr() as usize;
            this.uffd
                .unregister(ufo.mmap.as_ptr().cast(), ufo.config.true_size)?;

            let segment = *(state
                .objects_by_segment
                .get_entry(&mmap_base)
                .map(Ok)
                .unwrap_or_else(|| Err(anyhow::anyhow!("memory segment missing")))?
                .0);

            state.objects_by_segment.remove(&segment);

            state.loaded_chunks.drop_ufo_chunks(ufo_id);

            Ok(())
        }

        fn shutdown_impl(this: &Arc<UfoCore>) {
            info!(target: "ufo_core", "shutting down");
            let keys: Vec<UfoId> = {
                let state = &mut *this.get_locked_state().expect("err on shutdown");
                state.objects_by_id.keys().map(Clone::clone).collect()
            };

            keys.iter()
                .for_each(|k| free_impl(this, *k).expect("err on free"));
        }

        loop {
            match recv.recv() {
                Ok(m) => match m {
                    UfoInstanceMsg::Allocate(fulfiller, cfg) => {
                        fulfiller.fulfill(allocate_impl(&this, cfg).expect("Allocate Error"))
                    }
                    UfoInstanceMsg::Reset(_, ufo_id) => {
                        reset_impl(&this, ufo_id).expect("Reset Error")
                    }
                    UfoInstanceMsg::Free(_, ufo_id) => {
                        free_impl(&this, ufo_id).expect("Free Error")
                    }
                    UfoInstanceMsg::Shutdown(_) => {
                        shutdown_impl(&this);
                        drop(recv);
                        info!(target: "ufo_core", "closing msg loop");
                        return /*done*/;
                    }
                },
                err => {
                    err.expect("recv error");
                }
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        let sync = WaitGroup::new();
        trace!(target: "ufo_core", "sending shutdown msg");
        self.msg_send
            .send(UfoInstanceMsg::Shutdown(sync.clone()))
            .expect("Can't send shutdown signal");
        trace!(target: "ufo_core", "awaiting shutdown sync");
        sync.wait();
        trace!(target: "ufo_core", "sync, closing uffd filehandle");

        let fd = std::os::unix::prelude::AsRawFd::as_raw_fd(&self.uffd);
        // this will signal to the populate loop that it is time to close down
        let close_result = unsafe { libc::close(fd) };
        match close_result {
            0 => {}
            _ => {
                panic!(
                    "clouldn't close uffd handle {}",
                    nix::errno::Errno::last().desc()
                );
            }
        }
        trace!(target: "ufo_core", "close uffd handle: {}", close_result);
    }
}
