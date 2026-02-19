use glib::prelude::*;
use gstreamer as gst;
use gstreamer_video as gst_video;

mod imp;

glib::wrapper! {
    pub struct MppVideoDec(ObjectSubclass<imp::MppVideoDec>)
        @extends gst_video::VideoDecoder, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "mppvideodec",
        gst::Rank::PRIMARY + 1,
        MppVideoDec::static_type(),
    )
}
