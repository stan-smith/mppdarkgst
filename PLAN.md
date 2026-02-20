# MPP Plugin Rewrite Plan: Vendor-Matched Architecture

## 1. Vendor Architecture Summary

### 1.1 Encoder Architecture (gstmppenc.c + gstmpph265enc.c)

**Threading Model: Async srcpad task**

The vendor encoder uses a **two-thread architecture**:

- **Thread 1 (handle_frame)**: Converts input, queues frame number, increments `pending_frames`, broadcasts to wake the srcpad task. If `pending_frames >= max_pending` (default 16), it blocks waiting for the srcpad task to consume.
- **Thread 2 (srcpad task = `gst_mpp_enc_loop`)**: Runs on `encoder->srcpad` via `gst_pad_start_task`. Waits on condition variable until `pending_frames > 0 || flushing`. Then calls `send_frame_locked` (submits queued frames to MPP via `encode_put_frame`, non-blocking) and `poll_packet_locked` (polls encoded output via `encode_get_packet` with 1ms timeout) in a loop.

This is the **critical performance difference**: the vendor submits input and polls output on separate threads, achieving pipeline parallelism. Our implementation does both synchronously in `handle_frame`, blocking the entire pipeline.

**Buffer Flow:**

1. `handle_frame` calls `gst_mpp_enc_convert()` which:
   - Tries to **import** the input buffer's DmaBuf fd into an MppBuffer (zero-copy via `gst_mpp_allocator_import_gst_memory`)
   - If import fails (not DmaBuf, stride mismatch, rotation needed), falls back to:
     - RGA hardware conversion (if available), or
     - Software `gst_video_frame_copy` (memcpy, like ours)
   - Stores the converted buffer in `frame->output_buffer` (a HACK -- output_buffer is repurposed as temp storage)
2. The srcpad task reads the converted buffer from `frame->output_buffer`, extracts the MppBuffer, and calls `encode_put_frame`.
3. Output packets use **zero-copy by default** (`zero_copy_pkt = TRUE`): the MppBuffer from `encode_get_packet` is imported as DmaBuf GstMemory directly. Fallback is memcpy via `gst_buffer_fill`.

**MPP Timeouts:**
- Input timeout: `MPP_POLL_NON_BLOCK` (0) -- non-blocking input
- Output timeout: **1ms** -- very short polling timeout
- The srcpad task loops with these short timeouts, yielding back to the condition variable wait when idle

**Propose Allocation:**
- The vendor requests the upstream element to use its MppAllocator (DRM-backed DmaBuf), enabling zero-copy import in `gst_mpp_enc_convert`.

**Drain/EOS/Flush:**
- `finish()` (EOS): calls `gst_mpp_enc_reset(drain=TRUE)` which signals the srcpad task to drain pending frames, then stops the task
- `flush()`: calls `gst_mpp_enc_reset(drain=FALSE)` which discards pending frames
- Both call `self->mpi->reset(mpp_ctx)` after the task stops

**Properties:**
- `max-pending` (default 16): max frames queued between handle_frame and srcpad task
- `header-mode`: default `MPP_ENC_HEADER_MODE_DEFAULT` (first frame only)
- `sei-mode`: default disabled
- `rc-mode`: default CBR
- `gop`: default -1 (= FPS)
- `max-reenc`: default 1
- `bps/bps-min/bps-max`: default 0 (auto-calculate: `width * height / 8 * fps`)
- `zero-copy-pkt`: default TRUE -- zero-copy output packet
- `arm-afbc`: AFBC compressed input support

### 1.2 Decoder Architecture (gstmppdec.c + gstmppvideodec.c)

**Threading Model: Async srcpad task (same pattern as encoder)**

