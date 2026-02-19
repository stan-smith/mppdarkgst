//! Manual FFI bindings for Rockchip MPP (librockchip_mpp.so).
//!
//! Only the functions actually used by mpph265enc and mppvideodec are declared.
//! Constants are derived from rk_mpi.h / rk_mpi_cmd.h / mpp_frame.h headers.
//! Numeric values MUST be verified against the actual headers on the target.

#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]

use std::os::raw::{c_char, c_int, c_uint, c_void};

// ---------------------------------------------------------------------------
// Opaque handle types
// ---------------------------------------------------------------------------

pub type MppCtx = *mut c_void;
pub type MppApi = *mut MppApiStruct;
pub type MppFrame = *mut c_void;
pub type MppPacket = *mut c_void;
pub type MppBuffer = *mut c_void;
pub type MppBufferGroup = *mut c_void;
pub type MppEncCfg = *mut c_void;
pub type MppMeta = *mut c_void;
pub type MppParam = *mut c_void;
pub type MppRet = c_int;

pub const MPP_OK: MppRet = 0;

// ---------------------------------------------------------------------------
// MppApi — the function-pointer table returned by mpp_create()
// ---------------------------------------------------------------------------
// Layout must match the C struct exactly. Fields we don't use are still
// declared to keep offsets correct.

#[repr(C)]
pub struct MppApiStruct {
    pub size: u32,
    pub version: u32,
    // combined decode
    pub decode: Option<unsafe extern "C" fn(MppCtx, MppPacket, *mut MppFrame) -> MppRet>,
    // split decode
    pub decode_put_packet: Option<unsafe extern "C" fn(MppCtx, MppPacket) -> MppRet>,
    pub decode_get_frame: Option<unsafe extern "C" fn(MppCtx, *mut MppFrame) -> MppRet>,
    // combined encode
    pub encode: Option<unsafe extern "C" fn(MppCtx, MppFrame, *mut MppPacket) -> MppRet>,
    // split encode
    pub encode_put_frame: Option<unsafe extern "C" fn(MppCtx, MppFrame) -> MppRet>,
    pub encode_get_packet: Option<unsafe extern "C" fn(MppCtx, *mut MppPacket) -> MppRet>,
    // ISP (unused)
    pub isp: Option<unsafe extern "C" fn(MppCtx, MppFrame, MppFrame) -> MppRet>,
    pub isp_put_frame: Option<unsafe extern "C" fn(MppCtx, MppFrame) -> MppRet>,
    pub isp_get_frame: Option<unsafe extern "C" fn(MppCtx, *mut MppFrame) -> MppRet>,
    // task interface (unused)
    pub poll: Option<unsafe extern "C" fn(MppCtx, c_int, c_int) -> MppRet>,
    pub dequeue: Option<unsafe extern "C" fn(MppCtx, c_int, *mut c_void) -> MppRet>,
    pub enqueue: Option<unsafe extern "C" fn(MppCtx, c_int, *mut c_void) -> MppRet>,
    // control
    pub reset: Option<unsafe extern "C" fn(MppCtx) -> MppRet>,
    pub control: Option<unsafe extern "C" fn(MppCtx, c_uint, MppParam) -> MppRet>,
    // reserved
    pub reserved: [u32; 16],
}

// ---------------------------------------------------------------------------
// MppBufferInfo — passed to mpp_buffer_import_with_tag
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct MppBufferInfo {
    pub buf_type: c_int,
    pub size: usize,
    pub ptr: *mut c_void,
    pub hnd: *mut c_void,
    pub fd: c_int,
    pub index: c_int,
}

impl Default for MppBufferInfo {
    fn default() -> Self {
        Self {
            buf_type: MPP_BUFFER_TYPE_DRM,
            size: 0,
            ptr: std::ptr::null_mut(),
            hnd: std::ptr::null_mut(),
            fd: -1,
            index: -1,
        }
    }
}

// ---------------------------------------------------------------------------
// Context types (MppCtxType)
// ---------------------------------------------------------------------------

pub const MPP_CTX_DEC: c_int = 0;
pub const MPP_CTX_ENC: c_int = 1;

