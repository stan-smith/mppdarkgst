use glib::subclass::prelude::*;
use gstreamer as gst;
use gstreamer::subclass::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::subclass::prelude::*;

use once_cell::sync::Lazy;
use std::sync::Mutex;

use crate::mpp_ffi::{self as ffi, MppApiStruct};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("mppvideodec", gst::DebugColorFlags::empty(), Some("MPP Video Decoder"))
});

// ---------------------------------------------------------------------------
// Decoder state (only present while running)
// ---------------------------------------------------------------------------

struct DecoderState {
    mpp_ctx: ffi::MppCtx,
    mpi: *mut MppApiStruct,
    codec: i32,
    negotiated: bool,
    width: u32,
    height: u32,
    hor_stride: u32,
    ver_stride: u32,
}

unsafe impl Send for DecoderState {}

impl Drop for DecoderState {
    fn drop(&mut self) {
        unsafe {
            // Send EOS to MPP and drain remaining frames
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

// ---------------------------------------------------------------------------
// GObject subclass
// ---------------------------------------------------------------------------

pub struct MppVideoDec {
    state: Mutex<Option<DecoderState>>,
}

impl Default for MppVideoDec {
    fn default() -> Self {
        Self {
            state: Mutex::new(None),
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

        unsafe {
            let mut mpp_ctx: ffi::MppCtx = std::ptr::null_mut();
            let mut mpi: ffi::MppApi = std::ptr::null_mut();

            if ffi::mpp_create(&mut mpp_ctx, &mut mpi) != ffi::MPP_OK {
                return Err(gst::error_msg!(gst::LibraryError::Init, ["mpp_create failed"]));
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

                // Set output timeout to 200ms
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
        gst::debug!(CAT, imp = self, "handle_frame: frame #{}", frame.system_frame_number());

        {
            let mut dec_state = self.state.lock().unwrap();
            let dec = dec_state.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

            let input_buffer = frame.input_buffer().ok_or(gst::FlowError::Error)?;
            let map = input_buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let input_data = map.as_slice();

            unsafe {
                let mut mpkt: ffi::MppPacket = std::ptr::null_mut();
                if ffi::mpp_packet_init(&mut mpkt, input_data.as_ptr() as *const _, input_data.len())
                    != ffi::MPP_OK
                {
                    gst::error!(CAT, imp = self, "mpp_packet_init failed");
                    return Err(gst::FlowError::Error);
                }

                if let Some(pts) = frame.pts() {
                    ffi::mpp_packet_set_pts(mpkt, pts.nseconds() as i64);
                }

                let put_packet = (*dec.mpi)
                    .decode_put_packet
                    .ok_or(gst::FlowError::Error)?;

                // Submit packet to MPP (retry if MPP queue is full)
                let mut retries = 0;
                loop {
                    let ret = put_packet(dec.mpp_ctx, mpkt);
                    if ret == ffi::MPP_OK {
                        break;
                    }
                    retries += 1;
                    if retries > 1000 {
                        gst::error!(CAT, imp = self, "decode_put_packet timeout");
                        ffi::mpp_packet_deinit(&mut mpkt);
                        return Err(gst::FlowError::Error);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                ffi::mpp_packet_deinit(&mut mpkt);
            }

            // Drop input references before polling
            drop(map);
        }

        // Drop frame reference — VideoDecoder holds it for us until finish_frame
        drop(frame);

        // Poll for decoded output (may produce 0 or 1+ frames per input)
        self.poll_decoded_frames()
    }

    fn finish(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, imp = self, "finish: draining decoder");

        // Send EOS packet to MPP
        {
            let dec_state = self.state.lock().unwrap();
            let dec = dec_state.as_ref().ok_or(gst::FlowError::Error)?;
            unsafe {
                let mut eos_pkt: ffi::MppPacket = std::ptr::null_mut();
                if ffi::mpp_packet_init(&mut eos_pkt, std::ptr::null(), 0) == ffi::MPP_OK {
                    ffi::mpp_packet_set_eos(eos_pkt);
                    if let Some(put_packet) = (*dec.mpi).decode_put_packet {
                        let _ = put_packet(dec.mpp_ctx, eos_pkt);
                    }
                    ffi::mpp_packet_deinit(&mut eos_pkt);
                }
            }
        }

        // Poll remaining decoded frames until EOS or timeout
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::time::Instant::now() >= deadline {
                gst::warning!(CAT, imp = self, "finish: drain timeout");
                break;
            }

            match self.try_get_one_frame() {
                FrameResult::Frame(output_buffer) => {
                    self.finish_one_frame(output_buffer);
                }
                FrameResult::Eos => break,
                FrameResult::TryAgain => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                FrameResult::Skip => continue,
            }
        }

        gst::debug!(CAT, imp = self, "finish: drain complete");
        Ok(gst::FlowSuccess::Ok)
    }

    fn flush(&self) -> bool {
        gst::debug!(CAT, imp = self, "flush");
        let guard = self.state.lock().unwrap();
        if let Some(ref dec) = *guard {
            unsafe {
                if let Some(reset) = (*dec.mpi).reset {
                    reset(dec.mpp_ctx);
                }
            }
        }
        true
    }
}

enum FrameResult {
    Frame(gst::Buffer),
    Eos,
    TryAgain,
    Skip,
}

impl MppVideoDec {
    /// Try to get one decoded frame from MPP. Lock is acquired and released within.
    fn try_get_one_frame(&self) -> FrameResult {
        let mut dec_state = self.state.lock().unwrap();
        let dec = match dec_state.as_ref() {
            Some(d) => d,
            None => return FrameResult::Eos,
        };

        unsafe {
            let get_frame_fn = match (*dec.mpi).decode_get_frame {
                Some(f) => f,
                None => return FrameResult::Eos,
            };

            let mut mpp_frame: ffi::MppFrame = std::ptr::null_mut();
            let ret = get_frame_fn(dec.mpp_ctx, &mut mpp_frame);

            if ret != ffi::MPP_OK || mpp_frame.is_null() {
                return FrameResult::TryAgain;
            }

            if ffi::mpp_frame_get_eos(mpp_frame) != 0 {
                ffi::mpp_frame_deinit(&mut mpp_frame);
                return FrameResult::Eos;
            }

            if ffi::mpp_frame_get_info_change(mpp_frame) != 0 {
                self.handle_info_change_inline(&mut dec_state, mpp_frame);
                return FrameResult::Skip;
            }

            if ffi::mpp_frame_get_errinfo(mpp_frame) != 0
                || ffi::mpp_frame_get_discard(mpp_frame) != 0
            {
                gst::warning!(CAT, imp = self, "frame has error or discard flag");
                ffi::mpp_frame_deinit(&mut mpp_frame);
                return FrameResult::Skip;
            }

            let mpp_buf = ffi::mpp_frame_get_buffer(mpp_frame);
            if mpp_buf.is_null() {
                gst::warning!(CAT, imp = self, "decoded frame has no buffer");
                ffi::mpp_frame_deinit(&mut mpp_frame);
                return FrameResult::Skip;
            }

            let dec = dec_state.as_mut().unwrap();
            if !dec.negotiated {
                dec.width = ffi::mpp_frame_get_width(mpp_frame);
                dec.height = ffi::mpp_frame_get_height(mpp_frame);
                dec.hor_stride = ffi::mpp_frame_get_hor_stride(mpp_frame);
                dec.ver_stride = ffi::mpp_frame_get_ver_stride(mpp_frame);
                dec.negotiated = true;

                let w = dec.width;
                let h = dec.height;
                self.negotiate_output(w, h);
            }

            let dec = dec_state.as_ref().unwrap();
            let output_buffer = self.copy_decoded_frame(
                mpp_buf, dec.hor_stride, dec.ver_stride, dec.width, dec.height,
            );
            ffi::mpp_frame_deinit(&mut mpp_frame);

            match output_buffer {
                Some(buf) => FrameResult::Frame(buf),
                None => FrameResult::Skip,
            }
        }
    }

    /// Poll for decoded frames from MPP and push them downstream.
    /// Called after each decode_put_packet.
    fn poll_decoded_frames(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        loop {
            match self.try_get_one_frame() {
                FrameResult::Frame(output_buffer) => {
                    self.finish_one_frame(output_buffer);
                }
                FrameResult::TryAgain | FrameResult::Eos => break,
                FrameResult::Skip => continue,
            }
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn finish_one_frame(&self, output_buffer: gst::Buffer) {
        let obj = self.obj();
        let obj_ref = &*obj;
        let frame = gst_video::prelude::VideoDecoderExtManual::oldest_frame(obj_ref);
        if let Some(mut f) = frame {
            f.set_output_buffer(output_buffer);
            let _ = gst_video::prelude::VideoDecoderExt::finish_frame(obj_ref, f);
        }
    }

    /// Handle info_change frame inline.
    unsafe fn handle_info_change_inline(
        &self,
        dec_state: &mut std::sync::MutexGuard<'_, Option<DecoderState>>,
        mpp_frame: ffi::MppFrame,
    ) {
        let width = ffi::mpp_frame_get_width(mpp_frame);
        let height = ffi::mpp_frame_get_height(mpp_frame);
        let hor_stride = ffi::mpp_frame_get_hor_stride(mpp_frame);
        let ver_stride = ffi::mpp_frame_get_ver_stride(mpp_frame);

        gst::info!(
            CAT, imp = self,
            "info_change: {}x{} stride={}x{}",
            width, height, hor_stride, ver_stride
        );

        let dec = dec_state.as_mut().unwrap();
        dec.width = width;
        dec.height = height;
        dec.hor_stride = hor_stride;
        dec.ver_stride = ver_stride;
        dec.negotiated = true;

        // Acknowledge the info change
        if let Some(control) = (*dec.mpi).control {
            control(
                dec.mpp_ctx,
                ffi::MPP_DEC_SET_INFO_CHANGE_READY,
                std::ptr::null_mut(),
            );
        }

        ffi::mpp_frame_deinit(&mut { mpp_frame });

        self.negotiate_output(width, height);
    }

    /// Copy decoded NV12 data from MppBuffer to a regular GstBuffer.
    unsafe fn copy_decoded_frame(
        &self,
        mpp_buf: ffi::MppBuffer,
        hor_stride: u32,
        ver_stride: u32,
        width: u32,
        height: u32,
    ) -> Option<gst::Buffer> {
        let src_ptr = ffi::mpp_buffer_get_ptr(mpp_buf) as *const u8;
        if src_ptr.is_null() {
            return None;
        }

        let frame_size = (width * height * 3 / 2) as usize;
        let mut buffer = gst::Buffer::with_size(frame_size).ok()?;
        {
            let buf_mut = buffer.get_mut().unwrap();
            let mut map = buf_mut.map_writable().ok()?;
            let dst = map.as_mut_slice();

            let src_stride = hor_stride as usize;
            let w = width as usize;
            let h = height as usize;

            if w == src_stride {
                // No stride padding — direct copy
                let copy_size = frame_size.min(ffi::mpp_buffer_get_size(mpp_buf));
                std::ptr::copy_nonoverlapping(src_ptr, dst.as_mut_ptr(), copy_size);
            } else {
                // Copy Y plane line-by-line
                for y in 0..h {
                    let src_off = y * src_stride;
                    let dst_off = y * w;
                    std::ptr::copy_nonoverlapping(
                        src_ptr.add(src_off),
                        dst.as_mut_ptr().add(dst_off),
                        w,
                    );
                }
                // Copy UV plane line-by-line
                let src_uv_base = (ver_stride as usize) * src_stride;
                let dst_uv_base = h * w;
                for y in 0..h / 2 {
                    let src_off = src_uv_base + y * src_stride;
                    let dst_off = dst_uv_base + y * w;
                    std::ptr::copy_nonoverlapping(
                        src_ptr.add(src_off),
                        dst.as_mut_ptr().add(dst_off),
                        w,
                    );
                }
            }
        }

        Some(buffer)
    }

    fn negotiate_output(&self, width: u32, height: u32) {
        let obj = self.obj();
        let result = gst_video::prelude::VideoDecoderExtManual::set_output_state(
            &*obj,
            gst_video::VideoFormat::Nv12,
            width,
            height,
            None,
        );
        if let Ok(output_state) = result {
            let _ = gst_video::prelude::VideoDecoderExtManual::negotiate(&*obj, output_state);
        }
    }
}
