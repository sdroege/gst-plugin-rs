//! NDI Closed Caption encoder and parser
//!
//! See:
//!
//! * http://www.sienna-tv.com/ndi/ndiclosedcaptions.html
//! * http://www.sienna-tv.com/ndi/ndiclosedcaptions608.html

use anyhow::{bail, Context, Result};
use data_encoding::BASE64;
use smallvec::SmallVec;

use crate::video_anc;
use crate::video_anc::VideoAncillaryAFD;

const C608_TAG: &str = "C608";
const C608_TAG_BYTES: &[u8] = C608_TAG.as_bytes();

const C708_TAG: &str = "C708";
const C708_TAG_BYTES: &[u8] = C708_TAG.as_bytes();

const LINE_ATTR: &str = "line";
const DEFAULT_LINE_VALUE: &str = "21";

/// Video anc AFD content padded to 32bit alignment encoded in base64
const NDI_CC_CONTENT_MAX_LEN: usize = (video_anc::VIDEO_ANC_AFD_MAX_LEN + 3) * 3 / 2;

/// Video anc AFD padded to 32bit alignment encoded in base64
/// + XML tags with brackets and end '/'
const NDI_CC_MAX_LEN: usize = NDI_CC_CONTENT_MAX_LEN + 13;

#[derive(thiserror::Error, Debug, Eq, PartialEq)]
/// NDI Video Caption related Errors.
pub enum NDIClosedCaptionError {
    #[error("Unsupported closed caption type {cc_type:?}")]
    UnsupportedCC {
        cc_type: gst_video::VideoCaptionType,
    },
}

impl NDIClosedCaptionError {
    pub fn is_unsupported_cc(&self) -> bool {
        matches!(self, Self::UnsupportedCC { .. })
    }
}

fn write_32bit_padded_base64<W>(writer: &mut quick_xml::writer::Writer<W>, data: &[u8])
where
    W: std::io::Write,
{
    use quick_xml::events::{BytesText, Event};
    use std::borrow::Cow;

    let mut buf = String::with_capacity(NDI_CC_CONTENT_MAX_LEN);
    let mut input = Cow::from(data);

    let alignment_rem = input.len() % 4;
    if alignment_rem != 0 {
        let owned = input.to_mut();
        let mut padding = 4 - alignment_rem;
        while padding != 0 {
            owned.push(0);
            padding -= 1;
        }
    }

    debug_assert_eq!(input.len() % 4, 0);

    buf.clear();
    BASE64.encode_append(&input, &mut buf);
    writer
        .write_event(Event::Text(BytesText::from_escaped(buf)))
        .unwrap();
}

/// Encodes the provided VideoCaptionMeta in an NDI closed caption metadata.
pub fn encode_video_caption_meta(video_buf: &gst::BufferRef) -> Result<Option<String>> {
    use crate::video_anc::VideoAncillaryAFDEncoder;
    use quick_xml::events::{BytesEnd, BytesStart, Event};
    use quick_xml::writer::Writer;

    if video_buf.meta::<gst_video::VideoCaptionMeta>().is_none() {
        return Ok(None);
    }

    // Start with an initial capacity suitable to store one ndi cc metadata
    let mut writer = Writer::new(Vec::<u8>::with_capacity(NDI_CC_MAX_LEN));

    let cc_meta_iter = video_buf.iter_meta::<gst_video::VideoCaptionMeta>();
    for cc_meta in cc_meta_iter {
        if cc_meta.data().is_empty() {
            return Ok(None);
        }

        use gst_video::VideoCaptionType::*;
        match cc_meta.caption_type() {
            Cea608Raw => {
                let mut anc_afd = VideoAncillaryAFDEncoder::for_cea608_raw(21);
                anc_afd.push_data(cc_meta.data()).context("Cea608Raw")?;

                let mut elem = BytesStart::new(C608_TAG);
                elem.push_attribute((LINE_ATTR, DEFAULT_LINE_VALUE));
                writer.write_event(Event::Start(elem)).unwrap();

                write_32bit_padded_base64(&mut writer, anc_afd.terminate().as_slice());

                writer
                    .write_event(Event::End(BytesEnd::new(C608_TAG)))
                    .unwrap();
            }
            Cea608S3341a => {
                let mut anc_afd = VideoAncillaryAFDEncoder::for_cea608_s334_1a();
                anc_afd.push_data(cc_meta.data()).context("Cea608S3341a")?;

                let mut elem = BytesStart::new(C608_TAG);
                elem.push_attribute((LINE_ATTR, format!("{}", cc_meta.data()[0]).as_str()));
                writer.write_event(Event::Start(elem)).unwrap();

                write_32bit_padded_base64(&mut writer, anc_afd.terminate().as_slice());
                writer
                    .write_event(Event::End(BytesEnd::new(C608_TAG)))
                    .unwrap();
            }
            Cea708Cdp => {
                let mut anc_afd = VideoAncillaryAFDEncoder::for_cea708_cdp();
                anc_afd.push_data(cc_meta.data()).context("Cea708Cdp")?;

                writer
                    .write_event(Event::Start(BytesStart::new(C708_TAG)))
                    .unwrap();
                write_32bit_padded_base64(&mut writer, anc_afd.terminate().as_slice());
                writer
                    .write_event(Event::End(BytesEnd::new(C708_TAG)))
                    .unwrap();
            }
            other => bail!(NDIClosedCaptionError::UnsupportedCC { cc_type: other }),
        }
    }

    // # Safety
    // `writer` content is guaranteed to be a valid UTF-8 string since:
    // * It contains ASCII XML tags, ASCII XML attributes and base64 encoded content
    // * ASCII & base64 are subsets of UTF-8.
    unsafe {
        let ndi_cc_meta_b = writer.into_inner();
        let ndi_cc_meta = std::str::from_utf8_unchecked(&ndi_cc_meta_b);

        Ok(Some(ndi_cc_meta.into()))
    }
}