- **Thread 1 (handle_frame)**: Maps input buffer, creates MppPacket, submits via `decode_put_packet` with retry loop (2ms intervals, 2s timeout). Replaces `frame->input_buffer` with an empty buffer to free input memory immediately.
- **Thread 2 (srcpad task = `gst_mpp_dec_loop`)**: Calls `klass->poll_mpp_frame(timeout)` which calls `decode_get_frame`. Uses `MPP_OUTPUT_TIMEOUT_MS = 200ms` timeout normally, `MPP_POLL_NON_BLOCK` when flushing. Handles info_change, error frames, interlace detection.

**Buffer Flow (Zero-copy output):**

The vendor decoder uses an **external buffer group** (`MPP_DEC_SET_EXT_BUF_GROUP`) -- the MppAllocator's internal group is given to MPP so MPP allocates output buffers from it. Then:

1. `gst_mpp_dec_get_gst_buffer()` extracts `mpp_frame_get_buffer(mframe)` -> `mbuf`
2. Sets `mpp_buffer_set_index(mbuf, allocator->index)` to claim ownership
3. Imports the MppBuffer as DmaBuf GstMemory via `gst_mpp_allocator_import_mppbuf()`
4. **No memcpy** -- the DmaBuf fd from MPP's output is directly wrapped as GstMemory
5. When downstream releases the GstBuffer, the MppBuffer refcount drops and MPP can reuse it

**Frame Matching:**
- Complex PTS-based matching with `gst_mpp_dec_get_frame()`:
  - First frame: detect whether to use MPP PTS or original PTS
  - Subsequent: match by closest PTS within 5ms tolerance
  - Fallback: oldest frame
  - Handles interlaced, decode-only, and out-of-order frames
- Our implementation simply uses `oldest_frame()` which is correct for simple cases

**Info Change:**
- Calls `MPP_DEC_SET_INFO_CHANGE_READY` to acknowledge
- Extracts width/height/strides/format from the info_change MppFrame
- Negotiates output caps via `gst_video_decoder_set_output_state`
- Saves last MppFrame for detecting subsequent info changes

**mpp_init Timing:**
- `mpp_create()` in `start()`, but `mpp_init()` deferred to `set_format()` (after knowing codec type)
- `MPP_DEC_SET_PARSER_FAST_MODE` set **before** `mpp_init()` (required)
- Our implementation also does this correctly

**Drain/EOS:**
- `shutdown()` with `drain=TRUE`: sends EOS packet via `decode_put_packet`, returns TRUE so the task loop continues polling until EOS MppFrame
- `shutdown()` with `drain=FALSE`: calls `mpi->reset()` to abort immediately, returns FALSE

**Properties:**
- `ignore-error`: default TRUE -- ignore decode errors
- `fast-mode`: default TRUE -- enable parser fast mode
- `format`: output format (auto/NV12/etc.) -- requires RGA
- `arm-afbc`: AFBC output -- disabled by default

### 1.3 Allocator Architecture (gstmppallocator.c)

**GstDmaBufAllocator subclass:**
- Inherits from `GstDmaBufAllocator` (which inherits from `GstFdAllocator`)
- Two MppBufferGroups: `group` (internal, for allocations) and `ext_group` (external, for imports)
- Unique `index` per allocator instance (for tracking ownership)
- `cacheable` flag controls whether freed buffers are recycled or cleared

**Key Methods:**
- `gst_mpp_allocator_alloc()`: Allocates from internal group, wraps as DmaBuf GstMemory via `gst_fd_allocator_alloc(dup(fd))`, attaches MppBuffer as qdata
- `gst_mpp_allocator_import_mppbuf()`: Takes an MppBuffer (from MPP output), checks index:
  - Same group: `gst_fd_allocator_alloc(dup(fd))` + attach as qdata
  - Different group: import via ext_group first, then wrap
- `gst_mpp_allocator_import_gst_memory()`: Takes a GstMemory, checks if it's DmaBuf, extracts fd, imports into ext_group
- `gst_mpp_mpp_buffer_from_gst_memory()`: Retrieves MppBuffer from qdata (for getting the DRM buffer back from a GstMemory)

