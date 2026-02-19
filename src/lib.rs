mod mpp_ffi;

mod enc;
mod dec;

use gstreamer as gst;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    enc::register(plugin)?;
    dec::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    mppdarkgst,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "AGPL-3.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    "https://simplertsp.com",
    env!("BUILD_REL_DATE")
);
