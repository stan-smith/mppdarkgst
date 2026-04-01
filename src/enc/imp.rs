use glib::subclass::prelude::*;
use glib::translate::*;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::subclass::prelude::*;

use once_cell::sync::Lazy;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use crate::allocator::MppAllocator;
use crate::mpp_ffi::{self as ffi, MppApiStruct};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("mpph265enc", gst::DebugColorFlags::empty(), Some("MPP H265 Encoder"))
});

const DEFAULT_BPS: u32 = 4_000_000;
const DEFAULT_GOP: i32 = 60;
const MAX_PENDING: u32 = 16;

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
            ffi::mpp_enc_cfg_deinit(self.mpp_cfg);
            ffi::mpp_destroy(self.mpp_ctx);
        }
    }
}

/// Queued input frame: MppFrame ready to submit to encoder.
struct QueuedFrame {
    mpp_frame: ffi::MppFrame,
}

/// Shared state between handle_frame (pipeline thread) and enc_loop (srcpad task).
struct TaskShared {
    pending_frames: u32,
    frame_queue: VecDeque<QueuedFrame>,
    /// MppFrames submitted to encoder, awaiting encode_get_packet.
    /// mpp_frame_deinit is called when the corresponding packet arrives,
    /// which releases the buffer ref (mpp_frame_set_buffer did inc_ref).
    in_flight_frames: VecDeque<ffi::MppFrame>,
    flushing: bool,
    task_ret: Result<gst::FlowSuccess, gst::FlowError>,
    task_started: bool,
}

// Safety: QueuedFrame contains MppFrame (raw pointer) which is only accessed
// while holding the TaskShared mutex. MPP frames are thread-safe.
unsafe impl Send for QueuedFrame {}
unsafe impl Send for TaskShared {}
unsafe impl Sync for TaskShared {}

pub struct MppH265Enc {
    settings: Mutex<Settings>,
    state: Mutex<Option<EncoderState>>,
    shared: Arc<(Mutex<TaskShared>, Condvar)>,
}

impl Default for MppH265Enc {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(None),
            shared: Arc::new((
                Mutex::new(TaskShared {
                    pending_frames: 0,
                    frame_queue: VecDeque::new(),
                    in_flight_frames: VecDeque::new(),
                    flushing: false,
                    task_ret: Ok(gst::FlowSuccess::Ok),
                    task_started: false,
                }),
                Condvar::new(),
            )),
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

        // Reset shared state
        {
            let mut shared = self.shared.0.lock().unwrap();
            shared.pending_frames = 0;
            shared.frame_queue.clear();
            shared.in_flight_frames.clear();
            shared.flushing = false;
            shared.task_ret = Ok(gst::FlowSuccess::Ok);
            shared.task_started = false;
        }

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
                let mut timeout: i64 = 1; // 1ms output poll
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

        // Signal flushing to wake up task
        {
            let mut shared = self.shared.0.lock().unwrap();
            shared.flushing = true;
            // Clean up any queued MppFrames (deinit releases the buffer ref)
            for qf in shared.frame_queue.drain(..) {
                unsafe {
                    let mut f = qf.mpp_frame;
                    ffi::mpp_frame_deinit(&mut f);
                }
            }
            // Release in-flight frames (submitted but not yet returned by encoder)
            for frame in shared.in_flight_frames.drain(..) {
                unsafe {
                    let mut f = frame;
                    ffi::mpp_frame_deinit(&mut f);
                }
            }
            shared.pending_frames = 0;
        }
        self.shared.1.notify_all();

