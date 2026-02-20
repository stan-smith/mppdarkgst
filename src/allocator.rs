#![allow(dead_code)]
//! MppAllocator — wraps an MppBufferGroup (INTERNAL, DRM) and GstDmaBufAllocator
//! for zero-copy DMA-BUF GstMemory allocation.

use crate::mpp_ffi as ffi;

pub struct MppAllocator {
    group: ffi::MppBufferGroup,
    /// Raw pointer to GstDmaBufAllocator (ref held by us)
    dmabuf_alloc: *mut ffi::GstAllocator,
}

// Safety: MppAllocator is used behind a Mutex in all callers.
unsafe impl Send for MppAllocator {}

impl MppAllocator {
    pub fn new() -> Result<Self, ()> {
        unsafe {
            let mut group: ffi::MppBufferGroup = std::ptr::null_mut();
            if ffi::mpp_buffer_group_get_internal(&mut group, ffi::MPP_BUFFER_TYPE_DRM)
                != ffi::MPP_OK
            {
                return Err(());
            }

            let dmabuf_alloc = ffi::gst_dmabuf_allocator_new();
            if dmabuf_alloc.is_null() {
                ffi::mpp_buffer_group_put(group);
                return Err(());
            }

            Ok(Self {
                group,
                dmabuf_alloc,
            })
        }
    }

    /// Allocate a new MppBuffer of `size` bytes from the internal group.
    /// Returns (MppBuffer, fd). Caller must eventually call mpp_buffer_put.
    pub fn alloc(&self, size: usize) -> Result<(ffi::MppBuffer, i32), ()> {
        unsafe {
            let mut mbuf: ffi::MppBuffer = std::ptr::null_mut();
            if ffi::mpp_buffer_get(self.group, &mut mbuf, size) != ffi::MPP_OK {
                return Err(());
            }
            let fd = ffi::mpp_buffer_get_fd(mbuf);
            Ok((mbuf, fd))
        }
    }

    /// Get the MPP buffer group (for MPP_DEC_SET_EXT_BUF_GROUP).
    pub fn mpp_group(&self) -> ffi::MppBufferGroup {
        self.group
    }

    /// Get the raw GstDmaBufAllocator pointer.
    pub fn gst_dmabuf_allocator(&self) -> *mut ffi::GstAllocator {
        self.dmabuf_alloc
    }
}

impl Drop for MppAllocator {
    fn drop(&mut self) {
        unsafe {
            ffi::mpp_buffer_group_put(self.group);
            glib::gobject_ffi::g_object_unref(self.dmabuf_alloc as *mut _);
        }
    }
}

/// Guard that calls mpp_buffer_put on drop.
/// Attached as qdata on GstMemory so the MppBuffer is released when GstMemory is freed.
pub struct MppBufGuard {
    mpp_buf: ffi::MppBuffer,
}

// Safety: MppBuffer is refcounted and thread-safe.
unsafe impl Send for MppBufGuard {}
unsafe impl Sync for MppBufGuard {}

impl MppBufGuard {
    pub fn new(mpp_buf: ffi::MppBuffer) -> Self {
        Self { mpp_buf }
    }
}

impl Drop for MppBufGuard {
    fn drop(&mut self) {
        unsafe {
            ffi::mpp_buffer_put(self.mpp_buf);
        }
    }
}

/// Attach an MppBufGuard as qdata on a GstMemory (wrapped in gst::Memory).
/// The guard is dropped (calling mpp_buffer_put) when the GstMemory is freed.
pub fn attach_mpp_buf_guard(mem: &gst::Memory, guard: MppBufGuard) {
    use glib::translate::IntoGlib;

    static MPP_BUF_QUARK: std::sync::OnceLock<glib::Quark> = std::sync::OnceLock::new();
    let quark = *MPP_BUF_QUARK.get_or_init(|| glib::Quark::from_str("mpp-buf-guard"));

    unsafe {
        let mem_ptr = mem.as_ptr() as *mut gst::ffi::GstMiniObject;
        gst::ffi::gst_mini_object_set_qdata(
            mem_ptr,
            quark.into_glib(),
            Box::into_raw(Box::new(guard)) as *mut _,
            Some(destroy_guard),
        );
    }
}

unsafe extern "C" fn destroy_guard(ptr: glib::ffi::gpointer) {
    let _ = Box::from_raw(ptr as *mut MppBufGuard);
}

use glib;
use gstreamer as gst;

/// Create a DmaBuf GstMemory from an fd, with an MppBufGuard attached.
/// The fd is duped so the caller retains ownership of the original fd.
/// `mpp_buf` will have its refcount incremented; the guard decrements it on drop.
pub fn wrap_mpp_buffer_as_dmabuf_memory(
    allocator: &MppAllocator,
    mpp_buf: ffi::MppBuffer,
    size: usize,
) -> Option<gst::Memory> {
    unsafe {
        // Inc ref so the MppBuffer stays alive while GstMemory exists
        ffi::mpp_buffer_inc_ref(mpp_buf);

        let fd = ffi::mpp_buffer_get_fd(mpp_buf);
        let duped_fd = libc::dup(fd);
        if duped_fd < 0 {
            ffi::mpp_buffer_put(mpp_buf);
            return None;
        }

        let raw_mem = ffi::gst_fd_allocator_alloc(
            allocator.gst_dmabuf_allocator(),
            duped_fd,
            size,
            ffi::GST_FD_MEMORY_FLAG_KEEP_MAPPED,
        );
        if raw_mem.is_null() {
            libc::close(duped_fd);
            ffi::mpp_buffer_put(mpp_buf);
            return None;
        }

        let mem = gst::Memory::from_glib_full(raw_mem as *mut gst::ffi::GstMemory);

        // Attach guard that will put the MppBuffer when the memory is freed
        let guard = MppBufGuard::new(mpp_buf);
        attach_mpp_buf_guard(&mem, guard);

        Some(mem)
    }
}
