use glib::subclass::prelude::*;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::subclass::prelude::*;

use once_cell::sync::Lazy;
use std::sync::Mutex;

use crate::allocator::MppAllocator;
use crate::mpp_ffi::{self as ffi, MppApiStruct};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("mpph265enc", gst::DebugColorFlags::empty(), Some("MPP H265 Encoder"))
});

const DEFAULT_BPS: u32 = 4_000_000;
const DEFAULT_GOP: i32 = 60;
/// Max poll attempts for encode_get_packet (100ms timeout each → 500ms max)
const MAX_POLL_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone)]
struct Settings {
    bps: u32,
    gop: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bps: DEFAULT_BPS,
            gop: DEFAULT_GOP,
        }
    }
}

struct EncoderState {
    mpp_ctx: ffi::MppCtx,
    mpi: *mut MppApiStruct,
    mpp_cfg: ffi::MppEncCfg,
    allocator: MppAllocator,
    /// Ping-pong input buffers: write to one while MPP encodes from the other
    input_bufs: [ffi::MppBuffer; 2],
    buf_index: usize,
    /// In-flight MppFrame from previous submission (deinit when packet arrives)
    in_flight_frame: ffi::MppFrame,
    hor_stride: u32,
    ver_stride: u32,
    width: u32,
    height: u32,
    codec: i32,
}

unsafe impl Send for EncoderState {}

impl Drop for EncoderState {
    fn drop(&mut self) {
        unsafe {
            let mut eos_frame: ffi::MppFrame = std::ptr::null_mut();
            if ffi::mpp_frame_init(&mut eos_frame) == ffi::MPP_OK {
                ffi::mpp_frame_set_eos(eos_frame, 1);
                ffi::mpp_frame_set_buffer(eos_frame, std::ptr::null_mut());
                if let Some(put_frame) = (*self.mpi).encode_put_frame {
                    let _ = put_frame(self.mpp_ctx, eos_frame);
                }
                if let Some(get_packet) = (*self.mpi).encode_get_packet {
                    let mut pkt: ffi::MppPacket = std::ptr::null_mut();
                    for _ in 0..10 {
                        let ret = get_packet(self.mpp_ctx, &mut pkt);
                        if ret != ffi::MPP_OK || pkt.is_null() {
                            break;
                        }
                        ffi::mpp_packet_deinit(&mut pkt);
                    }
                }
                ffi::mpp_frame_deinit(&mut eos_frame);
            }
            if !self.in_flight_frame.is_null() {
                ffi::mpp_frame_deinit(&mut self.in_flight_frame);
            }
            for buf in &self.input_bufs {
                if !buf.is_null() {
                    ffi::mpp_buffer_put(*buf);
                }
            }
            ffi::mpp_enc_cfg_deinit(self.mpp_cfg);
            ffi::mpp_destroy(self.mpp_ctx);
        }
    }
}

pub struct MppH265Enc {
    settings: Mutex<Settings>,
    state: Mutex<Option<EncoderState>>,
}