#[derive(Debug)]
pub struct NDIClosedCaption {
    pub cc_type: gst_video::VideoCaptionType,
    pub data: VideoAncillaryAFD,
}

/// Parses the provided NDI metadata string, searching for
/// an NDI closed caption metadata.
pub fn parse_ndi_cc_meta(input: &str) -> Result<Vec<NDIClosedCaption>> {
    use crate::video_anc::VideoAncillaryAFDParser;
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut ndi_cc = Vec::new();

    let mut reader = Reader::from_str(input);
    reader.trim_text(true);

    let mut content = SmallVec::<[u8; NDI_CC_CONTENT_MAX_LEN]>::new();
    let mut buf = Vec::with_capacity(NDI_CC_MAX_LEN);
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(_) => content.clear(),
            Event::Text(e) => content.extend(e.iter().copied()),
            Event::End(e) => match e.name().as_ref() {
                C608_TAG_BYTES => {
                    let adf_packet = BASE64.decode(content.as_slice()).context(C608_TAG)?;

                    let data =
                        VideoAncillaryAFDParser::parse_for_cea608(&adf_packet).context(C608_TAG)?;

                    ndi_cc.push(NDIClosedCaption {
                        cc_type: gst_video::VideoCaptionType::Cea608S3341a,
                        data,
                    });
                }
                C708_TAG_BYTES => {
                    let adf_packet = BASE64.decode(content.as_slice()).context(C708_TAG)?;

                    let data =
                        VideoAncillaryAFDParser::parse_for_cea708(&adf_packet).context(C708_TAG)?;

                    ndi_cc.push(NDIClosedCaption {
                        cc_type: gst_video::VideoCaptionType::Cea708Cdp,
                        data,
                    });
                }
                _ => (),
            },
            _ => {}
        }

        buf.clear();
    }

    Ok(ndi_cc)
}

#[cfg(test)]
mod tests {
    use super::*;

    use gst_video::VideoCaptionType;

    #[test]
    fn encode_gst_meta_c608() {
        gst::init().unwrap();

        let mut buf = gst::Buffer::new();
        {
            let buf = buf.get_mut().unwrap();
            gst_video::VideoCaptionMeta::add(
                buf,
                VideoCaptionType::Cea608S3341a,
                &[0x80, 0x94, 0x2c],
            );
        }

        assert_eq!(
            encode_video_caption_meta(&buf).unwrap().unwrap(),
            "<C608 line=\"128\">AD///WFAoDYBlEsqYAAAAA==</C608>",
        );
    }

    #[test]
    fn encode_gst_meta_c708() {
        gst::init().unwrap();

        let mut buf = gst::Buffer::new();
        {
            let buf = buf.get_mut().unwrap();
            gst_video::VideoCaptionMeta::add(
                buf,
                VideoCaptionType::Cea708Cdp,
                &[
                    0x96, 0x69, 0x55, 0x3f, 0x43, 0x00, 0x00, 0x72, 0xf8, 0xfc, 0x94, 0x2c, 0xf9,
                    0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00,
                    0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                    0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                    0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00,
                    0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                    0xfa, 0x00, 0x00, 0x74, 0x00, 0x00, 0x1b,
                ],
            );
        }

        assert_eq!(
            encode_video_caption_meta(&buf).unwrap().unwrap(),
            "<C708>AD///WFAZVpaaZVj9Q4AgCcn4vxlEsvmAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAnSAIAhutwA=</C708>",
        );
    }

