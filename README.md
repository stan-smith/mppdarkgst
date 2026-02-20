gst-mppdarkgst

GStreamer plugin written in Rust for Rockchip MPP hardware video encoding
and decoding. Drop-in replacement for the vendor gstreamer1.0-rockchip1
C plugin.

Elements:
  mpph265enc - H.265/HEVC encoder (NV12 input, byte-stream output)
  mppvideodec - H.264/H.265 decoder (byte-stream input, NV12 output)

Both elements use async srcpad tasks with zero-copy DMA-BUF output.
The encoder accepts bps (bitrate) and gop properties.

Requires librockchip_mpp.so and libgstallocators-1.0.so at runtime.

Build (cross-compile for aarch64):
  cross build --release --target aarch64-unknown-linux-gnu

The output is target/aarch64-unknown-linux-gnu/release/libgstmppdarkgst.so.
Set GST_PLUGIN_PATH to the directory containing it.

Tested on Radxa Zero 3E (RK3566) running GStreamer 1.22.
