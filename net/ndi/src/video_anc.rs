//! Video Ancillary Active Format Description (AFD) encoder and parser
//! see SMPTE-291M

use anyhow::{bail, Context, Result};
use smallvec::SmallVec;

#[derive(thiserror::Error, Debug, Eq, PartialEq)]
/// Video Ancillary AFD related Errors.
pub enum VideoAncillaryAFDError {
    #[error("Unexpected data count {found}. Expected: {expected}")]
    UnexpectedDataCount { found: u8, expected: u8 },

    #[error("Not enough data")]
    NotEnoughData,

    #[error("Unexpected data flags")]
    UnexpectedDataFlags,

    #[error("Unexpected checksum {found}. Expected: {expected}")]
    WrongChecksum { found: u16, expected: u16 },

    #[error("Unexpected did {found}. Expected: {expected}")]
    UnexpectedDID { found: u16, expected: u16 },
}

const ANCILLARY_DATA_FLAGS: [u16; 3] = [0x000, 0x3ff, 0x3ff];
const EIA_708_ANCILLARY_DID_16: u16 = 0x6101;
const EIA_608_ANCILLARY_DID_16: u16 = 0x6102;

// Video anc AFD content:
// ADF + DID/SDID + DATA COUNT + PAYLOAD + checksum:
// 3 + 2 + 1 + 256 max + 1 = 263
// Those are 10bit words, so we need 329 bytes max.
pub const VIDEO_ANC_AFD_CAPACITY: usize = 329;

pub type VideoAncillaryAFD = SmallVec<[u8; VIDEO_ANC_AFD_CAPACITY]>;

fn with_afd_parity(val: u8) -> u16 {
    let p = (val.count_ones() % 2) as u16;
    (1 - p) << 9 | p << 8 | (val as u16)
}

#[derive(Debug)]
/// Video Ancillary Active Format Description (AFD) Encoder
pub struct VideoAncillaryAFDEncoder {
    data: VideoAncillaryAFD,
    offset: u8,
    checksum: u16,
    data_count: u8,
    expected_data_count: Option<u8>,
}

impl VideoAncillaryAFDEncoder {
    pub fn for_cea608_raw(line: u8) -> Self {
        let mut this = Self::new(EIA_608_ANCILLARY_DID_16);
        this.expected_data_count = Some(3);
        this.push_data(&[line]).unwrap();

        this
    }

    pub fn for_cea608_s334_1a() -> Self {
        let mut this = Self::new(EIA_608_ANCILLARY_DID_16);
        this.expected_data_count = Some(3);

        this
    }

    pub fn for_cea708_cdp() -> Self {
        Self::new(EIA_708_ANCILLARY_DID_16)
    }

    fn new(did16: u16) -> Self {
        let mut this = VideoAncillaryAFDEncoder {
            data: SmallVec::new(),
            offset: 0,
            checksum: 0,
            data_count: 0,
            expected_data_count: None,
        };

        // Ancillary Data Flag, component AFD description
        this.push_raw_10bit_word(ANCILLARY_DATA_FLAGS[0]);
        this.push_raw_10bit_word(ANCILLARY_DATA_FLAGS[1]);
        this.push_raw_10bit_word(ANCILLARY_DATA_FLAGS[2]);

        // did / sdid: not part of data count
        let did_sdid: [u8; 2] = did16.to_be_bytes();
        this.push_as_10bit_word(did_sdid[0]);
        this.push_as_10bit_word(did_sdid[1]);

        // Reserved for data count
        this.push_raw_10bit_word(0x000);

        this
    }

    /// Pushes the provided `word` as a 10 bits value.
    ///
    /// The 10bits lsb are pushed at current offset as is.
    fn push_raw_10bit_word(&mut self, word: u16) {
        debug_assert_eq!(word & 0xfc00, 0);
        let word = word & 0x3ff;

        match self.offset {
            0 => {
                self.data.push((word >> 2) as u8);
                self.data.push((word << 6) as u8);
                self.offset = 2;
            }
            2 => {
                *self.data.last_mut().unwrap() |= (word >> 4) as u8;
                self.data.push((word << 4) as u8);
                self.offset = 4;
            }
            4 => {
                *self.data.last_mut().unwrap() |= (word >> 6) as u8;
                self.data.push((word << 2) as u8);
                self.offset = 6;
            }
            6 => {
                *self.data.last_mut().unwrap() |= (word >> 8) as u8;
                self.data.push(word as u8);
                self.offset = 0;
            }
            _ => unreachable!(),
        }
    }