**Buffer Lifecycle:**
- MppBuffer refcount is incremented when wrapped as GstMemory
- `gst_mpp_mem_destroy` (qdata destroy notify) calls `mpp_buffer_put`
- When GstMemory is freed, MppBuffer refcount drops, allowing MPP to reuse it

### 1.4 Common Utilities (gstmpp.c + gstmpp.h)

- Format conversion tables (GstVideoFormat <-> MppFrameFormat <-> RgaSURF_FORMAT)
- `gst_mpp_video_info_align()`: Aligns strides to MPP requirements (16-byte default)
- `gst_mpp_frame_info_changed()`: Compares two MppFrames for info changes
- RGA (Rockchip Graphics Acceleration) hardware conversion helpers
- `GST_MPP_ALIGNMENT = 16`
- `GST_MPP_VIDEO_INFO_HSTRIDE/VSTRIDE` macros for extracting strides from GstVideoInfo

---

## 2. Diff Analysis: Key Differences Between Vendor and Our Implementation

### 2.1 Encoder: Critical Differences

| Aspect | Vendor | Ours | Impact |
|--------|--------|------|--------|
| **Threading** | Async srcpad task (2 threads) | Synchronous handle_frame (1 thread) | **1.8x perf gap** -- pipeline stall |
| **Input buffer** | Zero-copy DmaBuf import when possible, RGA/memcpy fallback | Always memcpy to MppBuffer | Memcpy overhead, ~30% of CPU |
| **Output buffer** | Zero-copy DmaBuf wrap (default) | Always memcpy from MppPacket buffer | Unnecessary copy |
| **Output timeout** | 1ms (tight polling in task loop) | 200ms (blocking in handle_frame) | 200x longer stall per poll |
| **Pending frames** | Up to 16 frames queued (pipeline depth) | 0 (fully synchronous) | No MPP pipeline utilization |
| **Propose allocation** | Requests DmaBuf/MppAllocator from upstream | Only adds VideoMeta | Missed zero-copy opportunity |
| **Header mode** | Default: first frame only | EACH_IDR | Minor bitrate overhead |
| **BPS auto-calc** | `width * height / 8 * fps` | Fixed 4Mbps default | Wrong for non-1080p |

### 2.2 Decoder: Critical Differences

| Aspect | Vendor | Ours | Impact |
|--------|--------|------|--------|
| **Threading** | Async srcpad task | Synchronous handle_frame | Pipeline stall |
| **Output buffer** | Zero-copy DmaBuf from MPP's buffer | memcpy Y/UV planes to new GstBuffer | Major CPU waste |
| **Ext buffer group** | Passes allocator's group to MPP via `MPP_DEC_SET_EXT_BUF_GROUP` | Not set (MPP uses internal group) | Can't zero-copy output |
| **Frame matching** | Complex PTS-based matching | `oldest_frame()` only | OK for simple pipelines |
| **Input buffer cleanup** | Replaces input with empty buffer to free memory immediately | Keeps input buffer alive | Memory waste on tight devices |
| **Output timeout** | 200ms (via MPP control, changes dynamically) | 200ms (hardcoded) | Same (OK) |

### 2.3 Allocator Differences

| Aspect | Vendor | Ours |
|--------|--------|------|
| **Type** | GstDmaBufAllocator subclass (GObject) | Plain Rust struct with raw GstAllocator ptr |
| **Cacheable** | Configurable, encoder disables caching | Not implemented |
| **Import flow** | Integrated with GstMemory qdata system | Same approach (qdata-based guard) |

---

## 3. Implementation Plan

### Phase 1: Async Srcpad Task for Encoder (Highest Impact)

This is the single biggest performance improvement. The vendor achieves 1.8x more throughput by decoupling input submission from output polling.

**Step 1.1: Add srcpad task infrastructure to encoder**

