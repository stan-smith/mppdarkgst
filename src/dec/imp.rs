use glib::subclass::prelude::*;
use glib::translate::*;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::subclass::prelude::*;

use once_cell::sync::Lazy;
use std::sync::{Arc, Mutex};

use crate::allocator::{self, MppAllocator};
use crate::mpp_ffi::{self as ffi, MppApiStruct};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("mppvideodec", gst::DebugColorFlags::empty(), Some("MPP Video Decoder"))
});

struct DecoderState {
    mpp_ctx: ffi::MppCtx,
    mpi: *mut MppApiStruct,
    codec: i32,
    negotiated: bool,
    width: u32,
    height: u32,
    hor_stride: u32,
    ver_stride: u32,
    allocator: MppAllocator,
}

unsafe impl Send for DecoderState {}

impl Drop for DecoderState {
    fn drop(&mut self) {
        unsafe {
            let mut eos_pkt: ffi::MppPacket = std::ptr::null_mut();
            if ffi::mpp_packet_init(&mut eos_pkt, std::ptr::null(), 0) == ffi::MPP_OK {
                ffi::mpp_packet_set_eos(eos_pkt);
                if let Some(put_packet) = (*self.mpi).decode_put_packet {
                    let _ = put_packet(self.mpp_ctx, eos_pkt);
                }
                ffi::mpp_packet_deinit(&mut eos_pkt);
            }
            if let Some(get_frame) = (*self.mpi).decode_get_frame {
                let mut frame: ffi::MppFrame = std::ptr::null_mut();
                for _ in 0..10 {
                    let ret = get_frame(self.mpp_ctx, &mut frame);
                    if ret != ffi::MPP_OK || frame.is_null() {
                        break;
                    }
                    ffi::mpp_frame_deinit(&mut frame);
                }
            }
            if let Some(reset) = (*self.mpi).reset {
                reset(self.mpp_ctx);
            }
            ffi::mpp_destroy(self.mpp_ctx);
        }
    }
}

/// Shared state between handle_frame (pipeline thread) and dec_loop (srcpad task).
struct TaskShared {
    flushing: bool,
    task_ret: Result<gst::FlowSuccess, gst::FlowError>,
    task_started: bool,
}

// Safety: TaskShared contains only primitive types + FlowError (Send+Sync).
unsafe impl Send for TaskShared {}
unsafe impl Sync for TaskShared {}

pub struct MppVideoDec {
    state: Mutex<Option<DecoderState>>,
    shared: Arc<Mutex<TaskShared>>,
}

impl Default for MppVideoDec {
    fn default() -> Self {
        Self {
            state: Mutex::new(None),
            shared: Arc::new(Mutex::new(TaskShared {
                flushing: false,
                task_ret: Ok(gst::FlowSuccess::Ok),
                task_started: false,
            })),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for MppVideoDec {
    const NAME: &'static str = "mppvideodec";
    type Type = super::MppVideoDec;
    type ParentType = gst_video::VideoDecoder;
}

impl ObjectImpl for MppVideoDec {}

impl GstObjectImpl for MppVideoDec {}

impl ElementImpl for MppVideoDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "MPP Video Decoder",
                "Codec/Decoder/Video",
                "Rockchip MPP hardware video decoder (H.264/H.265)",
                "simplertsp",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::builder_full()
                .structure(
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", "byte-stream")
                        .build(),
                )
                .structure(
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", "byte-stream")
                        .build(),
                )
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Nv12)
                .build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl VideoDecoderImpl for MppVideoDec {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "start");

        // Reset shared state
        {
            let mut shared = self.shared.lock().unwrap();
            shared.flushing = false;
            shared.task_ret = Ok(gst::FlowSuccess::Ok);
            shared.task_started = false;
        }

        unsafe {
            let mut mpp_ctx: ffi::MppCtx = std::ptr::null_mut();
            let mut mpi: ffi::MppApi = std::ptr::null_mut();

            if ffi::mpp_create(&mut mpp_ctx, &mut mpi) != ffi::MPP_OK {
                return Err(gst::error_msg!(gst::LibraryError::Init, ["mpp_create failed"]));
            }

            let allocator = MppAllocator::new().map_err(|_| {
                ffi::mpp_destroy(mpp_ctx);
                gst::error_msg!(gst::LibraryError::Init, ["MppAllocator::new failed"])
            })?;

            // Tell MPP to use our DRM buffer group for decoded output (zero-copy)
            if let Some(control) = (*mpi).control {
                let group = allocator.mpp_group();
                control(mpp_ctx, ffi::MPP_DEC_SET_EXT_BUF_GROUP, group as *mut _);
            }

            *self.state.lock().unwrap() = Some(DecoderState {
                mpp_ctx,
                mpi,
                codec: ffi::MPP_VIDEO_CodingUnused,
                negotiated: false,
                width: 0,
                height: 0,
                hor_stride: 0,
                ver_stride: 0,
                allocator,
            });
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "stop");

        // Signal flushing to stop the task
        {
            let mut shared = self.shared.lock().unwrap();
            shared.flushing = true;
        }

        // Stop the srcpad task
        let obj = self.obj();
        let src_pad = gst_video::prelude::VideoDecoderExtManual::src_pad(&*obj);
        let _ = src_pad.stop_task();

        *self.state.lock().unwrap() = None;
        Ok(())
    }

