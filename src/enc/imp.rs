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
    mpp_frame: ffi::MppFrame,
    allocator: MppAllocator,
    hor_stride: u32,
    ver_stride: u32,
    codec: i32,
}

unsafe impl Send for EncoderState {}

impl Drop for EncoderState {
    fn drop(&mut self) {
        unsafe {
            // Send EOS and drain
            ffi::mpp_frame_set_eos(self.mpp_frame, 1);
            ffi::mpp_frame_set_buffer(self.mpp_frame, std::ptr::null_mut());
            if let Some(put_frame) = (*self.mpi).encode_put_frame {
                let _ = put_frame(self.mpp_ctx, self.mpp_frame);
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
            ffi::mpp_frame_deinit(&mut self.mpp_frame);
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

            // Set output timeout to 200ms (synchronous polling)
            if let Some(control) = (*mpi).control {
                let mut timeout: i64 = ffi::MPP_POLL_NON_BLOCK as i64;
                control(mpp_ctx, ffi::MPP_SET_INPUT_TIMEOUT, &mut timeout as *mut i64 as ffi::MppParam);
                let mut timeout: i64 = 200;
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
                    let mut timeout: i64 = ffi::MPP_POLL_NON_BLOCK as i64;
                    control(mpp_ctx, ffi::MPP_SET_INPUT_TIMEOUT, &mut timeout as *mut i64 as ffi::MppParam);
                    let mut timeout: i64 = 200;
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

            let mut mpp_frame: ffi::MppFrame = std::ptr::null_mut();
            if ffi::mpp_frame_init(&mut mpp_frame) != ffi::MPP_OK {
                ffi::mpp_destroy(mpp_ctx);
                return Err(gst::error_msg!(gst::LibraryError::Init, ["mpp_frame_init failed"]));
            }

            let mut mpp_cfg: ffi::MppEncCfg = std::ptr::null_mut();
            if ffi::mpp_enc_cfg_init(&mut mpp_cfg) != ffi::MPP_OK {
                ffi::mpp_frame_deinit(&mut mpp_frame);
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
                mpp_frame,
                allocator,
                hor_stride: 0,
                ver_stride: 0,
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

            ffi::mpp_frame_set_width(enc.mpp_frame, width);
            ffi::mpp_frame_set_height(enc.mpp_frame, height);
            ffi::mpp_frame_set_hor_stride(enc.mpp_frame, hor_stride);
            ffi::mpp_frame_set_ver_stride(enc.mpp_frame, ver_stride);
            ffi::mpp_frame_set_fmt(enc.mpp_frame, ffi::MPP_FMT_YUV420SP);
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
        mut frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, imp = self, "handle_frame: frame #{}", frame.system_frame_number());
        let mut enc_state = self.state.lock().unwrap();
        let enc = enc_state.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        let input_buffer = frame.input_buffer().ok_or(gst::FlowError::Error)?;

        // Check for stride adaptation from DmaBuf/VideoMeta
        if let Some(meta) = input_buffer.meta::<gst_video::VideoMeta>() {
            let src_hstride = meta.stride()[0] as u32;
            let src_vstride = if meta.n_planes() >= 2 {
                (meta.offset()[1] as u32) / src_hstride.max(1)
            } else {
                enc.ver_stride
            };
            self.apply_strides(enc, src_hstride, src_vstride);
        }

        // Copy input to MppBuffer
        let mpp_buf = self.copy_input_to_mpp(enc, input_buffer)?;

        let pts = frame.pts();

        unsafe {
            // Build MppFrame for this input
            let mut mpp_frame: ffi::MppFrame = std::ptr::null_mut();
            if ffi::mpp_frame_init(&mut mpp_frame) != ffi::MPP_OK {
                ffi::mpp_buffer_put(mpp_buf);
                return Err(gst::FlowError::Error);
            }

            let width = ffi::mpp_frame_get_width(enc.mpp_frame);
            let height = ffi::mpp_frame_get_height(enc.mpp_frame);
            ffi::mpp_frame_set_width(mpp_frame, width);
            ffi::mpp_frame_set_height(mpp_frame, height);
            ffi::mpp_frame_set_hor_stride(mpp_frame, enc.hor_stride);
            ffi::mpp_frame_set_ver_stride(mpp_frame, enc.ver_stride);
            ffi::mpp_frame_set_fmt(mpp_frame, ffi::MPP_FMT_YUV420SP);
            ffi::mpp_frame_set_buffer(mpp_frame, mpp_buf);
            ffi::mpp_frame_set_eos(mpp_frame, 0);

            if let Some(p) = pts {
                ffi::mpp_frame_set_pts(mpp_frame, p.nseconds() as i64);
            }

            let put_frame = (*enc.mpi).encode_put_frame.ok_or(gst::FlowError::Error)?;
            let ret = put_frame(enc.mpp_ctx, mpp_frame);
            ffi::mpp_frame_deinit(&mut mpp_frame);

            if ret != ffi::MPP_OK {
                ffi::mpp_buffer_put(mpp_buf);
                gst::error!(CAT, imp = self, "encode_put_frame failed: {}", ret);
                return Err(gst::FlowError::Error);
            }

            // Synchronously poll for encoded output
            let get_packet = (*enc.mpi).encode_get_packet.ok_or(gst::FlowError::Error)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

            loop {
                let mut pkt: ffi::MppPacket = std::ptr::null_mut();
                get_packet(enc.mpp_ctx, &mut pkt);

                if !pkt.is_null() {
                    ffi::mpp_buffer_put(mpp_buf);

                    let pkt_buf = ffi::mpp_packet_get_buffer(pkt);
                    let pkt_len = ffi::mpp_packet_get_length(pkt);

                    if !pkt_buf.is_null() && pkt_len > 0 {
                        let pkt_ptr = ffi::mpp_buffer_get_ptr(pkt_buf) as *const u8;
                        let src = std::slice::from_raw_parts(pkt_ptr, pkt_len);
                        let mut output_buffer = gst::Buffer::with_size(pkt_len).unwrap();
                        {
                            let buf_mut = output_buffer.get_mut().unwrap();
                            let mut map = buf_mut.map_writable().unwrap();
                            map.as_mut_slice().copy_from_slice(src);
                        }
                        ffi::mpp_packet_deinit(&mut pkt);

                        drop(enc_state);
                        frame.set_output_buffer(output_buffer);
                        return gst_video::prelude::VideoEncoderExt::finish_frame(&*self.obj(), frame);
                    }

                    ffi::mpp_packet_deinit(&mut pkt);
                    drop(enc_state);
                    return gst_video::prelude::VideoEncoderExt::finish_frame(&*self.obj(), frame);
                }

                if std::time::Instant::now() >= deadline {
                    ffi::mpp_buffer_put(mpp_buf);
                    gst::error!(CAT, imp = self, "encode timeout");
                    return Err(gst::FlowError::Error);
                }
            }
        }
    }

    fn flush(&self) -> bool {
        gst::debug!(CAT, imp = self, "flush");
        let guard = self.state.lock().unwrap();
        if let Some(ref enc) = *guard {
            unsafe {
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
    fn copy_input_to_mpp(
        &self,
        enc: &EncoderState,
        buffer: &gst::BufferRef,
    ) -> Result<ffi::MppBuffer, gst::FlowError> {
        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
        let input_data = map.as_slice();

        let hor_stride = enc.hor_stride;
        let ver_stride = enc.ver_stride;
        let frame_size = (hor_stride * ver_stride * 3 / 2) as usize;

        unsafe {
            let (mpp_buf, _fd) = enc.allocator.alloc(frame_size).map_err(|_| {
                gst::error!(CAT, imp = self, "mpp_buffer alloc failed");
                gst::FlowError::Error
            })?;

            let dst_ptr = ffi::mpp_buffer_get_ptr(mpp_buf) as *mut u8;
            let width = ffi::mpp_frame_get_width(enc.mpp_frame) as usize;
            let height = ffi::mpp_frame_get_height(enc.mpp_frame) as usize;
            let dst_stride = hor_stride as usize;

            if width == dst_stride {
                let copy_size = frame_size.min(input_data.len());
                std::ptr::copy_nonoverlapping(input_data.as_ptr(), dst_ptr, copy_size);
            } else {
                // Copy Y plane
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
                // Copy UV plane
                let src_uv = height * width;
                let dst_uv = ver_stride as usize * dst_stride;
                for y in 0..height / 2 {
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

            Ok(mpp_buf)
        }
    }

    fn apply_strides(&self, enc: &mut EncoderState, hstride: u32, vstride: u32) {
        if hstride == enc.hor_stride && vstride == enc.ver_stride {
            return;
        }

        gst::info!(
            CAT, imp = self,
            "adapting strides: {}x{} -> {}x{}",
            enc.hor_stride, enc.ver_stride, hstride, vstride
        );

        enc.hor_stride = hstride;
        enc.ver_stride = vstride;

        unsafe {
            ffi::mpp_frame_set_hor_stride(enc.mpp_frame, hstride);
            ffi::mpp_frame_set_ver_stride(enc.mpp_frame, vstride);
            ffi::enc_cfg_set_s32(enc.mpp_cfg, "prep:hor_stride", hstride as i32);
            ffi::enc_cfg_set_s32(enc.mpp_cfg, "prep:ver_stride", vstride as i32);

            if let Some(control) = (*enc.mpi).control {
                control(enc.mpp_ctx, ffi::MPP_ENC_SET_CFG, enc.mpp_cfg as ffi::MppParam);
            }
        }
    }
}