The encoder state needs shared state accessible from both handle_frame and the task loop. Use an `Arc<Mutex<...>>` or move shared fields into a separate structure protected by a Mutex:

```
pending_frames: u32,
frames_queue: VecDeque<u32>,  // system_frame_numbers ready to submit
flushing: bool,
draining: bool,
task_ret: Result<gst::FlowSuccess, gst::FlowError>,
```

Add a Condvar for signaling between handle_frame and the loop.

**Step 1.2: Rewrite handle_frame to be non-blocking**

1. Convert input buffer (memcpy for now, DmaBuf import later)
2. Store converted buffer in frame.output_buffer (same HACK as vendor)
3. If `pending_frames >= max_pending`, release stream lock, wait on condvar, reacquire
4. Increment `pending_frames`, append frame number to `frames_queue`
5. Signal condvar to wake srcpad task
6. Start srcpad task if not running: `pad.start_task(enc_loop_fn)`
7. Return `task_ret`

**Step 1.3: Implement encoder srcpad loop**

The srcpad task function (`gst_mpp_enc_loop` equivalent):

1. Wait on condvar until `pending_frames > 0 || flushing`
2. Acquire stream lock
3. `send_frames_locked`: loop through frames_queue, for each:
   - `gst_video_encoder_get_frame(frame_number)` to re-acquire the frame
   - Extract MppBuffer from frame.output_buffer's first GstMemory
   - Create MppFrame, set buffer/format/strides
   - Call `encode_put_frame` (non-blocking)
   - Remove from frames_queue
4. `poll_packets_locked`: loop calling `encode_get_packet` (1ms timeout):
   - For each packet: get `oldest_frame`, extract encoded data
   - Set as frame.output_buffer, call `finish_frame`
   - Decrement `pending_frames`, signal condvar
5. Release stream lock
6. If flushing and no pending: pause task

**Step 1.4: Implement flush/finish/reset**

Match vendor's `gst_mpp_enc_reset`:
- Set `flushing = TRUE`
- If draining: let srcpad task drain remaining frames
- If not draining: set `pending_frames = 0`, discard frame queue
- Signal condvar to wake task
- Wait for task to pause
- Call `mpi->reset(mpp_ctx)`
- Reset state

### Phase 2: Zero-Copy Output for Encoder

Instead of copying encoded packet data, wrap the MppBuffer from `mpp_packet_get_buffer` as DmaBuf GstMemory:

```rust
let mbuf = mpp_packet_get_buffer(pkt);
let pkt_len = mpp_packet_get_length(pkt);
mpp_buffer_set_index(mbuf, allocator.index());
let mem = wrap_mpp_buffer_as_dmabuf_memory(&allocator, mbuf, pkt_len);
let buffer = gst::Buffer::new();
buffer.append_memory(mem);
```

This eliminates the output memcpy. Use `wrap_mpp_buffer_as_dmabuf_memory` from our existing `allocator.rs`.

### Phase 3: Zero-Copy Input for Encoder (DmaBuf Import)

**Step 3.1: Enhance propose_allocation**

Request upstream to provide DmaBuf buffers by adding our MppAllocator to the allocation query. The vendor creates a `gst_video_buffer_pool_new()` with its allocator and adds it to the query.

**Step 3.2: Attempt DmaBuf import before memcpy fallback**

In handle_frame:
1. Check if input buffer has single DmaBuf memory
2. Try to extract MppBuffer from qdata (if from our decoder)
3. If found: use directly -- TRUE zero copy
4. If not found but is DmaBuf: import fd via `allocator.import_dmabuf_fd`
5. Verify stride compatibility via VideoMeta
6. If successful: zero-copy, no memcpy needed
7. If failed: fall back to memcpy (current behavior)

**Note:** This requires upstream (v4l2src, appsrc with DmaBuf) to actually provide DmaBuf buffers. In our HDMI pipeline, this may not apply, so this is lower priority.

