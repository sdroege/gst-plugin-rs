/// Computes the seqnum distance
///
/// This makes sense if both seqnums are in the same cycle.
pub(crate) fn seqnum_distance(seqnum1: u16, seqnum2: u16) -> i16 {
    // See http://en.wikipedia.org/wiki/Serial_number_arithmetic

    let seqnum1 = i16::from_ne_bytes(seqnum1.to_ne_bytes());
    let seqnum2 = i16::from_ne_bytes(seqnum2.to_ne_bytes());

    seqnum1.wrapping_sub(seqnum2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_seqnums() {
        assert_eq!(seqnum_distance(0, 1), -1);
        assert_eq!(seqnum_distance(1, 1), 0);
        assert_eq!(seqnum_distance(1, 0), 1);

        assert_eq!(seqnum_distance(0x7fff, 0), 0x7fff);
        assert_eq!(seqnum_distance(0xffff, 0), -1);

        assert_eq!(seqnum_distance(0, 0x7fff), -0x7fff);
        assert_eq!(seqnum_distance(0, 0xffff), 1);

        // This is the limit of the algorithm:
        assert_eq!(seqnum_distance(0x8000, 0), -0x8000);
        assert_eq!(seqnum_distance(0, 0x8000), -0x8000);
    }
}