// ---------------------------------------------------------------------------
// Codec types (MppCodingType) — from rk_type.h
// ---------------------------------------------------------------------------

pub const MPP_VIDEO_CodingUnused: c_int = 0;
pub const MPP_VIDEO_CodingAutoDetect: c_int = 1;
pub const MPP_VIDEO_CodingMPEG2: c_int = 2;
pub const MPP_VIDEO_CodingH263: c_int = 3;
pub const MPP_VIDEO_CodingMPEG4: c_int = 4;
pub const MPP_VIDEO_CodingWMV: c_int = 5;
pub const MPP_VIDEO_CodingRV: c_int = 6;
pub const MPP_VIDEO_CodingAVC: c_int = 7;
pub const MPP_VIDEO_CodingMJPEG: c_int = 8;
pub const MPP_VIDEO_CodingVP8: c_int = 9;
pub const MPP_VIDEO_CodingVP9: c_int = 10;
pub const MPP_VIDEO_CodingVC1: c_int = 0x0100_0000;
pub const MPP_VIDEO_CodingFLV1: c_int = 0x0100_0001;
pub const MPP_VIDEO_CodingDIVX3: c_int = 0x0100_0002;
pub const MPP_VIDEO_CodingVP6: c_int = 0x0100_0003;
pub const MPP_VIDEO_CodingHEVC: c_int = 0x0100_0004;
pub const MPP_VIDEO_CodingAVSPLUS: c_int = 16;
pub const MPP_VIDEO_CodingAVS: c_int = 17;
pub const MPP_VIDEO_CodingAVS2: c_int = 18;
pub const MPP_VIDEO_CodingAV1: c_int = 19;

// ---------------------------------------------------------------------------
// Frame format (MppFrameFormat) — from mpp_frame.h
// ---------------------------------------------------------------------------

pub const MPP_FMT_YUV420SP: c_int = 0; // NV12
pub const MPP_FMT_YUV420SP_10BIT: c_int = 1;
pub const MPP_FMT_YUV422SP: c_int = 2; // NV16
pub const MPP_FMT_YUV422SP_10BIT: c_int = 3;
pub const MPP_FMT_YUV420P: c_int = 4; // I420
pub const MPP_FMT_YUV420SP_VU: c_int = 5; // NV21
pub const MPP_FMT_YUV422P: c_int = 6;
pub const MPP_FMT_YUV422SP_VU: c_int = 7;
pub const MPP_FMT_YUV422_YUYV: c_int = 8;
pub const MPP_FMT_YUV422_YVYU: c_int = 9;
pub const MPP_FMT_YUV422_UYVY: c_int = 10;
pub const MPP_FMT_YUV422_VYUY: c_int = 11;
pub const MPP_FMT_YUV444SP: c_int = 12;
pub const MPP_FMT_YUV444P: c_int = 13;
pub const MPP_FMT_RGB565: c_int = 16;
pub const MPP_FMT_BGR565: c_int = 17;
pub const MPP_FMT_RGB555: c_int = 18;
pub const MPP_FMT_BGR555: c_int = 19;
pub const MPP_FMT_RGB444: c_int = 20;
pub const MPP_FMT_BGR444: c_int = 21;
pub const MPP_FMT_RGB888: c_int = 22;
pub const MPP_FMT_BGR888: c_int = 23;
pub const MPP_FMT_RGB101010: c_int = 24;
pub const MPP_FMT_BGR101010: c_int = 25;
pub const MPP_FMT_ARGB8888: c_int = 26;
pub const MPP_FMT_ABGR8888: c_int = 27;
pub const MPP_FMT_BGRA8888: c_int = 28;
pub const MPP_FMT_RGBA8888: c_int = 29;

// Frame format mask (for stripping FBC flags)
pub const MPP_FRAME_FMT_MASK: c_int = 0x000f_ffff;

// ---------------------------------------------------------------------------
// Buffer types (MppBufferType)
// ---------------------------------------------------------------------------

pub const MPP_BUFFER_TYPE_NORMAL: c_int = 0;
pub const MPP_BUFFER_TYPE_ION: c_int = 1;
pub const MPP_BUFFER_TYPE_EXT_DMA: c_int = 2;
pub const MPP_BUFFER_TYPE_DRM: c_int = 3;
pub const MPP_BUFFER_TYPE_DMA_HEAP: c_int = 4;