    fn set_format(
        &self,
        state: &gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        let caps = state.caps().ok_or_else(|| gst::loggable_error!(CAT, "caps is None"))?;
        let structure = caps
            .structure(0)
            .ok_or_else(|| gst::loggable_error!(CAT, "caps has no structure"))?;

        let codec = match structure.name().as_str() {
            "video/x-h264" => ffi::MPP_VIDEO_CodingAVC,
            "video/x-h265" => ffi::MPP_VIDEO_CodingHEVC,
            name => {
                return Err(gst::loggable_error!(CAT, "unsupported codec: {}", name));
            }
        };

        gst::info!(CAT, imp = self, "set_format: codec={}", if codec == ffi::MPP_VIDEO_CodingAVC { "H.264" } else { "H.265" });

        let mut dec_state = self.state.lock().unwrap();
        let dec = dec_state.as_mut().ok_or_else(|| {
            gst::loggable_error!(CAT, "decoder not started")
        })?;

        unsafe {
            if let Some(control) = (*dec.mpi).control {
                // CRITICAL: set parser split mode BEFORE mpp_init
                let mut split_mode: i32 = 1;
                control(
                    dec.mpp_ctx,
                    ffi::MPP_DEC_SET_PARSER_SPLIT_MODE,
                    &mut split_mode as *mut i32 as ffi::MppParam,
                );
                let mut fast_mode: i32 = 1;
                control(
                    dec.mpp_ctx,
                    ffi::MPP_DEC_SET_PARSER_FAST_MODE,
                    &mut fast_mode as *mut i32 as ffi::MppParam,
                );
            }

            if ffi::mpp_init(dec.mpp_ctx, ffi::MPP_CTX_DEC, codec) != ffi::MPP_OK {
                return Err(gst::loggable_error!(CAT, "mpp_init decoder failed"));
            }

            if let Some(control) = (*dec.mpi).control {
                control(
                    dec.mpp_ctx,
                    ffi::MPP_DEC_SET_DISABLE_ERROR,
                    std::ptr::null_mut(),
                );

                // 200ms output timeout
                let mut timeout: i64 = 200;
                control(
                    dec.mpp_ctx,
                    ffi::MPP_SET_OUTPUT_TIMEOUT,
                    &mut timeout as *mut i64 as ffi::MppParam,
                );
            }
        }

        dec.codec = codec;
        dec.negotiated = false;

        Ok(())
    }

