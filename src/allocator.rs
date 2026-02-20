#![allow(dead_code)]
//! MppAllocator — manages MPP buffer groups for DmaBuf allocation and import.
//!
//! Wraps two `MppBufferGroup`s:
//! - `group` (INTERNAL, DRM) for fresh allocations
//! - `ext_group` (EXTERNAL, DRM) for importing foreign DmaBuf fds
//!
//! This is a Rust-side helper, not a GObject subclass. The actual GstMemory
//! objects are created using `gst_fd_allocator_alloc` from the GStreamer
//! DmaBuf allocator via raw FFI.

use crate::mpp_ffi as ffi;
use std::sync::atomic::{AtomicI32, Ordering};

static ALLOC_INDEX: AtomicI32 = AtomicI32::new(0);

pub struct MppAllocator {
    group: ffi::MppBufferGroup,
    ext_group: ffi::MppBufferGroup,
    index: i32,
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

            let mut ext_group: ffi::MppBufferGroup = std::ptr::null_mut();
            if ffi::mpp_buffer_group_get_external(&mut ext_group, ffi::MPP_BUFFER_TYPE_DRM)
                != ffi::MPP_OK
            {
                ffi::mpp_buffer_group_put(group);
                return Err(());
            }

            let dmabuf_alloc = ffi::gst_dmabuf_allocator_new();
            if dmabuf_alloc.is_null() {
                ffi::mpp_buffer_group_put(ext_group);
                ffi::mpp_buffer_group_put(group);
                return Err(());
            }

            let index = ALLOC_INDEX.fetch_add(1, Ordering::Relaxed);

            Ok(Self {
                group,
                ext_group,
                index,
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
            ffi::mpp_buffer_set_index(mbuf, self.index);
            let fd = ffi::mpp_buffer_get_fd(mbuf);
            Ok((mbuf, fd))
        }
    }

    /// Import an MppBuffer (e.g. from decoder output or encoder output).
    /// If the buffer's index matches ours, we dup the fd directly.
    /// Otherwise we import via ext_group.
    /// Increments refcount. Caller must eventually call mpp_buffer_put.
    pub fn import_mpp_buffer(&self, mbuf: ffi::MppBuffer) -> Result<(ffi::MppBuffer, i32), ()> {
        unsafe {
            let fd = ffi::mpp_buffer_get_fd(mbuf);
            if fd < 0 {
                return Err(());
            }

            // Page-align size (DRM buffers are aligned to 4096)
            let size = (ffi::mpp_buffer_get_size(mbuf) + 4095) & !4095;

            if ffi::mpp_buffer_get_index(mbuf) != self.index {
                // Import from other group via ext_group
                let info = ffi::MppBufferInfo {
                    buf_type: ffi::MPP_BUFFER_TYPE_DRM,
                    size,
                    fd,
                    ..Default::default()
                };
                let mut imported: ffi::MppBuffer = std::ptr::null_mut();
                if ffi::mpp_buffer_import(self.ext_group, &info, &mut imported) != ffi::MPP_OK {
                    return Err(());
                }
                ffi::mpp_buffer_set_index(imported, self.index);
                let imported_fd = ffi::mpp_buffer_get_fd(imported);
                // Also inc_ref the original so it stays alive
                ffi::mpp_buffer_inc_ref(mbuf);
                Ok((imported, imported_fd))
            } else {
                // Same group: just inc_ref and dup fd
                ffi::mpp_buffer_inc_ref(mbuf);
                Ok((mbuf, fd))
            }
        }
    }

    /// Import a foreign DmaBuf fd. Creates an MppBuffer in ext_group.
    pub fn import_dmabuf_fd(&self, fd: i32, size: usize) -> Result<ffi::MppBuffer, ()> {
        unsafe {
            let info = ffi::MppBufferInfo {
                buf_type: ffi::MPP_BUFFER_TYPE_DRM,
                size,
                fd,
                ..Default::default()
            };
            let mut mbuf: ffi::MppBuffer = std::ptr::null_mut();
            if ffi::mpp_buffer_import(self.ext_group, &info, &mut mbuf) != ffi::MPP_OK {
                return Err(());
            }
            ffi::mpp_buffer_set_index(mbuf, self.index);
            Ok(mbuf)
        }
    }

    /// Get the MPP buffer group (for MPP_DEC_SET_EXT_BUF_GROUP).
    pub fn mpp_group(&self) -> ffi::MppBufferGroup {
        self.group
    }

    pub fn index(&self) -> i32 {
        self.index
    }

    /// Clear external cached buffers.
    pub fn clear_external(&self) {
        unsafe {
            ffi::mpp_buffer_group_clear(self.ext_group);
        }
    }

    /// Get the raw GstDmaBufAllocator pointer.
    pub fn gst_dmabuf_allocator(&self) -> *mut ffi::GstAllocator {
        self.dmabuf_alloc
    }
}

impl Drop for MppAllocator {
    fn drop(&mut self) {
        unsafe {
            ffi::mpp_buffer_group_put(self.ext_group);
            ffi::mpp_buffer_group_put(self.group);
            // Unref the GstAllocator
            glib::gobject_ffi::g_object_unref(self.dmabuf_alloc as *mut _);
        }
    }
}

// ---------------------------------------------------------------------------
// MppBufGuard — ensures MppBuffer refcount is decremented when GstBuffer
// is freed. Attached as qdata on GstMemory via gst_mini_object_set_qdata.
// ---------------------------------------------------------------------------

/// Guard that calls mpp_buffer_put on drop.
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

/// Extract MppBuffer from qdata on a GstMemoryRef (if it was attached by us).
pub fn mpp_buffer_from_gst_memory_ref(mem: &gst::MemoryRef) -> Option<ffi::MppBuffer> {
    mpp_buffer_from_qdata(mem.as_ptr() as *mut gst::ffi::GstMiniObject)
}

fn mpp_buffer_from_qdata(mem_ptr: *mut gst::ffi::GstMiniObject) -> Option<ffi::MppBuffer> {
    use glib::translate::IntoGlib;

    static MPP_BUF_QUARK: std::sync::OnceLock<glib::Quark> = std::sync::OnceLock::new();
    let quark = *MPP_BUF_QUARK.get_or_init(|| glib::Quark::from_str("mpp-buf-guard"));

    unsafe {
        let ptr = gst::ffi::gst_mini_object_get_qdata(mem_ptr, quark.into_glib());
        if ptr.is_null() {
            None
        } else {
            let guard = &*(ptr as *const MppBufGuard);
            Some(guard.mpp_buf)
        }
    }
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