        // Stop the srcpad task
        let obj = self.obj();
        let src_pad = gst_video::prelude::VideoEncoderExtManual::src_pad(&*obj);
        let _ = src_pad.stop_task();

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
        // Copy input to MppBuffer and prepare MppFrame
        let mpp_frame = {
            let mut enc_state = self.state.lock().unwrap();
            let enc = enc_state.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

            let input_buffer = frame.input_buffer().ok_or(gst::FlowError::Error)?;
            let mpp_buf = self.copy_input_to_mpp(enc, input_buffer)?;

            unsafe {
                let mut mpp_frame: ffi::MppFrame = std::ptr::null_mut();
                if ffi::mpp_frame_init(&mut mpp_frame) != ffi::MPP_OK {
                    ffi::mpp_buffer_put(mpp_buf);
                    return Err(gst::FlowError::Error);
                }

                ffi::mpp_frame_set_width(mpp_frame, enc.width);
                ffi::mpp_frame_set_height(mpp_frame, enc.height);
                ffi::mpp_frame_set_hor_stride(mpp_frame, enc.hor_stride);
                ffi::mpp_frame_set_ver_stride(mpp_frame, enc.ver_stride);
                ffi::mpp_frame_set_fmt(mpp_frame, ffi::MPP_FMT_YUV420SP);
                ffi::mpp_frame_set_buffer(mpp_frame, mpp_buf);
                // Release our buffer ref — frame now owns it (set_buffer did inc_ref)
                ffi::mpp_buffer_put(mpp_buf);
                ffi::mpp_frame_set_eos(mpp_frame, 0);

                if let Some(p) = frame.pts() {
                    ffi::mpp_frame_set_pts(mpp_frame, p.nseconds() as i64);
                }

                mpp_frame
            }
        }; // enc_state lock dropped here

        // Drop frame to release stream lock ref count (from VideoCodecFrame)
        drop(frame);

        // Start srcpad task if not already running
        {
            let mut shared = self.shared.0.lock().unwrap();
            if shared.flushing {
                // Clean up the MppFrame (deinit releases the buffer ref)
                unsafe {
                    let mut f = mpp_frame;
                    ffi::mpp_frame_deinit(&mut f);
                }
                return Err(gst::FlowError::Flushing);
            }
            if !shared.task_started {
                let obj = self.obj();
                let src_pad = gst_video::prelude::VideoEncoderExtManual::src_pad(&*obj);

                let element = obj.clone();
                let task_shared = Arc::clone(&self.shared);
                src_pad
                    .start_task(move || {
                        Self::enc_loop(&element, &task_shared);
                    })
                    .map_err(|_| {
                        gst::error!(CAT, "Failed to start srcpad task");
                        gst::FlowError::Error
                    })?;
                shared.task_started = true;
                gst::debug!(CAT, imp = self, "started srcpad encoding task");
            }
        }

        // Back-pressure: release stream lock and wait if too many pending frames
        let stream_lock = unsafe {
            let obj = self.obj();
            let encoder_ptr: *const gst_video::ffi::GstVideoEncoder =
                obj.upcast_ref::<gst_video::VideoEncoder>().to_glib_none().0;
            &(*encoder_ptr).stream_lock as *const glib::ffi::GRecMutex as *mut glib::ffi::GRecMutex
        };

        {
            let shared = self.shared.0.lock().unwrap();
            if shared.pending_frames >= MAX_PENDING {
                // Release shared lock temporarily so we can release stream lock
                drop(shared);

                // Release stream lock so the srcpad task can run
                unsafe { glib::ffi::g_rec_mutex_unlock(stream_lock); }

                // Wait for space in the queue
                let mut shared = self.shared.0.lock().unwrap();
                while shared.pending_frames >= MAX_PENDING && !shared.flushing {
                    shared = self.shared.1.wait(shared).unwrap();
                }
                drop(shared);

                // Re-acquire stream lock
                unsafe { glib::ffi::g_rec_mutex_lock(stream_lock); }

                let shared = self.shared.0.lock().unwrap();
                if shared.flushing {
                    unsafe {
                        let mut f = mpp_frame;
                        ffi::mpp_frame_deinit(&mut f);
                    }
                    return Err(gst::FlowError::Flushing);
                }
            }
        }

        // Enqueue frame and signal the task
        let task_ret = {
            let mut shared = self.shared.0.lock().unwrap();
            shared.pending_frames += 1;
            shared.frame_queue.push_back(QueuedFrame {
                mpp_frame,
            });
            self.shared.1.notify_one();
            shared.task_ret.clone()
        };