    fn handle_frame(
        &self,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Check flushing
        {
            let shared = self.shared.lock().unwrap();
            if shared.flushing {
                return Err(gst::FlowError::Flushing);
            }
        }

        // Get stream lock pointer for explicit lock management
        let stream_lock = unsafe {
            let obj = self.obj();
            let decoder_ptr: *const gst_video::ffi::GstVideoDecoder =
                obj.upcast_ref::<gst_video::VideoDecoder>().to_glib_none().0;
            &(*decoder_ptr).stream_lock as *const glib::ffi::GRecMutex as *mut glib::ffi::GRecMutex
        };

        // Start srcpad task if not already running
        {
            let mut shared = self.shared.lock().unwrap();
            if !shared.task_started {
                let obj = self.obj();
                let src_pad = gst_video::prelude::VideoDecoderExtManual::src_pad(&*obj);

                let element = obj.clone();
                let task_shared = Arc::clone(&self.shared);
                src_pad
                    .start_task(move || {
                        Self::dec_loop(&element, &task_shared);
                    })
                    .map_err(|_| {
                        gst::error!(CAT, "Failed to start srcpad task");
                        gst::FlowError::Error
                    })?;
                shared.task_started = true;
                gst::debug!(CAT, imp = self, "started srcpad decoding task");
            }
        }

        // Copy input data and PTS, then drop frame to release stream lock ref
        let input_buffer = frame.input_buffer().ok_or(gst::FlowError::Error)?;
        let input_data: Vec<u8> = {
            let map = input_buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            map.as_slice().to_vec()
        };
        let pts = frame.pts();
        drop(frame);

        let (mpp_ctx, put_packet_fn) = {
            let dec_state = self.state.lock().unwrap();
            let dec = dec_state.as_ref().ok_or(gst::FlowError::NotNegotiated)?;
            let put_packet = unsafe { (*dec.mpi).decode_put_packet.ok_or(gst::FlowError::Error)? };
            (dec.mpp_ctx, put_packet)
        };

        unsafe {
            let mut mpkt: ffi::MppPacket = std::ptr::null_mut();
            if ffi::mpp_packet_init(&mut mpkt, input_data.as_ptr() as *const _, input_data.len())
                != ffi::MPP_OK
            {
                gst::error!(CAT, imp = self, "mpp_packet_init failed");
                return Err(gst::FlowError::Error);
            }

            if let Some(pts) = pts {
                ffi::mpp_packet_set_pts(mpkt, pts.nseconds() as i64);
            }

            // Submit packet to MPP, releasing stream lock during blocking send
            let mut retries = 0;
            loop {
                // Release stream lock so srcpad task can run
                glib::ffi::g_rec_mutex_unlock(stream_lock);

                let ret = put_packet_fn(mpp_ctx, mpkt);

                // Re-acquire stream lock
                glib::ffi::g_rec_mutex_lock(stream_lock);

                if ret == ffi::MPP_OK {
                    break;
                }

                retries += 1;
                if retries > 500 {
                    gst::error!(CAT, imp = self, "decode_put_packet timeout");
                    ffi::mpp_packet_deinit(&mut mpkt);
                    return Err(gst::FlowError::Error);
                }

                // Check flushing
                {
                    let shared = self.shared.lock().unwrap();
                    if shared.flushing {
                        ffi::mpp_packet_deinit(&mut mpkt);
                        return Err(gst::FlowError::Flushing);
                    }
                }

                // Brief sleep before retry (release stream lock during sleep)
                glib::ffi::g_rec_mutex_unlock(stream_lock);
                std::thread::sleep(std::time::Duration::from_millis(2));
                glib::ffi::g_rec_mutex_lock(stream_lock);
            }

            ffi::mpp_packet_deinit(&mut mpkt);
        }

        // Return the task's flow status
        let shared = self.shared.lock().unwrap();
        shared.task_ret.clone()
    }

    fn flush(&self) -> bool {
        gst::debug!(CAT, imp = self, "flush");

        // Signal flushing
        {
            let mut shared = self.shared.lock().unwrap();
            shared.flushing = true;
        }

        // Pause the task
        let obj = self.obj();
        let src_pad = gst_video::prelude::VideoDecoderExtManual::src_pad(&*obj);
        let _ = src_pad.pause_task();

        // Reset MPP
        let guard = self.state.lock().unwrap();
        if let Some(ref dec) = *guard {
            unsafe {
                if let Some(reset) = (*dec.mpi).reset {
                    reset(dec.mpp_ctx);
                }
            }
        }
        drop(guard);

        // Clear flushing state
        {
            let mut shared = self.shared.lock().unwrap();
            shared.flushing = false;
            shared.task_ret = Ok(gst::FlowSuccess::Ok);
            shared.task_started = false;
        }

        true
    }
}