// ---------------------------------------------------------------------------
// MpiCmd — control command IDs from rk_mpi_cmd.h
//
// Computed from the C enum using module/context bit masks:
//   CMD_MODULE_MPP   = 0x0020_0000
//   CMD_MODULE_CODEC = 0x0030_0000
//   CMD_CTX_ID_DEC   = 0x0001_0000
//   CMD_CTX_ID_ENC   = 0x0002_0000
//   CMD_ENC_CFG_MISC = 0x0000_8000
// Values verified against Radxa rk_mpi_cmd.h headers.
// ---------------------------------------------------------------------------

// MPP timeout commands (CMD_MODULE_MPP + offset)
pub const MPP_SET_INPUT_TIMEOUT: c_uint = 0x0020_0006;
pub const MPP_SET_OUTPUT_TIMEOUT: c_uint = 0x0020_0007;

// Decoder commands (CMD_MODULE_CODEC | CMD_CTX_ID_DEC + offset)
pub const MPP_DEC_SET_EXT_BUF_GROUP: c_uint = 0x0031_0002;
pub const MPP_DEC_SET_INFO_CHANGE_READY: c_uint = 0x0031_0003;
pub const MPP_DEC_SET_PARSER_FAST_MODE: c_uint = 0x0031_0006;
pub const MPP_DEC_SET_DISABLE_ERROR: c_uint = 0x0031_000b;
pub const MPP_DEC_SET_OUTPUT_FORMAT: c_uint = 0x0031_000a;

// Encoder commands (CMD_MODULE_CODEC | CMD_CTX_ID_ENC + offset)
pub const MPP_ENC_SET_CFG: c_uint = 0x0032_0001;
pub const MPP_ENC_GET_CFG: c_uint = 0x0032_0002;
// MPP_ENC_SET_HEADER_MODE is under CMD_ENC_CFG_MISC segment
pub const MPP_ENC_SET_HEADER_MODE: c_uint = 0x0032_8001;
pub const MPP_ENC_SET_SEI_CFG: c_uint = 0x0032_000f;

// ---------------------------------------------------------------------------
// Encoder rate control modes (MppEncRcMode)
// ---------------------------------------------------------------------------

pub const MPP_ENC_RC_MODE_VBR: c_int = 0;
pub const MPP_ENC_RC_MODE_CBR: c_int = 1;
pub const MPP_ENC_RC_MODE_FIXQP: c_int = 2;
pub const MPP_ENC_RC_MODE_AVBR: c_int = 3;

// ---------------------------------------------------------------------------
// Encoder header mode (MppEncHeaderMode)
// ---------------------------------------------------------------------------

pub const MPP_ENC_HEADER_MODE_DEFAULT: c_int = 0;
pub const MPP_ENC_HEADER_MODE_EACH_IDR: c_int = 1;

// ---------------------------------------------------------------------------
// Encoder SEI mode (MppEncSeiMode)
// ---------------------------------------------------------------------------

pub const MPP_ENC_SEI_MODE_DISABLE: c_int = 0;

// ---------------------------------------------------------------------------
// Timeout values
// ---------------------------------------------------------------------------

pub const MPP_POLL_NON_BLOCK: c_int = 0; // MPP_POLL_NON_BLOCK in mpp_task.h

// ---------------------------------------------------------------------------
// Meta keys (for mpp_meta_get_frame etc.)
// ---------------------------------------------------------------------------

pub const KEY_INPUT_FRAME: c_int = 3; // From mpp_meta.h enum

// ---------------------------------------------------------------------------
// Alignment
// ---------------------------------------------------------------------------

pub const MPP_ALIGNMENT: u32 = 16;

#[inline]
pub fn mpp_align(v: u32) -> u32 {
    (v + MPP_ALIGNMENT - 1) & !(MPP_ALIGNMENT - 1)
}

// ---------------------------------------------------------------------------
// Extern "C" function declarations
// ---------------------------------------------------------------------------

