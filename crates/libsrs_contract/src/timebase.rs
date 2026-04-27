use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timebase {
    pub num: u32,
    pub den: u32,
}

impl Timebase {
    pub const fn new(num: u32, den: u32) -> Self {
        Self { num, den }
    }

    pub const fn milliseconds() -> Self {
        Self { num: 1, den: 1_000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp {
    pub ticks: i64,
    pub timebase: Timebase,
}

impl Timestamp {
    pub const fn new(ticks: i64, timebase: Timebase) -> Self {
        Self { ticks, timebase }
    }
}