    /// Pushes the provided `value` as a 10 bits value.
    ///
    /// The `value` is:
    ///
    /// - prepended with the parity bits,
    /// - pushed at current buffer offset,
    /// - pushed to the checksum.
    fn push_as_10bit_word(&mut self, value: u8) {
        let pval = with_afd_parity(value);
        self.push_raw_10bit_word(pval);
        self.checksum += pval;
    }

    /// Pushes the provided each item in `data` as a 10 bits value.
    ///
    /// The `value` is:
    ///
    /// - prepended with the parity bits,
    /// - pushed at current buffer offset,
    /// - pushed to the checksum.
    ///
    /// The data count is incremented for each pushed value.
    /// If the expected data count is defined and data count exceeds it,
    /// `VideoAncillaryAFDError::UnexpectedDataCount` is returned.
    pub fn push_data(&mut self, data: &[u8]) -> Result<()> {
        for val in data {
            self.data_count += 1;
            if let Some(expected_data_count) = self.expected_data_count {
                if self.data_count > expected_data_count {
                    bail!(VideoAncillaryAFDError::UnexpectedDataCount {
                        found: self.data_count,
                        expected: expected_data_count,
                    });
                }
            }

            self.push_as_10bit_word(*val);
        }

        Ok(())
    }

    /// Terminates and returns the Video Ancillary AFD buffer.
    pub fn terminate(mut self) -> VideoAncillaryAFD {
        // update data_count starting at idx 6, offset 2
        let data_count = with_afd_parity(self.data_count);
        self.data[6] |= (data_count >> 4) as u8;
        self.data[7] |= (data_count << 4) as u8;

        self.checksum = (self.checksum + data_count) & 0x1ff;
        self.checksum |= (!(self.checksum >> 8)) << 9;
        self.checksum &= 0x3ff;

        self.push_raw_10bit_word(self.checksum);

        self.data
    }
}

#[derive(Debug)]
/// Video Ancillary Active Format Description (AFD) Parser
pub struct VideoAncillaryAFDParser<'a> {
    input: &'a [u8],
    data: VideoAncillaryAFD,
    did: u16,
    idx: usize,
    offset: u8,
    checksum: u16,
    data_count: u8,
}

impl<'a> VideoAncillaryAFDParser<'a> {
    pub fn parse_for_cea608(input: &'a [u8]) -> Result<VideoAncillaryAFD> {
        let this = Self::parse(input)?;

        if this.did != EIA_608_ANCILLARY_DID_16 {
            bail!(VideoAncillaryAFDError::UnexpectedDID {
                found: this.did,
                expected: EIA_608_ANCILLARY_DID_16,
            });
        }

        if this.data_count != 3 {
            bail!(VideoAncillaryAFDError::UnexpectedDataCount {
                found: this.data_count,
                expected: 3,
            });
        }

        Ok(this.data)
    }

    pub fn parse_for_cea708(input: &'a [u8]) -> Result<VideoAncillaryAFD> {
        let this = Self::parse(input)?;

        if this.did != EIA_708_ANCILLARY_DID_16 {
            bail!(VideoAncillaryAFDError::UnexpectedDID {
                found: this.did,
                expected: EIA_708_ANCILLARY_DID_16,
            });
        }

        Ok(this.data)
    }

    fn parse(input: &'a [u8]) -> Result<Self> {
        let mut this = VideoAncillaryAFDParser {
            input,
            data: SmallVec::new(),
            did: 0,
            idx: 0,
            offset: 0,
            checksum: 0,
            data_count: 0,
        };

        let mut anc_data_flags = [0u16; 3];
        anc_data_flags[0] = this
            .pull_raw_10bit_word()
            .context("Parsing anc data flags")?;
        anc_data_flags[1] = this
            .pull_raw_10bit_word()
            .context("Parsing anc data flags")?;
        anc_data_flags[2] = this
            .pull_raw_10bit_word()
            .context("Parsing anc data flags")?;

        if anc_data_flags != ANCILLARY_DATA_FLAGS {
            bail!(VideoAncillaryAFDError::UnexpectedDataFlags);
        }

        let did = this.pull_from_10bit_word().context("Parsing did")?;
        let sdid = this.pull_from_10bit_word().context("Parsing sdid")?;
        this.did = u16::from_be_bytes([did, sdid]);

        let data_count = this.pull_from_10bit_word().context("Parsing data_count")?;

        for _ in 0..data_count {
            let val = this.pull_from_10bit_word().context("Parsing data")?;
            this.data.push(val);
        }

        this.data_count = data_count;

        let found_checksum = this.pull_raw_10bit_word().context("Parsing checksum")?;

        this.checksum &= 0x1ff;
        this.checksum |= (!(this.checksum >> 8)) << 9;
        this.checksum &= 0x3ff;

        if this.checksum != found_checksum {
            bail!(VideoAncillaryAFDError::WrongChecksum {
                found: found_checksum,
                expected: this.checksum
            });
        }

        Ok(this)
    }

