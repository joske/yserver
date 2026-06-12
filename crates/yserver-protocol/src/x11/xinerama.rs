//! XINERAMA (PanoramiX) extension wire encoders. yserver mirrors the
//! RANDR-backed Xinerama path (Xorg `randr/rrxinerama.c`): the Xinerama
//! screens are the RANDR monitors. All replies use the 32-byte fixed reply;
//! only `QueryScreens` appends `N * 8` bytes.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_i16, write_u16, write_u32},
};

pub const MAJOR_VERSION: u16 = 1;
pub const MINOR_VERSION: u16 = 1;

pub const QUERY_VERSION: u8 = 0;
pub const GET_STATE: u8 = 1;
pub const GET_SCREEN_COUNT: u8 = 2;
pub const GET_SCREEN_SIZE: u8 = 3;
pub const IS_ACTIVE: u8 = 4;
pub const QUERY_SCREENS: u8 = 5;

/// One Xinerama screen rect. Wire: x_org i16, y_org i16, width u16, height u16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenInfo {
    pub x_org: i16,
    pub y_org: i16,
    pub width: u16,
    pub height: u16,
}

trait Put {
    fn put(self, byte_order: ClientByteOrder, out: &mut Vec<u8>);
}

impl Put for u16 {
    fn put(self, byte_order: ClientByteOrder, out: &mut Vec<u8>) {
        write_u16(byte_order, out, self);
    }
}

impl Put for u32 {
    fn put(self, byte_order: ClientByteOrder, out: &mut Vec<u8>) {
        write_u32(byte_order, out, self);
    }
}

impl Put for i16 {
    fn put(self, byte_order: ClientByteOrder, out: &mut Vec<u8>) {
        write_i16(byte_order, out, self);
    }
}

fn put<T: Put>(byte_order: ClientByteOrder, out: &mut Vec<u8>, x: T) {
    x.put(byte_order, out);
}

fn fixed_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    data: u8,
    length: u32,
) -> Vec<u8> {
    let mut reply = Vec::with_capacity(32);
    reply.push(1u8);
    reply.push(data);
    put(byte_order, &mut reply, sequence.0);
    put(byte_order, &mut reply, length);
    reply
}

#[must_use]
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    put(byte_order, &mut reply, MAJOR_VERSION);
    put(byte_order, &mut reply, MINOR_VERSION);
    reply.resize(32, 0);
    reply
}

#[must_use]
pub fn encode_get_state_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    active: bool,
    window: u32,
) -> Vec<u8> {
    let mut reply = fixed_reply(byte_order, sequence, u8::from(active), 0);
    put(byte_order, &mut reply, window);
    reply.resize(32, 0);
    reply
}

#[must_use]
pub fn encode_get_screen_count_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    count: u8,
    window: u32,
) -> Vec<u8> {
    let mut reply = fixed_reply(byte_order, sequence, count, 0);
    put(byte_order, &mut reply, window);
    reply.resize(32, 0);
    reply
}

#[must_use]
pub fn encode_get_screen_size_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    width: u32,
    height: u32,
    window: u32,
    screen: u32,
) -> Vec<u8> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    put(byte_order, &mut reply, width);
    put(byte_order, &mut reply, height);
    put(byte_order, &mut reply, window);
    put(byte_order, &mut reply, screen);
    reply.resize(32, 0);
    reply
}

#[must_use]
pub fn encode_is_active_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    active: bool,
) -> Vec<u8> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    put(byte_order, &mut reply, u32::from(active));
    reply.resize(32, 0);
    reply
}

