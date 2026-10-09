use super::patch_get_image_reply_header;
use yserver_protocol::x11::{ClientByteOrder, SequenceNumber};

#[test]
fn patch_get_image_reply_header_honors_big_endian() {
    let bytes = vec![0u8; 40];
    let patched = patch_get_image_reply_header(
        bytes,
        ClientByteOrder::BigEndian,
        SequenceNumber(0x0102),
        0xA1B2_C3D4,
    );
    assert_eq!(&patched[2..4], &0x0102u16.to_be_bytes());
    assert_eq!(&patched[4..8], &2u32.to_be_bytes());
    assert_eq!(&patched[8..12], &0xA1B2_C3D4u32.to_be_bytes());
}