#[link(name = "rockchip_mpp")]
extern "C" {
    // ---- Context management ----
    pub fn mpp_create(ctx: *mut MppCtx, mpi: *mut MppApi) -> MppRet;
    pub fn mpp_init(ctx: MppCtx, ctx_type: c_int, coding: c_int) -> MppRet;
    pub fn mpp_destroy(ctx: MppCtx) -> MppRet;

    // ---- Frame operations ----
    pub fn mpp_frame_init(frame: *mut MppFrame) -> MppRet;
    pub fn mpp_frame_deinit(frame: *mut MppFrame) -> MppRet;
    pub fn mpp_frame_set_width(frame: MppFrame, width: u32);
    pub fn mpp_frame_set_height(frame: MppFrame, height: u32);
    pub fn mpp_frame_set_hor_stride(frame: MppFrame, stride: u32);
    pub fn mpp_frame_set_ver_stride(frame: MppFrame, stride: u32);
    pub fn mpp_frame_set_fmt(frame: MppFrame, fmt: c_int);
    pub fn mpp_frame_set_buffer(frame: MppFrame, buf: MppBuffer);
    pub fn mpp_frame_set_pts(frame: MppFrame, pts: i64);
    pub fn mpp_frame_set_eos(frame: MppFrame, eos: u32);
    pub fn mpp_frame_get_width(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_height(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_hor_stride(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_ver_stride(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_fmt(frame: MppFrame) -> c_int;
    pub fn mpp_frame_get_buffer(frame: MppFrame) -> MppBuffer;
    pub fn mpp_frame_get_pts(frame: MppFrame) -> i64;
    pub fn mpp_frame_get_info_change(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_eos(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_errinfo(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_discard(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_mode(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_offset_x(frame: MppFrame) -> u32;
    pub fn mpp_frame_get_offset_y(frame: MppFrame) -> u32;

    // ---- Packet operations ----
    pub fn mpp_packet_init(pkt: *mut MppPacket, data: *const c_void, size: usize) -> MppRet;
    pub fn mpp_packet_deinit(pkt: *mut MppPacket) -> MppRet;
    pub fn mpp_packet_set_pts(pkt: MppPacket, pts: i64);
    pub fn mpp_packet_set_eos(pkt: MppPacket);
    pub fn mpp_packet_set_extra_data(pkt: MppPacket);
    pub fn mpp_packet_get_length(pkt: MppPacket) -> usize;
    pub fn mpp_packet_get_buffer(pkt: MppPacket) -> MppBuffer;
    pub fn mpp_packet_get_meta(pkt: MppPacket) -> MppMeta;

    // ---- Encoder config ----
    pub fn mpp_enc_cfg_init(cfg: *mut MppEncCfg) -> MppRet;
    pub fn mpp_enc_cfg_deinit(cfg: MppEncCfg) -> MppRet;
    pub fn mpp_enc_cfg_set_s32(cfg: MppEncCfg, name: *const c_char, val: i32) -> MppRet;
    pub fn mpp_enc_cfg_set_u32(cfg: MppEncCfg, name: *const c_char, val: u32) -> MppRet;

    // ---- Buffer group management ----
    // NOTE: mpp_buffer_group_get_internal / _external are C macros.
    // The real exported symbol is mpp_buffer_group_get (with mode + tag + caller).
    pub fn mpp_buffer_group_get(
        group: *mut MppBufferGroup,
        buf_type: c_int,
        mode: c_int,
        tag: *const c_char,
        caller: *const c_char,
    ) -> MppRet;
    pub fn mpp_buffer_group_put(group: MppBufferGroup) -> MppRet;
    pub fn mpp_buffer_group_clear(group: MppBufferGroup) -> MppRet;

    // ---- Buffer operations ----
    // NOTE: mpp_buffer_get / _put / _get_ptr etc. are C macros.
    // Real exported symbols have _with_tag or _with_caller suffix.
    pub fn mpp_buffer_get_with_tag(
        group: MppBufferGroup,
        buf: *mut MppBuffer,
        size: usize,
        tag: *const c_char,
        caller: *const c_char,
    ) -> MppRet;
    pub fn mpp_buffer_put_with_caller(buf: MppBuffer, caller: *const c_char) -> MppRet;
    pub fn mpp_buffer_inc_ref_with_caller(buf: MppBuffer, caller: *const c_char) -> MppRet;
    pub fn mpp_buffer_get_fd_with_caller(buf: MppBuffer, caller: *const c_char) -> c_int;
    pub fn mpp_buffer_get_ptr_with_caller(buf: MppBuffer, caller: *const c_char) -> *mut c_void;
    pub fn mpp_buffer_get_size_with_caller(buf: MppBuffer, caller: *const c_char) -> usize;
    pub fn mpp_buffer_import_with_tag(
        group: MppBufferGroup,
        info: *const MppBufferInfo,
        buf: *mut MppBuffer,
        tag: *const c_char,
        caller: *const c_char,
    ) -> MppRet;

    // ---- Meta operations ----
    pub fn mpp_meta_get_frame(meta: MppMeta, key: c_int, frame: *mut MppFrame) -> MppRet;
}

// ---------------------------------------------------------------------------
// Safe helper to set an mpp_enc_cfg string key
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// MppBufferMode (from mpp_buffer.h)
// ---------------------------------------------------------------------------

pub const MPP_BUFFER_INTERNAL: c_int = 0;
pub const MPP_BUFFER_EXTERNAL: c_int = 1;

// ---------------------------------------------------------------------------
// Wrapper functions — emulate the C macros that call _with_tag/_with_caller
// ---------------------------------------------------------------------------

const TAG: *const c_char = b"gstmppdarkgst\0".as_ptr() as *const c_char;
const CALLER: *const c_char = b"gstmppdarkgst\0".as_ptr() as *const c_char;

#[inline]
pub unsafe fn mpp_buffer_group_get_internal(
    group: *mut MppBufferGroup,
    buf_type: c_int,
) -> MppRet {
    mpp_buffer_group_get(group, buf_type, MPP_BUFFER_INTERNAL, TAG, CALLER)
}

#[inline]
pub unsafe fn mpp_buffer_group_get_external(
    group: *mut MppBufferGroup,
    buf_type: c_int,
) -> MppRet {
    mpp_buffer_group_get(group, buf_type, MPP_BUFFER_EXTERNAL, TAG, CALLER)
}

#[inline]
pub unsafe fn mpp_buffer_get(
    group: MppBufferGroup,
    buf: *mut MppBuffer,
    size: usize,
) -> MppRet {
    mpp_buffer_get_with_tag(group, buf, size, TAG, CALLER)
}

#[inline]
pub unsafe fn mpp_buffer_put(buf: MppBuffer) -> MppRet {
    mpp_buffer_put_with_caller(buf, CALLER)
}

#[inline]
pub unsafe fn mpp_buffer_inc_ref(buf: MppBuffer) -> MppRet {
    mpp_buffer_inc_ref_with_caller(buf, CALLER)
}

#[inline]
pub unsafe fn mpp_buffer_get_fd(buf: MppBuffer) -> c_int {
    mpp_buffer_get_fd_with_caller(buf, CALLER)
}

#[inline]
pub unsafe fn mpp_buffer_get_ptr(buf: MppBuffer) -> *mut c_void {
    mpp_buffer_get_ptr_with_caller(buf, CALLER)
}

#[inline]
pub unsafe fn mpp_buffer_get_size(buf: MppBuffer) -> usize {
    mpp_buffer_get_size_with_caller(buf, CALLER)
}

// ---------------------------------------------------------------------------
// Safe helper to set an mpp_enc_cfg string key
// ---------------------------------------------------------------------------

pub unsafe fn enc_cfg_set_s32(cfg: MppEncCfg, key: &str, val: i32) -> MppRet {
    let ckey = std::ffi::CString::new(key).unwrap();
    mpp_enc_cfg_set_s32(cfg, ckey.as_ptr(), val)
}

pub unsafe fn enc_cfg_set_u32(cfg: MppEncCfg, key: &str, val: u32) -> MppRet {
    let ckey = std::ffi::CString::new(key).unwrap();
    mpp_enc_cfg_set_u32(cfg, ckey.as_ptr(), val)
}