### Phase 4: Async Srcpad Task for Decoder

Same pattern as encoder:

**Step 4.1: Add srcpad task to decoder**

The decoder srcpad loop:
1. Call `decode_get_frame` with 200ms timeout (MPP_OUTPUT_TIMEOUT_MS)
2. Handle info_change (ack, negotiate output)
3. Handle error/discard frames
4. For valid frames: wrap decoded MppBuffer as DmaBuf GstMemory
5. Find matching frame via `oldest_frame()`, finish it

**Step 4.2: Make handle_frame non-blocking**

1. Map input, create MppPacket, set PTS
2. Submit via `decode_put_packet` (retry loop, 2ms intervals, 2s timeout)
3. Start srcpad task if not running
4. Return `task_ret`

### Phase 5: Zero-Copy Decoder Output

**Step 5.1: Set external buffer group**

In decoder startup, after creating MppAllocator:
```rust
let group = allocator.mpp_group();
control(mpp_ctx, MPP_DEC_SET_EXT_BUF_GROUP, group);
```

This tells MPP to allocate decoded frames from our buffer group, enabling zero-copy output.

**Step 5.2: Wrap decoded frames as DmaBuf**

Instead of memcpy, import the MppBuffer from decoded frame:
```rust
let mbuf = mpp_frame_get_buffer(mframe);
mpp_buffer_set_index(mbuf, allocator.index());
let mem = wrap_mpp_buffer_as_dmabuf_memory(&allocator, mbuf, frame_size);
// Add VideoMeta with MPP strides (hor_stride, ver_stride)
```

This eliminates the per-frame memcpy in the decoder (3.1MB per frame at 1080p NV12).

### Phase 6: Output Timeout Tuning

**Step 6.1: Encoder output timeout = 1ms**

Change from 200ms to 1ms. The srcpad task loop provides the effective timeout through repeated polling. This matches the vendor:
```c
timeout = 1;
self->mpi->control(self->mpp_ctx, MPP_SET_OUTPUT_TIMEOUT, &timeout);
```

**Step 6.2: Decoder output timeout = 200ms (keep as-is)**

The 200ms decoder timeout matches vendor behavior exactly.

---

## 4. FFI Changes Needed

### New FFI functions required:

None. All needed functions are already declared in `mpp_ffi.rs`:
- `mpp_buffer_set_index` / `mpp_buffer_get_index` -- already present
- `mpp_packet_get_buffer` / `mpp_packet_get_meta` / `mpp_meta_get_frame` -- already present
- `mpp_frame_set_pts` / `mpp_frame_get_pts` -- already present
- `mpp_packet_set_eos` / `mpp_packet_set_extra_data` -- already present
- Buffer group / buffer management -- already present
- All `MpiCmd` constants -- already present

### GstPad task functions:

Use gstreamer-rs bindings (`gst::Pad::start_task`, `gst::Pad::stop_task`, `gst::Pad::pause_task`). No raw FFI needed.

---

## 5. Risk Assessment

### High Risk

