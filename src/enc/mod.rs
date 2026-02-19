use glib::prelude::*;
use gstreamer as gst;
use gstreamer_video as gst_video;

mod imp;

glib::wrapper! {
    pub struct MppH265Enc(ObjectSubclass<imp::MppH265Enc>)
        @extends gst_video::VideoEncoder, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "mpph265enc",
        gst::Rank::PRIMARY + 1,
        MppH265Enc::static_type(),
    )
}