impl Default for MppH265Enc {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(None),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for MppH265Enc {
    const NAME: &'static str = "mpph265enc";
    type Type = super::MppH265Enc;
    type ParentType = gst_video::VideoEncoder;
}

impl ObjectImpl for MppH265Enc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::builder("bps")
                    .nick("Bitrate")
                    .blurb("Target bitrate in bits per second (0 = auto)")
                    .minimum(0)
                    .maximum(100_000_000)
                    .default_value(DEFAULT_BPS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecInt::builder("gop")
                    .nick("GOP")
                    .blurb("Group of pictures size (-1 = same as FPS)")
                    .minimum(-1)
                    .maximum(1000)
                    .default_value(DEFAULT_GOP)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "bps" => settings.bps = value.get::<u32>().unwrap(),
            "gop" => settings.gop = value.get::<i32>().unwrap(),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "bps" => settings.bps.to_value(),
            "gop" => settings.gop.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for MppH265Enc {}

impl ElementImpl for MppH265Enc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "MPP H.265 Encoder",
                "Codec/Encoder/Video",
                "Rockchip MPP hardware H.265/HEVC encoder",
                "simplertsp",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Nv12)
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder_full()
                .structure(
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", "byte-stream")
                        .field("alignment", "au")
                        .build(),
                )
                .structure(
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", "byte-stream")
                        .field("alignment", "au")
                        .build(),
                )
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

impl VideoEncoderImpl for MppH265Enc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "start");

        let allocator = MppAllocator::new().map_err(|_| {
            gst::error_msg!(gst::LibraryError::Init, ["MppAllocator::new failed"])
        })?;

        unsafe {
            let mut mpp_ctx: ffi::MppCtx = std::ptr::null_mut();
            let mut mpi: ffi::MppApi = std::ptr::null_mut();

            if ffi::mpp_create(&mut mpp_ctx, &mut mpi) != ffi::MPP_OK {
                return Err(gst::error_msg!(gst::LibraryError::Init, ["mpp_create failed"]));
            }

            if let Some(control) = (*mpi).control {
                let mut timeout: i64 = 100; // 100ms — blocks in get_packet until packet ready
                control(mpp_ctx, ffi::MPP_SET_OUTPUT_TIMEOUT, &mut timeout as *mut i64 as ffi::MppParam);
            }

            let codec;
            let init_ret = ffi::mpp_init(mpp_ctx, ffi::MPP_CTX_ENC, ffi::MPP_VIDEO_CodingHEVC);
            if init_ret == ffi::MPP_OK {
                codec = ffi::MPP_VIDEO_CodingHEVC;
                gst::info!(CAT, imp = self, "MPP encoder: HEVC (H.265)");
            } else {
                gst::warning!(CAT, imp = self, "HEVC encode init returned {}, trying AVC", init_ret);
                ffi::mpp_destroy(mpp_ctx);
                if ffi::mpp_create(&mut mpp_ctx, &mut mpi) != ffi::MPP_OK {
                    return Err(gst::error_msg!(gst::LibraryError::Init, ["mpp_create failed (retry)"]));
                }
                if let Some(control) = (*mpi).control {
                    let mut timeout: i64 = 1;
                    control(mpp_ctx, ffi::MPP_SET_OUTPUT_TIMEOUT, &mut timeout as *mut i64 as ffi::MppParam);
                }
                let avc_ret = ffi::mpp_init(mpp_ctx, ffi::MPP_CTX_ENC, ffi::MPP_VIDEO_CodingAVC);
                if avc_ret != ffi::MPP_OK {
                    ffi::mpp_destroy(mpp_ctx);
                    return Err(gst::error_msg!(gst::LibraryError::Init, ["mpp_init failed for both HEVC and AVC"]));
                }
                codec = ffi::MPP_VIDEO_CodingAVC;
                gst::info!(CAT, imp = self, "MPP encoder: AVC (H.264)");
            }

            let mut mpp_cfg: ffi::MppEncCfg = std::ptr::null_mut();
            if ffi::mpp_enc_cfg_init(&mut mpp_cfg) != ffi::MPP_OK {
                ffi::mpp_destroy(mpp_ctx);
                return Err(gst::error_msg!(gst::LibraryError::Init, ["mpp_enc_cfg_init failed"]));
            }

            if let Some(control) = (*mpi).control {
                control(mpp_ctx, ffi::MPP_ENC_GET_CFG, mpp_cfg as ffi::MppParam);
            }

            *self.state.lock().unwrap() = Some(EncoderState {
                mpp_ctx,
                mpi,
                mpp_cfg,
                allocator,
                input_bufs: [std::ptr::null_mut(), std::ptr::null_mut()],
                buf_index: 0,
                in_flight_frame: std::ptr::null_mut(),
                hor_stride: 0,
                ver_stride: 0,
                width: 0,
                height: 0,
                codec,
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
        let info = state.info();
        let width = info.width();
        let height = info.height();
        let mut fps_n = info.fps().numer() as i32;
        let mut fps_d = info.fps().denom() as i32;
        if fps_n == 0 {
            fps_n = 30;
            fps_d = 1;
        }

        let hor_stride = ffi::mpp_align(width);
        let ver_stride = ffi::mpp_align(height);

        let settings = self.settings.lock().unwrap().clone();
        let bps = if settings.bps == 0 {
            width * height / 8 * (fps_n as u32 / fps_d.max(1) as u32)
        } else {
            settings.bps
        };
        let gop = if settings.gop < 0 {
            fps_n / fps_d.max(1)
        } else {
            settings.gop
        };

        gst::info!(
            CAT, imp = self,
            "set_format: {}x{} stride={}x{} fps={}/{} bps={} gop={}",
            width, height, hor_stride, ver_stride, fps_n, fps_d, bps, gop
        );

        let mut enc_state = self.state.lock().unwrap();
        let enc = enc_state.as_mut().ok_or_else(|| {
            gst::loggable_error!(CAT, "encoder not started")
        })?;

        enc.hor_stride = hor_stride;
        enc.ver_stride = ver_stride;
        enc.width = width;
        enc.height = height;

        // (Re-)allocate ping-pong input buffers for the new resolution
        unsafe {
            for i in 0..2 {
                if !enc.input_bufs[i].is_null() {
                    ffi::mpp_buffer_put(enc.input_bufs[i]);
                    enc.input_bufs[i] = std::ptr::null_mut();
                }
                let frame_size = (hor_stride * ver_stride * 3 / 2) as usize;
                let (buf, _fd) = enc.allocator.alloc(frame_size).map_err(|_| {
                    gst::loggable_error!(CAT, "failed to allocate input buffer {}", i)
                })?;
                enc.input_bufs[i] = buf;
            }
            enc.buf_index = 0;
        }

        unsafe {
            let cfg = enc.mpp_cfg;

            ffi::enc_cfg_set_s32(cfg, "prep:width", width as i32);
            ffi::enc_cfg_set_s32(cfg, "prep:height", height as i32);
            ffi::enc_cfg_set_s32(cfg, "prep:hor_stride", hor_stride as i32);
            ffi::enc_cfg_set_s32(cfg, "prep:ver_stride", ver_stride as i32);
            ffi::enc_cfg_set_s32(cfg, "prep:format", ffi::MPP_FMT_YUV420SP);

            ffi::enc_cfg_set_s32(cfg, "rc:mode", ffi::MPP_ENC_RC_MODE_CBR);
            ffi::enc_cfg_set_s32(cfg, "rc:bps_target", bps as i32);
            ffi::enc_cfg_set_s32(cfg, "rc:bps_max", (bps * 17 / 16) as i32);
            ffi::enc_cfg_set_s32(cfg, "rc:bps_min", (bps * 15 / 16) as i32);
            ffi::enc_cfg_set_s32(cfg, "rc:gop", gop);
            ffi::enc_cfg_set_u32(cfg, "rc:max_reenc_times", 1);

            ffi::enc_cfg_set_s32(cfg, "rc:fps_in_flex", 0);
            ffi::enc_cfg_set_s32(cfg, "rc:fps_in_num", fps_n);
            ffi::enc_cfg_set_s32(cfg, "rc:fps_in_denorm", fps_d);
            ffi::enc_cfg_set_s32(cfg, "rc:fps_out_flex", 0);
            ffi::enc_cfg_set_s32(cfg, "rc:fps_out_num", fps_n);
            ffi::enc_cfg_set_s32(cfg, "rc:fps_out_denorm", fps_d);

            ffi::enc_cfg_set_s32(cfg, "rc:qp_init", 26);
            ffi::enc_cfg_set_s32(cfg, "rc:qp_min", 10);
            ffi::enc_cfg_set_s32(cfg, "rc:qp_max", 51);
            ffi::enc_cfg_set_s32(cfg, "rc:qp_min_i", 10);
            ffi::enc_cfg_set_s32(cfg, "rc:qp_max_i", 51);
            ffi::enc_cfg_set_s32(cfg, "rc:qp_ip", 2);

            if let Some(control) = (*enc.mpi).control {
                let mut sei_mode: i32 = ffi::MPP_ENC_SEI_MODE_DISABLE;
                control(enc.mpp_ctx, ffi::MPP_ENC_SET_SEI_CFG, &mut sei_mode as *mut i32 as ffi::MppParam);

                let mut header_mode: i32 = ffi::MPP_ENC_HEADER_MODE_EACH_IDR;
                control(enc.mpp_ctx, ffi::MPP_ENC_SET_HEADER_MODE, &mut header_mode as *mut i32 as ffi::MppParam);

                let ret = control(enc.mpp_ctx, ffi::MPP_ENC_SET_CFG, cfg as ffi::MppParam);
                if ret != ffi::MPP_OK {
                    return Err(gst::loggable_error!(CAT, "MPP_ENC_SET_CFG failed: {}", ret));
                }
            }
        }

        let codec_mime = if enc.codec == ffi::MPP_VIDEO_CodingHEVC { "video/x-h265" } else { "video/x-h264" };
        let obj = self.obj();
        let output_state = gst_video::prelude::VideoEncoderExtManual::set_output_state(
            &*obj,
            gst::Caps::builder(codec_mime)
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .field("width", width as i32)
                .field("height", height as i32)
                .build(),
            Some(state),
        )
        .map_err(|_| gst::loggable_error!(CAT, "set_output_state failed"))?;

        gst_video::prelude::VideoEncoderExtManual::negotiate(&*obj, output_state)
            .map_err(|_| gst::loggable_error!(CAT, "negotiate failed"))?;

        Ok(())
    }

    fn handle_frame(
        &self,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut enc_state = self.state.lock().unwrap();
        let enc = enc_state.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        // 1) If there's an in-flight frame, retrieve its encoded packet first
        let prev_result = if !enc.in_flight_frame.is_null() {
            Some(self.poll_and_finish_pending(enc)?)
        } else {
            None
        };

        // 2) Copy input to the current ping-pong buffer
        let input_buffer = frame.input_buffer().ok_or(gst::FlowError::Error)?;
        self.copy_input_to_mpp(enc, input_buffer)?;

        // 3) Submit current frame to MPP (non-blocking — packet retrieved next call)
        unsafe {
            let mut mpp_frame: ffi::MppFrame = std::ptr::null_mut();
            if ffi::mpp_frame_init(&mut mpp_frame) != ffi::MPP_OK {
                return Err(gst::FlowError::Error);
            }

            ffi::mpp_frame_set_width(mpp_frame, enc.width);
            ffi::mpp_frame_set_height(mpp_frame, enc.height);
            ffi::mpp_frame_set_hor_stride(mpp_frame, enc.hor_stride);
            ffi::mpp_frame_set_ver_stride(mpp_frame, enc.ver_stride);
            ffi::mpp_frame_set_fmt(mpp_frame, ffi::MPP_FMT_YUV420SP);
            ffi::mpp_frame_set_buffer(mpp_frame, enc.input_bufs[enc.buf_index]);
            ffi::mpp_frame_set_eos(mpp_frame, 0);

            if let Some(p) = frame.pts() {
                ffi::mpp_frame_set_pts(mpp_frame, p.nseconds() as i64);
            }

            let put_frame = (*enc.mpi).encode_put_frame.ok_or_else(|| {
                ffi::mpp_frame_deinit(&mut mpp_frame);
                gst::FlowError::Error
            })?;

            let ret = put_frame(enc.mpp_ctx, mpp_frame);
            if ret != ffi::MPP_OK {
                gst::warning!(CAT, imp = self, "encode_put_frame failed: {}", ret);
                ffi::mpp_frame_deinit(&mut mpp_frame);
                return Err(gst::FlowError::Error);
            }

            // Track in-flight frame for deinit when its packet arrives
            enc.in_flight_frame = mpp_frame;
            // Alternate ping-pong buffer for next frame
            enc.buf_index ^= 1;
        }

        // 4) Drop frame ref (stays in encoder's pending list for later finish_frame)
        //    and release state lock before finishing the PREVIOUS frame
        drop(frame);
        drop(enc_state);

        // 5) Finish the previous frame (if any) now that we've released the state lock
        if let Some(prev_buf) = prev_result {
            let obj = self.obj();
            let mut prev_frame: gst_video::VideoCodecFrame = gst_video::prelude::VideoEncoderExtManual::oldest_frame(&*obj).ok_or(gst::FlowError::Error)?;
            prev_frame.set_output_buffer(prev_buf);
            return gst_video::prelude::VideoEncoderExt::finish_frame(&*obj, prev_frame);
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn finish(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, imp = self, "finish: draining last frame");
        let mut enc_state = self.state.lock().unwrap();
        let enc = match enc_state.as_mut() {
            Some(e) => e,
            None => return Ok(gst::FlowSuccess::Ok),
        };

        if enc.in_flight_frame.is_null() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let buf = self.poll_and_finish_pending(enc)?;
        drop(enc_state);

        let obj = self.obj();
        let mut last_frame: gst_video::VideoCodecFrame = match gst_video::prelude::VideoEncoderExtManual::oldest_frame(&*obj) {
            Some(f) => f,
            None => return Ok(gst::FlowSuccess::Ok),
        };
        last_frame.set_output_buffer(buf);
        gst_video::prelude::VideoEncoderExt::finish_frame(&*obj, last_frame)
    }

    fn flush(&self) -> bool {
        gst::debug!(CAT, imp = self, "flush");

        let mut guard = self.state.lock().unwrap();
        if let Some(ref mut enc) = *guard {
            unsafe {
                // Release any in-flight frame
                if !enc.in_flight_frame.is_null() {
                    ffi::mpp_frame_deinit(&mut enc.in_flight_frame);
                    enc.in_flight_frame = std::ptr::null_mut();
                }
                if let Some(reset) = (*enc.mpi).reset {
                    reset(enc.mpp_ctx);
                }
            }
        }

        true
    }

    fn propose_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        query.add_allocation_meta::<gst_video::VideoMeta>(None);
        Ok(())
    }
}

impl MppH265Enc {
    /// Poll for the encoded packet of the in-flight frame.
    /// Deinits the in-flight MppFrame and returns the output GstBuffer.
    fn poll_and_finish_pending(
        &self,
        enc: &mut EncoderState,
    ) -> Result<gst::Buffer, gst::FlowError> {
        unsafe {
            let get_packet = (*enc.mpi).encode_get_packet.ok_or(gst::FlowError::Error)?;
            let mut pkt: ffi::MppPacket = std::ptr::null_mut();

            for _ in 0..MAX_POLL_ATTEMPTS {
                get_packet(enc.mpp_ctx, &mut pkt);
                if !pkt.is_null() {
                    break;
                }
            }

            // Release the in-flight MppFrame (releases buffer ref from set_buffer)
            ffi::mpp_frame_deinit(&mut enc.in_flight_frame);
            enc.in_flight_frame = std::ptr::null_mut();

            if pkt.is_null() {
                gst::error!(CAT, imp = self, "encode_get_packet timed out");
                return Err(gst::FlowError::Error);
            }

            let pkt_buf = ffi::mpp_packet_get_buffer(pkt);
            let pkt_len = ffi::mpp_packet_get_length(pkt);

            let buf = if !pkt_buf.is_null() && pkt_len > 0 {
                let pkt_ptr = ffi::mpp_buffer_get_ptr(pkt_buf) as *const u8;
                let src = std::slice::from_raw_parts(pkt_ptr, pkt_len);
                let mut buf = gst::Buffer::with_size(pkt_len).unwrap();
                {
                    let buf_mut = buf.get_mut().unwrap();
                    let mut map = buf_mut.map_writable().unwrap();
                    map.as_mut_slice().copy_from_slice(src);
                }
                buf
            } else {
                gst::Buffer::new()
            };

            ffi::mpp_packet_deinit(&mut pkt);
            Ok(buf)
        }
    }

    fn copy_input_to_mpp(
        &self,
        enc: &EncoderState,
        buffer: &gst::BufferRef,
    ) -> Result<(), gst::FlowError> {
        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
        let input_data = map.as_slice();

        let width = enc.width as usize;
        let height = enc.height as usize;
        let dst_stride = enc.hor_stride as usize;
        let dst_vstride = enc.ver_stride as usize;

        unsafe {
            let mpp_buf = enc.input_bufs[enc.buf_index];
            let dst_ptr = ffi::mpp_buffer_get_ptr(mpp_buf) as *mut u8;

            // Copy Y plane (row by row if stride differs, bulk if same)
            if width == dst_stride {
                let y_copy = width * height;
                if y_copy <= input_data.len() {
                    std::ptr::copy_nonoverlapping(input_data.as_ptr(), dst_ptr, y_copy);
                }
            } else {
                for y in 0..height {
                    let src_off = y * width;
                    let dst_off = y * dst_stride;
                    if src_off + width <= input_data.len() {
                        std::ptr::copy_nonoverlapping(
                            input_data.as_ptr().add(src_off),
                            dst_ptr.add(dst_off),
                            width,
                        );
                    }
                }
            }

            // Zero Y padding rows (height..ver_stride)
            if dst_vstride > height {
                let pad_start = height * dst_stride;
                let pad_end = dst_vstride * dst_stride;
                std::ptr::write_bytes(dst_ptr.add(pad_start), 0u8, pad_end - pad_start);
            }

            // Copy UV plane to correct offset (ver_stride * hor_stride)
            let src_uv = height * width;
            let dst_uv = dst_vstride * dst_stride;
            let uv_height = height / 2;

            if width == dst_stride {
                let uv_copy = width * uv_height;
                if src_uv + uv_copy <= input_data.len() {
                    std::ptr::copy_nonoverlapping(
                        input_data.as_ptr().add(src_uv),
                        dst_ptr.add(dst_uv),
                        uv_copy,
                    );
                }
            } else {
                for y in 0..uv_height {
                    let src_off = src_uv + y * width;
                    let dst_off = dst_uv + y * dst_stride;
                    if src_off + width <= input_data.len() {
                        std::ptr::copy_nonoverlapping(
                            input_data.as_ptr().add(src_off),
                            dst_ptr.add(dst_off),
                            width,
                        );
                    }
                }
            }

            // Fill UV padding rows with 128 (neutral chroma = gray, not green)
            if dst_vstride / 2 > uv_height {
                let uv_pad_start = dst_uv + uv_height * dst_stride;
                let uv_pad_end = dst_uv + (dst_vstride / 2) * dst_stride;
                std::ptr::write_bytes(dst_ptr.add(uv_pad_start), 128u8, uv_pad_end - uv_pad_start);
            }

            Ok(())
        }
    }
}