impl MppVideoDec {
    /// Srcpad task loop — runs in a dedicated thread.
    fn dec_loop(
        element: &super::MppVideoDec,
        shared: &Arc<Mutex<TaskShared>>,
    ) {
        let imp = element.imp();

        // Check flushing before polling
        {
            let shared = shared.lock().unwrap();
            if shared.flushing {
                return;
            }
        }

        // Poll for a decoded frame (no stream lock held)
        let mpp_frame = {
            let dec_state = imp.state.lock().unwrap();
            let Some(ref dec) = *dec_state else {
                return;
            };

            let get_frame_fn = match unsafe { (*dec.mpi).decode_get_frame } {
                Some(f) => f,
                None => return,
            };

            let mut mpp_frame: ffi::MppFrame = std::ptr::null_mut();
            let mpp_ctx = dec.mpp_ctx;

            drop(dec_state); // Release state lock before blocking call

            unsafe { get_frame_fn(mpp_ctx, &mut mpp_frame); }

            if mpp_frame.is_null() {
                // Timeout — just return, task will be called again
                return;
            }

            mpp_frame
        };

        // Acquire stream lock
        let stream_lock = unsafe {
            let decoder_ptr: *const gst_video::ffi::GstVideoDecoder =
                element.upcast_ref::<gst_video::VideoDecoder>().to_glib_none().0;
            &(*decoder_ptr).stream_lock as *const glib::ffi::GRecMutex as *mut glib::ffi::GRecMutex
        };
        unsafe { glib::ffi::g_rec_mutex_lock(stream_lock); }

        // Check EOS
        if unsafe { ffi::mpp_frame_get_eos(mpp_frame) } != 0 {
            unsafe { ffi::mpp_frame_deinit(&mut { mpp_frame }); }
            {
                let mut shared = shared.lock().unwrap();
                shared.task_ret = Err(gst::FlowError::Eos);
            }
            let src_pad = gst_video::prelude::VideoDecoderExtManual::src_pad(element);
            let _ = src_pad.pause_task();
            unsafe { glib::ffi::g_rec_mutex_unlock(stream_lock); }
            return;
        }

        // Handle info change
        if unsafe { ffi::mpp_frame_get_info_change(mpp_frame) } != 0 {
            let width = unsafe { ffi::mpp_frame_get_width(mpp_frame) };
            let height = unsafe { ffi::mpp_frame_get_height(mpp_frame) };
            let hor_stride = unsafe { ffi::mpp_frame_get_hor_stride(mpp_frame) };
            let ver_stride = unsafe { ffi::mpp_frame_get_ver_stride(mpp_frame) };

            gst::info!(
                CAT, obj = element,
                "info_change: {}x{} stride={}x{}",
                width, height, hor_stride, ver_stride
            );

            {
                let mut dec_state = imp.state.lock().unwrap();
                if let Some(ref mut dec) = *dec_state {
                    dec.width = width;
                    dec.height = height;
                    dec.hor_stride = hor_stride;
                    dec.ver_stride = ver_stride;
                    dec.negotiated = true;

                    // Acknowledge the info change
                    unsafe {
                        if let Some(control) = (*dec.mpi).control {
                            control(
                                dec.mpp_ctx,
                                ffi::MPP_DEC_SET_INFO_CHANGE_READY,
                                std::ptr::null_mut(),
                            );
                        }
                    }
                }
            }

            unsafe { ffi::mpp_frame_deinit(&mut { mpp_frame }); }

            // Negotiate output caps
            Self::negotiate_output(element, width, height);

            unsafe { glib::ffi::g_rec_mutex_unlock(stream_lock); }
            return;
        }

        // Check for errors/discard
        if unsafe { ffi::mpp_frame_get_errinfo(mpp_frame) } != 0
            || unsafe { ffi::mpp_frame_get_discard(mpp_frame) } != 0
        {
            gst::warning!(CAT, obj = element, "frame has error or discard flag");
            unsafe { ffi::mpp_frame_deinit(&mut { mpp_frame }); }
            unsafe { glib::ffi::g_rec_mutex_unlock(stream_lock); }
            return;
        }

        let mpp_buf = unsafe { ffi::mpp_frame_get_buffer(mpp_frame) };
        if mpp_buf.is_null() {
            gst::warning!(CAT, obj = element, "decoded frame has no buffer");
            unsafe { ffi::mpp_frame_deinit(&mut { mpp_frame }); }
            unsafe { glib::ffi::g_rec_mutex_unlock(stream_lock); }
            return;
        }

        // Get dimensions and wrap decoded buffer as zero-copy DMA-BUF
        let output_buffer = {
            let mut dec_state = imp.state.lock().unwrap();
            let dec = match dec_state.as_mut() {
                Some(d) => d,
                None => {
                    unsafe {
                        ffi::mpp_frame_deinit(&mut { mpp_frame });
                        glib::ffi::g_rec_mutex_unlock(stream_lock);
                    }
                    return;
                }
            };

            if !dec.negotiated {
                dec.width = unsafe { ffi::mpp_frame_get_width(mpp_frame) };
                dec.height = unsafe { ffi::mpp_frame_get_height(mpp_frame) };
                dec.hor_stride = unsafe { ffi::mpp_frame_get_hor_stride(mpp_frame) };
                dec.ver_stride = unsafe { ffi::mpp_frame_get_ver_stride(mpp_frame) };
                dec.negotiated = true;

                let w = dec.width;
                let h = dec.height;
                drop(dec_state);
                Self::negotiate_output(element, w, h);
                dec_state = imp.state.lock().unwrap();
            }

            let dec = dec_state.as_ref().unwrap();
            let buf = unsafe {
                Self::wrap_decoded_frame(
                    &dec.allocator, mpp_buf,
                    dec.width, dec.height, dec.hor_stride, dec.ver_stride,
                )
            };
            drop(dec_state);
            buf
        };
        unsafe { ffi::mpp_frame_deinit(&mut { mpp_frame }); }

        // Finish the frame with decoded output
        if let Some(buf) = output_buffer {
            let oldest = gst_video::prelude::VideoDecoderExtManual::oldest_frame(element);
            if let Some(mut f) = oldest {
                f.set_output_buffer(buf);
                let ret = gst_video::prelude::VideoDecoderExt::finish_frame(element, f);
                if let Err(e) = ret {
                    let mut shared = shared.lock().unwrap();
                    shared.task_ret = Err(e);
                    let src_pad = gst_video::prelude::VideoDecoderExtManual::src_pad(element);
                    let _ = src_pad.pause_task();
                }
            }
        }

        // Check if we should stop
        {
            let shared = shared.lock().unwrap();
            if shared.task_ret.is_err() || shared.flushing {
                let src_pad = gst_video::prelude::VideoDecoderExtManual::src_pad(element);
                let _ = src_pad.pause_task();
            }
        }

        unsafe { glib::ffi::g_rec_mutex_unlock(stream_lock); }
    }

