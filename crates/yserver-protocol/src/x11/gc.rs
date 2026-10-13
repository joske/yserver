//! GC component values (CreateGC / ChangeGC) with their wire encoding.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineStyle {
    Solid,
    OnOffDash,
    DoubleDash,
}

impl LineStyle {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            1 => Self::OnOffDash,
            2 => Self::DoubleDash,
            _ => Self::Solid,
        }
    }

    pub fn protocol_value(self) -> u8 {
        match self {
            Self::Solid => 0,
            Self::OnOffDash => 1,
            Self::DoubleDash => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapStyle {
    NotLast,
    Butt,
    Round,
    Projecting,
}

impl CapStyle {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            0 => Self::NotLast,
            2 => Self::Round,
            3 => Self::Projecting,
            _ => Self::Butt,
        }
    }

    pub fn protocol_value(self) -> u8 {
        match self {
            Self::NotLast => 0,
            Self::Butt => 1,
            Self::Round => 2,
            Self::Projecting => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinStyle {
    Miter,
    Round,
    Bevel,
}

impl JoinStyle {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            1 => Self::Round,
            2 => Self::Bevel,
            _ => Self::Miter,
        }
    }

    pub fn protocol_value(self) -> u8 {
        match self {
            Self::Miter => 0,
            Self::Round => 1,
            Self::Bevel => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcFunction {
    Clear,
    And,
    AndReverse,
    Copy,
    AndInverted,
    NoOp,
    Xor,
    Or,
    Nor,
    Equiv,
    Invert,
    OrReverse,
    CopyInverted,
    OrInverted,
    Nand,
    Set,
}

impl GcFunction {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            0 => Self::Clear,
            1 => Self::And,
            2 => Self::AndReverse,
            4 => Self::AndInverted,
            5 => Self::NoOp,
            6 => Self::Xor,
            7 => Self::Or,
            8 => Self::Nor,
            9 => Self::Equiv,
            10 => Self::Invert,
            11 => Self::OrReverse,
            12 => Self::CopyInverted,
            13 => Self::OrInverted,
            14 => Self::Nand,
            15 => Self::Set,
            _ => Self::Copy,
        }
    }

    pub fn protocol_value(self) -> u8 {
        match self {
            Self::Clear => 0,
            Self::And => 1,
            Self::AndReverse => 2,
            Self::Copy => 3,
            Self::AndInverted => 4,
            Self::NoOp => 5,
            Self::Xor => 6,
            Self::Or => 7,
            Self::Nor => 8,
            Self::Equiv => 9,
            Self::Invert => 10,
            Self::OrReverse => 11,
            Self::CopyInverted => 12,
            Self::OrInverted => 13,
            Self::Nand => 14,
            Self::Set => 15,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArcMode {
    Chord,
    PieSlice,
}

impl ArcMode {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            0 => Self::Chord,
            _ => Self::PieSlice,
        }
    }

    pub fn protocol_value(self) -> u8 {
        match self {
            Self::Chord => 0,
            Self::PieSlice => 1,
        }
    }
}