    #[test]
    fn encode_gst_meta_c608_and_c708() {
        gst::init().unwrap();

        let mut buf = gst::Buffer::new();
        {
            let buf = buf.get_mut().unwrap();
            gst_video::VideoCaptionMeta::add(
                buf,
                VideoCaptionType::Cea608S3341a,
                &[0x80, 0x94, 0x2c],
            );
            gst_video::VideoCaptionMeta::add(
                buf,
                VideoCaptionType::Cea708Cdp,
                &[
                    0x96, 0x69, 0x55, 0x3f, 0x43, 0x00, 0x00, 0x72, 0xf8, 0xfc, 0x94, 0x2c, 0xf9,
                    0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00,
                    0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                    0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                    0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00,
                    0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                    0xfa, 0x00, 0x00, 0x74, 0x00, 0x00, 0x1b,
                ],
            );
        }

        assert_eq!(
            encode_video_caption_meta(&buf).unwrap().unwrap(),
            "<C608 line=\"128\">AD///WFAoDYBlEsqYAAAAA==</C608><C708>AD///WFAZVpaaZVj9Q4AgCcn4vxlEsvmAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAnSAIAhutwA=</C708>",
        );
    }

    #[test]
    fn encode_gst_meta_unsupported_cc() {
        gst::init().unwrap();

        let mut buf = gst::Buffer::new();
        {
            let buf = buf.get_mut().unwrap();
            gst_video::VideoCaptionMeta::add(
                buf,
                VideoCaptionType::Cea708Raw,
                // Content doesn't matter here
                &[0x00, 0x01, 0x02, 0x03, 0x04, 0x05],
            );
        }

        let err = encode_video_caption_meta(&buf)
            .unwrap_err()
            .downcast::<NDIClosedCaptionError>()
            .unwrap();
        assert_eq!(
            err,
            NDIClosedCaptionError::UnsupportedCC {
                cc_type: VideoCaptionType::Cea708Raw
            }
        );

        assert!(err.is_unsupported_cc());
    }

    #[test]
    fn encode_gst_meta_none() {
        gst::init().unwrap();

        let buf = gst::Buffer::new();
        assert!(encode_video_caption_meta(&buf).unwrap().is_none());
    }

    #[test]
    fn parse_ndi_meta_c608() {
        let mut ndi_cc_list =
            parse_ndi_cc_meta("<C608 line=\"128\">AD///WFAoDYBlEsqYAAAAA==</C608>").unwrap();

        let ndi_cc = ndi_cc_list.pop().unwrap();
        assert_eq!(ndi_cc.cc_type, VideoCaptionType::Cea608S3341a);
        assert_eq!(ndi_cc.data.as_slice(), [0x80, 0x94, 0x2c]);

        assert!(ndi_cc_list.is_empty());
    }

    #[test]
    fn parse_ndi_meta_c708() {
        let mut ndi_cc_list = parse_ndi_cc_meta(
                "<C708>AD///WFAZVpaaZVj9Q4AgCcn4vxlEsvmAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAnSAIAhutwA=</C708>",
            )
            .unwrap();

        let ndi_cc = ndi_cc_list.pop().unwrap();
        assert_eq!(ndi_cc.cc_type, VideoCaptionType::Cea708Cdp);
        assert_eq!(
            ndi_cc.data.as_slice(),
            [
                0x96, 0x69, 0x55, 0x3f, 0x43, 0x00, 0x00, 0x72, 0xf8, 0xfc, 0x94, 0x2c, 0xf9, 0x00,
                0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00,
                0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0x74, 0x00, 0x00,
                0x1b,
            ]
        );

        assert!(ndi_cc_list.is_empty());
    }

    #[test]
    fn parse_ndi_meta_c608_and_c708() {
        let ndi_cc_list = parse_ndi_cc_meta(
                "<C608 line=\"128\">AD///WFAoDYBlEsqYAAAAA==</C608><C708>AD///WFAZVpaaZVj9Q4AgCcn4vxlEsvmAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAvqAIAnSAIAhutwA=</C708>",
            )
            .unwrap();

        let mut ndi_cc_iter = ndi_cc_list.iter();

        let ndi_cc = ndi_cc_iter.next().unwrap();
        assert_eq!(ndi_cc.cc_type, VideoCaptionType::Cea608S3341a);
        assert_eq!(ndi_cc.data.as_slice(), [0x80, 0x94, 0x2c]);

        let ndi_cc = ndi_cc_iter.next().unwrap();
        assert_eq!(ndi_cc.cc_type, VideoCaptionType::Cea708Cdp);
        assert_eq!(
            ndi_cc.data.as_slice(),
            [
                0x96, 0x69, 0x55, 0x3f, 0x43, 0x00, 0x00, 0x72, 0xf8, 0xfc, 0x94, 0x2c, 0xf9, 0x00,
                0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00,
                0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0x74, 0x00, 0x00,
                0x1b,
            ]
        );

        assert!(ndi_cc_iter.next().is_none());
    }

    #[test]
    fn parse_ndi_meta_tag_mismatch() {
        // Expecting </C608> found </C708>'
        let _ =
            parse_ndi_cc_meta("<C608 line=\"128\">AD///WFAoDYBlEsqYAAAAA==</C708>").unwrap_err();
    }

    #[test]
    fn parse_ndi_meta_c608_deeper_failure() {
        // Caused by:
        // 0: Parsing anc data flags
        // 1: Not enough data'
        let _ = parse_ndi_cc_meta("<C608 line=\"128\">AAA=</C608>").unwrap_err();
    }
}
