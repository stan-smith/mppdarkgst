use glib::prelude::*;
use gstreamer as gst;
use gstreamer_video as gst_video;

mod imp;

glib::wrapper! {
    pub struct MppJpegDec(ObjectSubclass<imp::MppJpegDec>)
        @extends gst_video::VideoDecoder, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "mppjpegdec",
        gst::Rank::PRIMARY + 1,
        MppJpegDec::static_type(),
    )
}