    fn negotiate_output(element: &super::MppVideoDec, width: u32, height: u32) {
        let result = gst_video::prelude::VideoDecoderExtManual::set_output_state(
            element,
            gst_video::VideoFormat::Nv12,
            width,
            height,
            None,
        );
        if let Ok(output_state) = result {
            let _ = gst_video::prelude::VideoDecoderExtManual::negotiate(element, output_state);
        }
    }

    /// Wrap decoded NV12 MppBuffer as zero-copy DMA-BUF GstBuffer with VideoMeta.
    unsafe fn wrap_decoded_frame(
        alloc: &MppAllocator,
        mpp_buf: ffi::MppBuffer,
        width: u32,
        height: u32,
        hor_stride: u32,
        ver_stride: u32,
    ) -> Option<gst::Buffer> {
        let buf_size = (hor_stride as usize) * (ver_stride as usize) * 3 / 2;

        let mem = allocator::wrap_mpp_buffer_as_dmabuf_memory(alloc, mpp_buf, buf_size)?;

        let mut buffer = gst::Buffer::new();
        {
            let buf_mut = buffer.get_mut().unwrap();
            buf_mut.append_memory(mem);

            // NV12: plane 0 = Y (offset 0, stride = hor_stride)
            //       plane 1 = UV (offset = hor_stride * ver_stride, stride = hor_stride)
            let offsets = [0usize, (hor_stride * ver_stride) as usize];
            let strides = [hor_stride as i32, hor_stride as i32];

            let _ = gst_video::VideoMeta::add_full(
                buf_mut,
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Nv12,
                width,
                height,
                &offsets,
                &strides,
            );
        }
        Some(buffer)
    }
}
