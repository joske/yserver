use super::*;

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().unwrap())
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

#[test]
fn argb_visual_has_only_argb32_format_association() {
    let root_visual = ResourceId(0x102);
    let argb_visual = ResourceId(0x103);
    let glmark_visual = ResourceId(0x105);
    let mut reply = Vec::new();

    write_render_query_pict_formats_reply(
        &mut reply,
        ClientByteOrder::LittleEndian,
        SequenceNumber(7),
        root_visual,
        argb_visual,
        glmark_visual,
    )
    .unwrap();

    assert_eq!(reply.len(), 32 + 188);
    assert_eq!(le_u32(&reply[4..8]), 47);
    assert_eq!(le_u32(&reply[8..12]), 5);
    assert_eq!(le_u32(&reply[12..16]), 1);
    assert_eq!(le_u32(&reply[16..20]), 2);
    assert_eq!(le_u32(&reply[20..24]), 3);

    let screen = 32 + 5 * 28;
    assert_eq!(le_u32(&reply[screen..screen + 4]), 2);
    assert_eq!(le_u32(&reply[screen + 4..screen + 8]), RENDER_FMT_RGB24);

    let depth24 = screen + 8;
    assert_eq!(reply[depth24], 24);
    assert_eq!(le_u16(&reply[depth24 + 2..depth24 + 4]), 2);
    assert_eq!(le_u32(&reply[depth24 + 8..depth24 + 12]), root_visual.0);
    assert_eq!(le_u32(&reply[depth24 + 12..depth24 + 16]), RENDER_FMT_RGB24);
    assert_eq!(le_u32(&reply[depth24 + 16..depth24 + 20]), glmark_visual.0);
    assert_eq!(le_u32(&reply[depth24 + 20..depth24 + 24]), RENDER_FMT_RGB24);

    let depth32 = depth24 + 24;
    assert_eq!(reply[depth32], 32);
    assert_eq!(le_u16(&reply[depth32 + 2..depth32 + 4]), 1);
    assert_eq!(le_u32(&reply[depth32 + 8..depth32 + 12]), argb_visual.0);
    assert_eq!(
        le_u32(&reply[depth32 + 12..depth32 + 16]),
        RENDER_FMT_ARGB32
    );
    assert_eq!(depth32 + 16, reply.len());
}
