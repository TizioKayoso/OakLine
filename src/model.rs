#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Pos {
    pub x: i32,
    pub y: i16,
    pub z: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Rail {
    pub exits: u8,
    /// Direction of the raised exit on an ascending rail; zero for flat rails.
    pub ascending: u8,
    pub marker: bool,
    pub ordinary: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SurfaceCell {
    pub color: [u8; 3],
    pub height: i16,
    pub present: bool,
}

impl Default for SurfaceCell {
    fn default() -> Self {
        Self {
            color: [24, 27, 31],
            height: 0,
            present: false,
        }
    }
}

#[derive(Debug)]
pub struct Graph {
    pub positions: Vec<Pos>,
    pub edges: Vec<Vec<usize>>,
    /// Real track endpoints; omitted connections at the gap limit are excluded.
    pub stations: Vec<usize>,
}

#[derive(Debug)]
pub struct Lane {
    pub path: Vec<usize>,
    pub color: [u8; 3],
}

#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub min_x: i32,
    pub max_x: i32,
    pub min_z: i32,
    pub max_z: i32,
}

impl Bounds {
    pub fn width(self) -> u64 {
        (i64::from(self.max_x) - i64::from(self.min_x) + 1) as u64
    }

    pub fn height(self) -> u64 {
        (i64::from(self.max_z) - i64::from(self.min_z) + 1) as u64
    }
}