#[must_use]
pub fn encode_query_screens_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    screens: &[ScreenInfo],
) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)]
    let length = (screens.len() * 2) as u32;
    let mut reply = fixed_reply(byte_order, sequence, 0, length);
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut reply, screens.len() as u32);
    reply.resize(32, 0);
    for screen in screens {
        put(byte_order, &mut reply, screen.x_org);
        put(byte_order, &mut reply, screen.y_org);
        put(byte_order, &mut reply, screen.width);
        put(byte_order, &mut reply, screen.height);
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    const LE: ClientByteOrder = ClientByteOrder::LittleEndian;

    fn seq() -> SequenceNumber {
        SequenceNumber(0x1234)
    }

    #[test]
    fn query_version_reply_bytes() {
        let reply = encode_query_version_reply(LE, seq());
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(&reply[2..4], &0x1234u16.to_le_bytes());
        assert_eq!(&reply[4..8], &0u32.to_le_bytes());
        assert_eq!(&reply[8..10], &1u16.to_le_bytes());
        assert_eq!(&reply[10..12], &1u16.to_le_bytes());
    }

    #[test]
    fn get_state_reply_has_state_in_byte1() {
        let reply = encode_get_state_reply(LE, seq(), true, 0xdead_beef);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[1], 1);
        assert_eq!(&reply[8..12], &0xdead_beefu32.to_le_bytes());
        let inactive = encode_get_state_reply(LE, seq(), false, 0);
        assert_eq!(inactive[1], 0);
    }

    #[test]
    fn get_screen_count_reply_has_count_in_byte1() {
        let reply = encode_get_screen_count_reply(LE, seq(), 2, 0x55);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[1], 2);
        assert_eq!(&reply[8..12], &0x55u32.to_le_bytes());
    }

    #[test]
    fn get_screen_size_reply_bytes() {
        let reply = encode_get_screen_size_reply(LE, seq(), 5120, 1440, 0x10, 0);
        assert_eq!(reply.len(), 32);
        assert_eq!(&reply[8..12], &5120u32.to_le_bytes());
        assert_eq!(&reply[12..16], &1440u32.to_le_bytes());
        assert_eq!(&reply[16..20], &0x10u32.to_le_bytes());
        assert_eq!(&reply[20..24], &0u32.to_le_bytes());
    }

    #[test]
    fn is_active_reply_state_word() {
        let reply = encode_is_active_reply(LE, seq(), true);
        assert_eq!(reply.len(), 32);
        assert_eq!(&reply[8..12], &1u32.to_le_bytes());
        assert_eq!(
            &encode_is_active_reply(LE, seq(), false)[8..12],
            &0u32.to_le_bytes()
        );
    }

    #[test]
    fn query_screens_two_screens() {
        let screens = [
            ScreenInfo {
                x_org: 0,
                y_org: 0,
                width: 2560,
                height: 1440,
            },
            ScreenInfo {
                x_org: 2560,
                y_org: 0,
                width: 2560,
                height: 1440,
            },
        ];
        let reply = encode_query_screens_reply(LE, seq(), &screens);
        assert_eq!(reply.len(), 48);
        assert_eq!(&reply[4..8], &4u32.to_le_bytes());
        assert_eq!(&reply[8..12], &2u32.to_le_bytes());
        assert_eq!(&reply[32..34], &0i16.to_le_bytes());
        assert_eq!(&reply[34..36], &0i16.to_le_bytes());
        assert_eq!(&reply[36..38], &2560u16.to_le_bytes());
        assert_eq!(&reply[38..40], &1440u16.to_le_bytes());
        assert_eq!(&reply[40..42], &2560i16.to_le_bytes());
        assert_eq!(&reply[42..44], &0i16.to_le_bytes());
        assert_eq!(&reply[44..46], &2560u16.to_le_bytes());
        assert_eq!(&reply[46..48], &1440u16.to_le_bytes());
    }

    #[test]
    fn query_screens_empty() {
        let reply = encode_query_screens_reply(LE, seq(), &[]);
        assert_eq!(reply.len(), 32);
        assert_eq!(&reply[4..8], &0u32.to_le_bytes());
        assert_eq!(&reply[8..12], &0u32.to_le_bytes());
    }
}