        task_ret
    }

    fn flush(&self) -> bool {
        gst::debug!(CAT, imp = self, "flush");

        // Signal flushing
        {
            let mut shared = self.shared.0.lock().unwrap();
            shared.flushing = true;
            // Clean up queued MppFrames (deinit releases the buffer ref)
            for qf in shared.frame_queue.drain(..) {
                unsafe {
                    let mut f = qf.mpp_frame;
                    ffi::mpp_frame_deinit(&mut f);
                }
            }
            // Release in-flight frames
            for frame in shared.in_flight_frames.drain(..) {
                unsafe {
                    let mut f = frame;
                    ffi::mpp_frame_deinit(&mut f);
                }
            }
            shared.pending_frames = 0;
        }
        self.shared.1.notify_all();

        // Pause the task
        let obj = self.obj();
        let src_pad = gst_video::prelude::VideoEncoderExtManual::src_pad(&*obj);
        let _ = src_pad.pause_task();

        // Reset MPP
        let guard = self.state.lock().unwrap();
        if let Some(ref enc) = *guard {
            unsafe {
                if let Some(reset) = (*enc.mpi).reset {
                    reset(enc.mpp_ctx);
                }
            }
        }
        drop(guard);

        // Clear flushing state
        {
            let mut shared = self.shared.0.lock().unwrap();
            shared.flushing = false;
            shared.task_ret = Ok(gst::FlowSuccess::Ok);
            shared.task_started = false;
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
    /// Srcpad task loop — runs in a dedicated thread.
    fn enc_loop(
        element: &super::MppH265Enc,
        shared: &Arc<(Mutex<TaskShared>, Condvar)>,
    ) {
        let (ref mtx, ref cvar) = **shared;

        // Wait until there's work to do
        {
            let mut guard = mtx.lock().unwrap();
            while guard.pending_frames == 0 && !guard.flushing {
                guard = cvar.wait(guard).unwrap();
            }

            if guard.flushing && guard.pending_frames == 0 {
                guard.task_ret = Err(gst::FlowError::Flushing);
                let src_pad = gst_video::prelude::VideoEncoderExtManual::src_pad(element);
                let _ = src_pad.pause_task();
                return;
            }
        }

        // Acquire stream lock
        let stream_lock = unsafe {
            let encoder_ptr: *const gst_video::ffi::GstVideoEncoder =
                element.upcast_ref::<gst_video::VideoEncoder>().to_glib_none().0;
            &(*encoder_ptr).stream_lock as *const glib::ffi::GRecMutex as *mut glib::ffi::GRecMutex
        };
        unsafe { glib::ffi::g_rec_mutex_lock(stream_lock); }

        let imp = element.imp();

        // Send all queued frames to MPP (non-blocking)
        loop {
            let queued = {
                let mut guard = mtx.lock().unwrap();
                guard.frame_queue.pop_front()
            };

            let Some(qf) = queued else { break };

            let enc_guard = imp.state.lock().unwrap();
            let Some(ref enc) = *enc_guard else {
                // Encoder stopped — clean up (deinit releases buffer ref)
                unsafe {
                    let mut f = qf.mpp_frame;
                    ffi::mpp_frame_deinit(&mut f);
                }
                break;
            };

            unsafe {
                let put_frame = match (*enc.mpi).encode_put_frame {
                    Some(f) => f,
                    None => {
                        let mut f = qf.mpp_frame;
                        ffi::mpp_frame_deinit(&mut f);
                        break;
                    }
                };

                let ret = put_frame(enc.mpp_ctx, qf.mpp_frame);

                if ret != ffi::MPP_OK {
                    gst::warning!(CAT, obj = element, "encode_put_frame failed: {}", ret);
                    // Put_frame failed — deinit frame (releases buffer ref)
                    let mut f = qf.mpp_frame;
                    ffi::mpp_frame_deinit(&mut f);
                } else {
                    // Frame is in-flight — encoder owns it now.
                    // Will be deinited when the encoded packet arrives.
                    let mut guard = mtx.lock().unwrap();
                    guard.in_flight_frames.push_back(qf.mpp_frame);
                }
            }

            drop(enc_guard);
        }

        // Poll for encoded packets from MPP (1ms timeout per attempt)
        loop {
            let enc_guard = imp.state.lock().unwrap();
            let Some(ref enc) = *enc_guard else { break };

            let get_packet = match unsafe { (*enc.mpi).encode_get_packet } {
                Some(f) => f,
                None => break,
            };

            let mut pkt: ffi::MppPacket = std::ptr::null_mut();
            unsafe { get_packet(enc.mpp_ctx, &mut pkt); }

            if pkt.is_null() {
                break;
            }

            let pkt_buf = unsafe { ffi::mpp_packet_get_buffer(pkt) };
            let pkt_len = unsafe { ffi::mpp_packet_get_length(pkt) };

            // Memcpy encoded packet to GstBuffer. Encoded H.265 packets are small
            // (~36KB), so memcpy is negligible. DMA-BUF zero-copy for output is
            // problematic due to MPP output pool refcount semantics.
            let output_buffer = if !pkt_buf.is_null() && pkt_len > 0 {
                let pkt_ptr = unsafe { ffi::mpp_buffer_get_ptr(pkt_buf) as *const u8 };
                let src = unsafe { std::slice::from_raw_parts(pkt_ptr, pkt_len) };
                let mut buf = gst::Buffer::with_size(pkt_len).unwrap();
                {
                    let buf_mut = buf.get_mut().unwrap();
                    let mut map = buf_mut.map_writable().unwrap();
                    map.as_mut_slice().copy_from_slice(src);
                }
                Some(buf)
            } else {
                None
            };

            unsafe { ffi::mpp_packet_deinit(&mut pkt); }
            drop(enc_guard);

            // Finish the oldest pending frame with the encoded output
            let oldest = gst_video::prelude::VideoEncoderExtManual::oldest_frame(element);
            if let Some(mut f) = oldest {
                if let Some(buf) = output_buffer {
                    f.set_output_buffer(buf);
                }
                let ret = gst_video::prelude::VideoEncoderExt::finish_frame(element, f);

                // Release the oldest in-flight input frame (encoder is done with it).
                // mpp_frame_deinit releases the buffer ref that mpp_frame_set_buffer added.
                {
                    let mut guard = mtx.lock().unwrap();
                    if let Some(frame) = guard.in_flight_frames.pop_front() {
                        unsafe {
                            let mut f = frame;
                            ffi::mpp_frame_deinit(&mut f);
                        }
                    }
                    if guard.pending_frames > 0 {
                        guard.pending_frames -= 1;
                    }
                    if ret.is_err() {
                        guard.task_ret = ret;
                    }
                    cvar.notify_all();
                }

                if ret.is_err() {
                    let src_pad = gst_video::prelude::VideoEncoderExtManual::src_pad(element);
                    let _ = src_pad.pause_task();
                    unsafe { glib::ffi::g_rec_mutex_unlock(stream_lock); }
                    return;
                }
            } else {
                // No frame waiting — shouldn't happen, but release in-flight frame and decrement
                let mut guard = mtx.lock().unwrap();
                if let Some(frame) = guard.in_flight_frames.pop_front() {
                    unsafe {
                        let mut f = frame;
                        ffi::mpp_frame_deinit(&mut f);
                    }
                }
                if guard.pending_frames > 0 {
                    guard.pending_frames -= 1;
                }
                cvar.notify_all();
                break;
            }
        }

        // Check if we should stop
        {
            let guard = mtx.lock().unwrap();
            if guard.task_ret.is_err() {
                let src_pad = gst_video::prelude::VideoEncoderExtManual::src_pad(element);
                let _ = src_pad.pause_task();
            }
        }

        unsafe { glib::ffi::g_rec_mutex_unlock(stream_lock); }
    }

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
            let width = enc.width as usize;
            let height = enc.height as usize;
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
}
