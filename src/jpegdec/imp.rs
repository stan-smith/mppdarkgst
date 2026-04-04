use glib::subclass::prelude::*;
use gstreamer as gst;
use gstreamer::subclass::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::subclass::prelude::*;

use once_cell::sync::Lazy;
use std::sync::Mutex;

use crate::mpp_ffi::{self as ffi, MppApiStruct};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("mppjpegdec", gst::DebugColorFlags::empty(), Some("MPP JPEG Decoder"))
});

struct DecoderState {
    mpp_ctx: ffi::MppCtx,
    mpi: *mut MppApiStruct,
    width: u32,
    height: u32,
    hor_stride: u32,
    ver_stride: u32,
    buf_size: usize,
    input_group: ffi::MppBufferGroup,
    buf_group: ffi::MppBufferGroup,
    negotiated: bool,
}

unsafe impl Send for DecoderState {}

impl Drop for DecoderState {
    fn drop(&mut self) {
        unsafe {
            // Just reset and destroy — no EOS dance needed for JPEG decoder teardown
            if let Some(reset) = (*self.mpi).reset {
                reset(self.mpp_ctx);
            }

            if !self.buf_group.is_null() {
                ffi::mpp_buffer_group_put(self.buf_group);
            }
            if !self.input_group.is_null() {
                ffi::mpp_buffer_group_put(self.input_group);
            }

            ffi::mpp_destroy(self.mpp_ctx);
        }
    }
}

pub struct MppJpegDec {
    state: Mutex<Option<DecoderState>>,
}

impl Default for MppJpegDec {
    fn default() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for MppJpegDec {
    const NAME: &'static str = "mppjpegdec";
    type Type = super::MppJpegDec;
    type ParentType = gst_video::VideoDecoder;
}

impl ObjectImpl for MppJpegDec {}

impl GstObjectImpl for MppJpegDec {}

impl ElementImpl for MppJpegDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "MPP JPEG Decoder",
                "Codec/Decoder/Image",
                "Rockchip MPP hardware JPEG decoder",
                "simplertsp",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::builder("image/jpeg").build();

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

impl VideoDecoderImpl for MppJpegDec {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "start");