1. **Srcpad task thread safety**: The vendor uses `GST_VIDEO_ENCODER_STREAM_LOCK/UNLOCK` carefully around the srcpad task. In Rust with gstreamer-rs, we need to ensure the `VideoEncoder` stream lock is correctly managed. The `handle_frame` method already holds the stream lock, and we need to release it before waiting on the condvar (like the vendor's `GST_MPP_ENC_LOCK` macro does: unlock stream, lock mutex, relock stream).

   In gstreamer-rs, the stream lock is not directly exposed. We may need to use the `VideoEncoder`/`VideoDecoder` extension traits that provide `stream_lock`/`stream_unlock`, or work with the underlying `gst::Pad` API.

2. **Frame lifetime management**: Storing converted buffers in `frame.output_buffer` and later reading them from the srcpad task requires that the GstVideoCodecFrame remains valid. The vendor uses `gst_video_encoder_get_frame(frame_number)` to re-acquire frame references from the system_frame_number. In gstreamer-rs, this is available as `VideoEncoder::frame(system_frame_number)`.

3. **Zero-copy output race**: When using zero-copy DmaBuf output, the MppBuffer must stay alive until downstream is done with it. The qdata guard mechanism handles this, but we need to verify that MPP's internal buffer recycling doesn't conflict. Our existing `MppBufGuard` + `wrap_mpp_buffer_as_dmabuf_memory` pattern should work correctly.

### Medium Risk

4. **Condvar/mutex ordering**: The vendor carefully orders lock acquisition. Getting this wrong causes deadlocks. Consider using `parking_lot` mutexes for lock-order debugging. Key pattern to follow: always release stream lock before acquiring our internal mutex, then reacquire stream lock after (matching vendor's `GST_MPP_ENC_LOCK` macro).

5. **propose_allocation interaction**: If upstream ignores our allocation proposal (common with appsrc), we must gracefully fall back to memcpy. The vendor handles this in `gst_mpp_enc_convert` -- if DmaBuf import fails, it falls through to RGA or software copy.

6. **Srcpad task on decoder**: The decoder srcpad task calls `gst_video_decoder_finish_frame` which may trigger downstream negotiation. This must happen under the stream lock. The vendor acquires `GST_VIDEO_DECODER_STREAM_LOCK` at the top of `gst_mpp_dec_loop` and releases at the bottom.

### Low Risk

7. **Encoder output timeout change (200ms -> 1ms)**: Safe since the srcpad task loop provides the retry mechanism.

8. **Header mode change**: Switching from `EACH_IDR` to `DEFAULT` (first frame only) requires downstream to handle the initial SPS/PPS-only frame correctly. For RTSP with `h265parse config-interval=-1`, this should be fine since h265parse will insert headers periodically. But given our appsrc architecture skips h265parse, we should keep `EACH_IDR` mode.

### Testing Plan

1. **Benchmark**: Compare `gst-launch-1.0 videotestsrc ! mpph265enc ! fakesink` fps before/after each phase
2. **Memory**: Monitor RSS on Radxa -- zero-copy should reduce memory pressure
3. **RTSP**: Full pipeline test with HDMI capture -> mpph265enc -> rtph265pay -> RTSP
4. **Decoder**: Test with `rtspsrc ! mppvideodec ! kmssink/fakesink`
5. **Stress**: Signal loss/recovery cycles to test flush/reset paths
6. **Latency**: Measure end-to-end latency (async task adds minimal latency due to 1ms poll + condvar wake)

---

## 6. Implementation Priority

1. **Phase 1** (async encoder task) -- addresses the 1.8x perf gap, highest ROI
2. **Phase 2** (zero-copy encoder output) -- easy win, small code change
3. **Phase 6** (timeout tuning) -- trivial, do alongside Phase 1
4. **Phase 5** (zero-copy decoder output) -- significant for decoder use cases
5. **Phase 4** (async decoder task) -- same pattern as encoder, lower priority if decoder perf is OK
6. **Phase 3** (zero-copy encoder input) -- depends on upstream DmaBuf support

---

## 7. What to Skip

The vendor plugin has many features we do NOT need:

- **RGA conversion** -- we only use NV12, no rotation, no format conversion
- **ARM AFBC** -- not used in our pipeline
- **Multi-codec encoder** (H.264/VP8/JPEG enc) -- keep H.264 fallback but don't add new codecs
- **Interlace mode handling** -- HDMI input is progressive
- **Crop rectangle** -- not used
- **DMA feature caps** -- not needed for our internal pipeline
- **PTS fixup / frame reordering** -- our decoder uses oldest-frame, which is adequate
- **Cache management** (`set_cacheable`) -- skip initially
- **Most properties**: Hard-code `max_pending=16` and `zero_copy_pkt=true`