    fn pull_raw_10bit_word(&mut self) -> Result<u16> {
        if self.input.len() <= self.idx + 1 {
            bail!(VideoAncillaryAFDError::NotEnoughData);
        }

        let word;
        let msb = self.input[self.idx] as u16;
        self.idx += 1;
        let lsb = self.input[self.idx] as u16;

        match self.offset {
            0 => {
                word = (msb << 2) | (lsb >> 6);
                self.offset = 2;
            }
            2 => {
                word = ((msb & 0x3f) << 4) | (lsb >> 4);
                self.offset = 4;
            }
            4 => {
                word = ((msb & 0x0f) << 6) | (lsb >> 2);
                self.offset = 6;
            }
            6 => {
                word = ((msb & 0x03) << 8) | lsb;
                self.idx += 1;
                self.offset = 0;
            }
            _ => unreachable!(),
        }

        Ok(word)
    }

    /// Pulls a 8bit value from next 10bit word.
    ///
    /// Also checks parity and adds to checksum.
    fn pull_from_10bit_word(&mut self) -> Result<u8> {
        let word = self.pull_raw_10bit_word()?;
        let val = (word & 0xff) as u8;

        // Don't check parity: we will rely on the checksum for integrity

        self.checksum += word;

        Ok(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn afd_encode_cea608_raw() {
        let mut anc_afd = VideoAncillaryAFDEncoder::for_cea608_raw(21);
        anc_afd.push_data(&[0x94, 0x2c]).unwrap();
        let buf = anc_afd.terminate();
        assert_eq!(
            buf.as_slice(),
            [0x00, 0x3f, 0xff, 0xfd, 0x61, 0x40, 0xa0, 0x34, 0x55, 0x94, 0x4b, 0x23, 0xb0]
        );
    }

    #[test]
    fn afd_encode_cea608_s334_1a() {
        let mut anc_afd = VideoAncillaryAFDEncoder::for_cea608_s334_1a();
        anc_afd.push_data(&[0x80, 0x94, 0x2c]).unwrap();
        let buf = anc_afd.terminate();
        assert_eq!(
            buf.as_slice(),
            [0x00, 0x3f, 0xff, 0xfd, 0x61, 0x40, 0xa0, 0x36, 0x01, 0x94, 0x4b, 0x2a, 0x60]
        );
    }

    #[test]
    fn afd_encode_cea608_s334_1a_data_count_exceeded() {
        let mut anc_afd = VideoAncillaryAFDEncoder::for_cea608_s334_1a();
        assert_eq!(
            anc_afd
                .push_data(&[0x80, 0x94, 0x2c, 0xab])
                .unwrap_err()
                .downcast::<VideoAncillaryAFDError>()
                .unwrap(),
            VideoAncillaryAFDError::UnexpectedDataCount {
                expected: 3,
                found: 4
            },
        );
    }

    #[test]
    fn afd_encode_cea708_cdp() {
        let mut anc_afd = VideoAncillaryAFDEncoder::for_cea708_cdp();
        anc_afd
            .push_data(&[
                0x96, 0x69, 0x55, 0x3f, 0x43, 0x00, 0x00, 0x72, 0xf8, 0xfc, 0x94, 0x2c, 0xf9, 0x00,
                0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00,
                0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0x74, 0x00, 0x00,
                0x1b,
            ])
            .unwrap();
        let buf = anc_afd.terminate();
        assert_eq!(
            buf.as_slice(),
            [
                0x00, 0x3f, 0xff, 0xfd, 0x61, 0x40, 0x65, 0x5a, 0x5a, 0x69, 0x95, 0x63, 0xf5, 0x0e,
                0x00, 0x80, 0x27, 0x27, 0xe2, 0xfc, 0x65, 0x12, 0xcb, 0xe6, 0x00, 0x80, 0x2f, 0xa8,
                0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b, 0xea, 0x00, 0x80, 0x2f,
                0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b, 0xea, 0x00, 0x80,
                0x2f, 0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b, 0xea, 0x00,
                0x80, 0x2f, 0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b, 0xea,
                0x00, 0x80, 0x2f, 0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b,
                0xea, 0x00, 0x80, 0x2f, 0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0x74, 0x80, 0x20,
                0x08, 0x6e, 0xb7,
            ]
        );
    }

    #[test]
    fn parse_afd_cea608() {
        let buf = VideoAncillaryAFDParser::parse_for_cea608(&[
            0x00, 0x3f, 0xff, 0xfd, 0x61, 0x40, 0xa0, 0x34, 0x55, 0x94, 0x4b, 0x23, 0xb0,
        ])
        .unwrap();

        assert_eq!(buf.as_slice(), [0x15, 0x94, 0x2c]);
    }

    #[test]
    fn parse_afd_cea608_32bit_padded() {
        let buf = VideoAncillaryAFDParser::parse_for_cea608(&[
            0x00, 0x3f, 0xff, 0xfd, 0x61, 0x40, 0xa0, 0x34, 0x55, 0x94, 0x4b, 0x23, 0xb0, 0x00,
            0x00, 0x00,
        ])
        .unwrap();

        assert_eq!(buf.as_slice(), [0x15, 0x94, 0x2c]);
    }

    #[test]
    fn parse_afd_cea708() {
        let buf = VideoAncillaryAFDParser::parse_for_cea708(&[
            0x00, 0x3f, 0xff, 0xfd, 0x61, 0x40, 0x65, 0x5a, 0x5a, 0x69, 0x95, 0x63, 0xf5, 0x0e,
            0x00, 0x80, 0x27, 0x27, 0xe2, 0xfc, 0x65, 0x12, 0xcb, 0xe6, 0x00, 0x80, 0x2f, 0xa8,
            0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b, 0xea, 0x00, 0x80, 0x2f,
            0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b, 0xea, 0x00, 0x80,
            0x2f, 0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b, 0xea, 0x00,
            0x80, 0x2f, 0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b, 0xea,
            0x00, 0x80, 0x2f, 0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0xfa, 0x80, 0x20, 0x0b,
            0xea, 0x00, 0x80, 0x2f, 0xa8, 0x02, 0x00, 0xbe, 0xa0, 0x08, 0x02, 0x74, 0x80, 0x20,
            0x08, 0x6e, 0xb7,
        ])
        .unwrap();

        assert_eq!(
            buf.as_slice(),
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
    }

    #[test]
    fn parse_afd_cea608_not_enough_data() {
        assert_eq!(
            VideoAncillaryAFDParser::parse_for_cea608(&[0x00, 0x3f])
                .unwrap_err()
                .downcast::<VideoAncillaryAFDError>()
                .unwrap(),
            VideoAncillaryAFDError::NotEnoughData,
        );
    }

    #[test]
    fn parse_afd_cea608_unexpected_data_flags() {
        assert_eq!(
            VideoAncillaryAFDParser::parse_for_cea608(&[
                0x00, 0x3f, 0xff, 0xdd, 0x61, 0x40, 0x60, 0x09, 0x88
            ])
            .unwrap_err()
            .downcast::<VideoAncillaryAFDError>()
            .unwrap(),
            VideoAncillaryAFDError::UnexpectedDataFlags,
        );
    }

    #[test]
    fn parse_afd_cea608_unexpected_did() {
        assert_eq!(
            VideoAncillaryAFDParser::parse_for_cea608(&[
                0x00, 0x3f, 0xff, 0xfd, 0x61, 0x40, 0x60, 0x09, 0x88
            ])
            .unwrap_err()
            .downcast::<VideoAncillaryAFDError>()
            .unwrap(),
            VideoAncillaryAFDError::UnexpectedDID {
                found: EIA_708_ANCILLARY_DID_16,
                expected: EIA_608_ANCILLARY_DID_16
            },
        );
    }

    #[test]
    fn parse_afd_cea708_wrong_checksum() {
        assert_eq!(
            VideoAncillaryAFDParser::parse_for_cea708(&[
                0x00, 0x3f, 0xff, 0xfd, 0x61, 0x40, 0x60, 0x09, 0x81
            ])
            .unwrap_err()
            .downcast::<VideoAncillaryAFDError>()
            .unwrap(),
            VideoAncillaryAFDError::WrongChecksum {
                found: 0x260,
                expected: 0x262
            },
        );
    }
}