        unsafe {
            let mut mpp_ctx: ffi::MppCtx = std::ptr::null_mut();
            let mut mpi: ffi::MppApi = std::ptr::null_mut();

            if ffi::mpp_create(&mut mpp_ctx, &mut mpi) != ffi::MPP_OK {
                return Err(gst::error_msg!(gst::LibraryError::Init, ["mpp_create failed"]));
            }

            *self.state.lock().unwrap() = Some(DecoderState {
                mpp_ctx,
                mpi,
                width: 0,
                height: 0,
                hor_stride: 0,
                ver_stride: 0,
                buf_size: 0,
                input_group: std::ptr::null_mut(),
                buf_group: std::ptr::null_mut(),
                negotiated: false,
            });
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "stop");
        *self.state.lock().unwrap() = None;
        Ok(())
    }

    fn set_format(
        &self,
        state: &gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        gst::info!(CAT, imp = self, "set_format: JPEG");

        let mut dec_state = self.state.lock().unwrap();
        let dec = dec_state
            .as_mut()
            .ok_or_else(|| gst::loggable_error!(CAT, "decoder not started"))?;

        // Get dimensions from input caps
        let caps = state.caps()
            .ok_or_else(|| gst::loggable_error!(CAT, "no caps on input state"))?;
        let s = caps.structure(0)
            .ok_or_else(|| gst::loggable_error!(CAT, "no structure in caps"))?;

        let width = s.get::<i32>("width").unwrap_or(1920) as u32;
        let height = s.get::<i32>("height").unwrap_or(1080) as u32;
        let hor_stride = ffi::mpp_align(width);
        let ver_stride = ffi::mpp_align(height);
        // JPEG often decodes to YUV422 (NV16) — allocate 2 bytes/pixel to be safe
        let buf_size = (hor_stride as usize) * (ver_stride as usize) * 2;

        gst::info!(
            CAT, imp = self,
            "JPEG format: {}x{} stride={}x{} buf_size={}",
            width, height, hor_stride, ver_stride, buf_size
        );

        unsafe {
            // No PARSER_SPLIT_MODE for JPEG — each frame is self-contained
            if ffi::mpp_init(dec.mpp_ctx, ffi::MPP_CTX_DEC, ffi::MPP_VIDEO_CodingMJPEG)
                != ffi::MPP_OK
            {
                return Err(gst::loggable_error!(CAT, "mpp_init MJPEG decoder failed"));
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

            // Create internal buffer groups for input and output
            if !dec.input_group.is_null() {
                ffi::mpp_buffer_group_put(dec.input_group);
                dec.input_group = std::ptr::null_mut();
            }
            if ffi::mpp_buffer_group_get_internal(&mut dec.input_group, ffi::MPP_BUFFER_TYPE_DRM)
                != ffi::MPP_OK
            {
                return Err(gst::loggable_error!(CAT, "failed to create input buffer group"));
            }

            if !dec.buf_group.is_null() {
                ffi::mpp_buffer_group_put(dec.buf_group);
                dec.buf_group = std::ptr::null_mut();
            }
            if ffi::mpp_buffer_group_get_internal(&mut dec.buf_group, ffi::MPP_BUFFER_TYPE_DRM)
                != ffi::MPP_OK
            {
                return Err(gst::loggable_error!(CAT, "failed to create output buffer group"));
            }
        }

        dec.width = width;
        dec.height = height;
        dec.hor_stride = hor_stride;
        dec.ver_stride = ver_stride;
        dec.buf_size = buf_size;
        dec.negotiated = false;

        Ok(())
    }

    fn handle_frame(
        &self,
        mut frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Copy input data
        let input_buffer = frame.input_buffer().ok_or(gst::FlowError::Error)?;
        let input_data: Vec<u8> = {
            let map = input_buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            map.as_slice().to_vec()
        };
        let frame_number = frame.system_frame_number();

        let mut dec_state = self.state.lock().unwrap();
        let dec = dec_state.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        if dec.buf_size == 0 {
            gst::error!(CAT, imp = self, "buf_size is 0 — set_format not called?");
            return Err(gst::FlowError::NotNegotiated);
        }

        unsafe {
            // Create buffer-backed input packet (newer MPP requires MppBuffer on packet)
            let mut in_buf: ffi::MppBuffer = std::ptr::null_mut();
            if ffi::mpp_buffer_get(dec.input_group, &mut in_buf, input_data.len())
                != ffi::MPP_OK
            {
                gst::error!(CAT, imp = self, "failed to allocate input buffer");
                return Err(gst::FlowError::Error);
            }

            // Copy JPEG data into the MppBuffer
            let in_ptr = ffi::mpp_buffer_get_ptr(in_buf);
            std::ptr::copy_nonoverlapping(
                input_data.as_ptr(),
                in_ptr as *mut u8,
                input_data.len(),
            );

            let mut mpkt: ffi::MppPacket = std::ptr::null_mut();
            if ffi::mpp_packet_init_with_buffer(&mut mpkt, in_buf) != ffi::MPP_OK {
                gst::error!(CAT, imp = self, "mpp_packet_init_with_buffer failed");
                ffi::mpp_buffer_put(in_buf);
                return Err(gst::FlowError::Error);
            }
            // Set actual data length (buffer may be larger than data)
            ffi::mpp_packet_set_length(mpkt, input_data.len());
            // Release our ref on the input buffer — packet now holds it
            ffi::mpp_buffer_put(in_buf);

            gst::trace!(CAT, imp = self, "frame {}: decode {} bytes", frame_number, input_data.len());

            let poll = (*dec.mpi).poll.ok_or_else(|| {
                gst::error!(CAT, imp = self, "MPP poll function not available");
                gst::FlowError::Error
            })?;
            let dequeue = (*dec.mpi).dequeue.ok_or_else(|| {
                gst::error!(CAT, imp = self, "MPP dequeue function not available");
                gst::FlowError::Error
            })?;
            let enqueue = (*dec.mpi).enqueue.ok_or_else(|| {
                gst::error!(CAT, imp = self, "MPP enqueue function not available");
                gst::FlowError::Error
            })?;

            // === INPUT: poll → dequeue → set meta → enqueue ===

            let ret = poll(dec.mpp_ctx, ffi::MPP_PORT_INPUT, ffi::MPP_POLL_BLOCK);
            if ret != ffi::MPP_OK {
                gst::error!(CAT, imp = self, "poll INPUT failed: ret={}", ret);
                ffi::mpp_packet_deinit(&mut mpkt);
                return Err(gst::FlowError::Error);
            }

            let mut task: ffi::MppTask = std::ptr::null_mut();
            let ret = dequeue(
                dec.mpp_ctx,
                ffi::MPP_PORT_INPUT,
                &mut task as *mut _ as *mut std::os::raw::c_void,
            );
            if ret != ffi::MPP_OK || task.is_null() {
                gst::error!(CAT, imp = self, "dequeue INPUT failed: ret={}", ret);
                ffi::mpp_packet_deinit(&mut mpkt);
                return Err(gst::FlowError::Error);
            }

            // Set input packet on task
            ffi::mpp_task_meta_set_packet(task, ffi::KEY_INPUT_PACKET, mpkt);

            // Allocate output buffer and frame
            let mut out_buf: ffi::MppBuffer = std::ptr::null_mut();
            if ffi::mpp_buffer_get(dec.buf_group, &mut out_buf, dec.buf_size) != ffi::MPP_OK {
                gst::error!(CAT, imp = self, "failed to allocate output buffer");
                ffi::mpp_packet_deinit(&mut mpkt);
                return Err(gst::FlowError::Error);
            }

            let mut out_frame: ffi::MppFrame = std::ptr::null_mut();
            ffi::mpp_frame_init(&mut out_frame);
            ffi::mpp_frame_set_buffer(out_frame, out_buf);
            // Release our ref — the frame now holds the buffer ref
            ffi::mpp_buffer_put(out_buf);

            // Cross-reference: set packet in frame's meta (required by MPP advanced thread)
            let frame_meta = ffi::mpp_frame_get_meta(out_frame);
            if !frame_meta.is_null() {
                ffi::mpp_meta_set_packet(frame_meta, ffi::KEY_INPUT_PACKET, mpkt);
            }

            // Set output frame on task
            ffi::mpp_task_meta_set_frame(task, ffi::KEY_OUTPUT_FRAME, out_frame);

            // Enqueue input task
            let ret = enqueue(dec.mpp_ctx, ffi::MPP_PORT_INPUT, task);
            if ret != ffi::MPP_OK {
                gst::error!(CAT, imp = self, "enqueue INPUT failed: ret={}", ret);
                ffi::mpp_frame_deinit(&mut out_frame);
                ffi::mpp_packet_deinit(&mut mpkt);
                return Err(gst::FlowError::Error);
            }

            // === OUTPUT: poll → dequeue → get meta → enqueue ===

            let ret = poll(dec.mpp_ctx, ffi::MPP_PORT_OUTPUT, ffi::MPP_POLL_BLOCK);
            if ret != ffi::MPP_OK {
                gst::error!(CAT, imp = self, "poll OUTPUT failed: ret={}", ret);
                ffi::mpp_packet_deinit(&mut mpkt);
                return Err(gst::FlowError::Error);
            }

            let mut out_task: ffi::MppTask = std::ptr::null_mut();
            let ret = dequeue(
                dec.mpp_ctx,
                ffi::MPP_PORT_OUTPUT,
                &mut out_task as *mut _ as *mut std::os::raw::c_void,
            );
            if ret != ffi::MPP_OK || out_task.is_null() {
                gst::error!(CAT, imp = self, "dequeue OUTPUT failed: ret={}", ret);
                ffi::mpp_packet_deinit(&mut mpkt);
                return Err(gst::FlowError::Error);
            }

            let mut decoded_frame: ffi::MppFrame = std::ptr::null_mut();
            ffi::mpp_task_meta_get_frame(out_task, ffi::KEY_OUTPUT_FRAME, &mut decoded_frame);

            // Clean up: get packet from frame meta and deinit it
            if !decoded_frame.is_null() {
                let dec_meta = ffi::mpp_frame_get_meta(decoded_frame);
                if !dec_meta.is_null() {
                    let mut meta_pkt: ffi::MppPacket = std::ptr::null_mut();
                    ffi::mpp_meta_get_packet(dec_meta, ffi::KEY_INPUT_PACKET, &mut meta_pkt);
                    if !meta_pkt.is_null() {
                        ffi::mpp_packet_deinit(&mut meta_pkt);
                    }
                }
            }

            // Enqueue output task back (must do this even if frame is null)
            let _ = enqueue(dec.mpp_ctx, ffi::MPP_PORT_OUTPUT, out_task);

            if decoded_frame.is_null() {
                gst::warning!(CAT, imp = self, "no decoded frame from task");
                drop(frame);
                return Ok(gst::FlowSuccess::Ok);
            }

            // Check error/discard
            if ffi::mpp_frame_get_errinfo(decoded_frame) != 0
                || ffi::mpp_frame_get_discard(decoded_frame) != 0
            {
                gst::warning!(CAT, imp = self, "frame has error or discard flag");
                ffi::mpp_frame_deinit(&mut decoded_frame);
                drop(frame);
                return Ok(gst::FlowSuccess::Ok);
            }

            // Update dimensions from decoded frame (MPP may adjust strides)
            let actual_w = ffi::mpp_frame_get_width(decoded_frame);
            let actual_h = ffi::mpp_frame_get_height(decoded_frame);
            let actual_hs = ffi::mpp_frame_get_hor_stride(decoded_frame);
            let actual_vs = ffi::mpp_frame_get_ver_stride(decoded_frame);
            let actual_fmt = ffi::mpp_frame_get_fmt(decoded_frame) & ffi::MPP_FRAME_FMT_MASK;

            gst::debug!(
                CAT, imp = self,
                "frame {}: decoded {}x{} stride={}x{} fmt={}",
                frame_number, actual_w, actual_h, actual_hs, actual_vs, actual_fmt
            );

            if !dec.negotiated || actual_w != dec.width || actual_h != dec.height {
                dec.width = actual_w;
                dec.height = actual_h;
                dec.hor_stride = actual_hs;
                dec.ver_stride = actual_vs;
                dec.negotiated = true;

                gst::info!(
                    CAT, imp = self,
                    "negotiate: {}x{} stride={}x{}",
                    actual_w, actual_h, actual_hs, actual_vs
                );

                let w = dec.width;
                let h = dec.height;
                drop(dec_state);
                Self::negotiate_output(&self.obj(), w, h);
                dec_state = self.state.lock().unwrap();
            }

            let dec = dec_state.as_ref().ok_or(gst::FlowError::Error)?;

            let mpp_buf = ffi::mpp_frame_get_buffer(decoded_frame);
            if mpp_buf.is_null() {
                gst::warning!(CAT, imp = self, "decoded frame has no buffer");
                ffi::mpp_frame_deinit(&mut decoded_frame);
                drop(frame);
                return Ok(gst::FlowSuccess::Ok);
            }

            let output_buffer = Self::copy_decoded_frame(
                mpp_buf,
                dec.width,
                dec.height,
                dec.hor_stride,
                dec.ver_stride,
            );
            ffi::mpp_frame_deinit(&mut decoded_frame);

            if let Some(buf) = output_buffer {
                drop(dec_state);
                frame.set_output_buffer(buf);
                return gst_video::prelude::VideoDecoderExt::finish_frame(
                    &*self.obj(),
                    frame,
                );
            }

            drop(frame);
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn flush(&self) -> bool {
        gst::debug!(CAT, imp = self, "flush");

        let mut guard = self.state.lock().unwrap();
        if let Some(ref mut dec) = *guard {
            unsafe {
                if let Some(reset) = (*dec.mpi).reset {
                    reset(dec.mpp_ctx);
                }
            }
            dec.negotiated = false;
        }

        true
    }
}

impl MppJpegDec {
    fn negotiate_output(element: &super::MppJpegDec, width: u32, height: u32) {
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

    /// Copy decoded NV12 data from stride-aligned MppBuffer into packed GstBuffer.
    /// Strips stride padding so downstream (encoder) gets standard packed NV12.
    unsafe fn copy_decoded_frame(
        mpp_buf: ffi::MppBuffer,
        width: u32,
        height: u32,
        hor_stride: u32,
        ver_stride: u32,
    ) -> Option<gst::Buffer> {
        let w = width as usize;
        let h = height as usize;
        let hs = hor_stride as usize;
        let vs = ver_stride as usize;
        let packed_size = w * h * 3 / 2;

        let src = ffi::mpp_buffer_get_ptr(mpp_buf) as *const u8;
        if src.is_null() {
            return None;
        }

        let mut buffer = gst::Buffer::with_size(packed_size).ok()?;
        {
            let buf_mut = buffer.get_mut().unwrap();
            let mut map = buf_mut.map_writable().ok()?;
            let dst = map.as_mut_slice();

            // Copy Y plane: strip hor_stride padding per row
            if w == hs {
                // No stride padding — bulk copy Y
                std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w * h);
            } else {
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(
                        src.add(y * hs),
                        dst.as_mut_ptr().add(y * w),
                        w,
                    );
                }
            }

            // Copy UV plane from ver_stride*hor_stride offset in source
            // to height*width offset in dest (packed layout)
            let src_uv = src.add(vs * hs);
            let dst_uv_off = w * h;
            let uv_h = h / 2;

            if w == hs {
                std::ptr::copy_nonoverlapping(src_uv, dst.as_mut_ptr().add(dst_uv_off), w * uv_h);
            } else {
                for y in 0..uv_h {
                    std::ptr::copy_nonoverlapping(
                        src_uv.add(y * hs),
                        dst.as_mut_ptr().add(dst_uv_off + y * w),
                        w,
                    );
                }
            }
        }
        Some(buffer)
    }
}
