//! No executable paths, arbitrary flags, strings or variable allocations cross this boundary.
pub const FRAME_SIZE: usize = 24;
pub const MAGIC: u32 = 0x31534341; // ACS1
/// Effective helper thread priority after MMCSS admission, as an i32 LE in the
/// fixed reply. The backend and helper must use the same local protocol build.
pub const REPLY_PRIORITY_OFFSET: usize = 88;
/// Replies include the fixed payload and request identity.
pub const REPLY_SIZE: usize = 100;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Relative pointer motion in the native injection owner.
    MoveRelative {
        dx: i32,
        dy: i32,
    },
    Ping,
    Release,
    Position,
    Move {
        x: i32,
        y: i32,
    },
    Button {
        button: u16,
        down: bool,
    },
    Wheel {
        amount: i32,
        horizontal: bool,
    },
    Key {
        code: u16,
        scan: bool,
        extended: bool,
        down: bool,
    },
    /// One absolute-positioned left Down, never two independently dispatched events.
    DragPress {
        x: i32,
        y: i32,
    },
    /// Lease only: the pipe owner handles it without desktop/query work or a reply.
    Heartbeat,
}
impl Command {
    pub fn encode(self) -> [u8; FRAME_SIZE] {
        let (kind, a, b, c) = match self {
            Self::MoveRelative { dx, dy } => (7, dx, dy, 0),
            Self::DragPress { x, y } => (8, x, y, 0),
            Self::Heartbeat => (9, 0, 0, 0),
            Self::Ping => (0, 0, 0, 0),
            Self::Release => (1, 0, 0, 0),
            Self::Position => (2, 0, 0, 0),
            Self::Move { x, y } => (3, x, y, 0),
            Self::Button { button, down } => (4, i32::from(button), i32::from(down), 0),
            Self::Wheel { amount, horizontal } => (5, amount, i32::from(horizontal), 0),
            Self::Key {
                code,
                scan,
                extended,
                down,
            } => (
                6,
                i32::from(code),
                i32::from(scan) | (i32::from(extended) << 1),
                i32::from(down),
            ),
        };
        let mut bytes = [0; FRAME_SIZE];
        for (i, v) in [MAGIC, kind, a as u32, b as u32, c as u32, 0].iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != FRAME_SIZE {
            return None;
        }
        let mut v = [0u32; 6];
        for (i, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
            v[i] = u32::from_le_bytes(*chunk);
        }
        if v[0] != MAGIC || v[5] != 0 {
            return None;
        }
        let (a, b, c) = (v[2], v[3], v[4]);
        Some(match v[1] {
            0 if a | b | c == 0 => Self::Ping,
            1 if a | b | c == 0 => Self::Release,
            2 if a | b | c == 0 => Self::Position,
            9 if a | b | c == 0 => Self::Heartbeat,
            3 if a <= 65535 && b <= 65535 && c == 0 => Self::Move {
                x: a as i32,
                y: b as i32,
            },
            8 if a <= 65535 && b <= 65535 && c == 0 => Self::DragPress {
                x: a as i32,
                y: b as i32,
            },
            4 if a < 5 && b <= 1 && c == 0 => Self::Button {
                button: a as u16,
                down: b == 1,
            },
            5 if (a as i32).unsigned_abs() <= 120000 && b <= 1 && c == 0 => Self::Wheel {
                amount: a as i32,
                horizontal: b == 1,
            },
            6 if (1..=255).contains(&a) && b <= 3 && c <= 1 => Self::Key {
                code: a as u16,
                scan: b & 1 != 0,
                extended: b & 2 != 0,
                down: c == 1,
            },
            7 if (a as i32).unsigned_abs() <= 1_000_000 && (b as i32).unsigned_abs() <= 1_000_000 && c == 0 => {
                Self::MoveRelative {
                    dx: a as i32,
                    dy: b as i32,
                }
            }
            _ => return None,
        })
    }
}

/// One authenticated connection carries two lanes. Only read-only queries may
/// use the ordinary lane. Input and its native query/release barriers stay FIFO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: u32,
    pub ordinary: bool,
    pub command: Command,
}
impl Request {
    pub fn encode(self) -> [u8; FRAME_SIZE] {
        assert!(self.id < 1 << 31);
        let mut bytes = self.command.encode();
        bytes[20..24].copy_from_slice(&(self.id | (u32::from(self.ordinary) << 31)).to_le_bytes());
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut bytes: [u8; FRAME_SIZE] = bytes.try_into().ok()?;
        let meta = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
        bytes[20..24].fill(0);
        let command = Command::decode(&bytes)?;
        let ordinary = meta >> 31 != 0;
        let id = meta & 0x7fff_ffff;
        if (id == 0) != (command == Command::Heartbeat)
            || ordinary && !matches!(command, Command::Ping | Command::Position)
        {
            return None;
        }
        Some(Self { id, ordinary, command })
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Reply(pub [u8; REPLY_SIZE]);
impl Reply {
    pub fn id(self) -> u32 {
        u32::from_le_bytes(self.0[96..100].try_into().expect("fixed reply"))
    }
    pub fn with_id(mut self, id: u32) -> Self {
        self.0[96..100].copy_from_slice(&id.to_le_bytes());
        self
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn correlated_lanes_keep_payload_and_reject_ordinary_mutations() {
        for ordinary in [false, true] {
            let request = Request {
                id: 0x7fff_ffff,
                ordinary,
                command: Command::Position,
            };
            assert_eq!(Request::decode(&request.encode()), Some(request));
            assert_eq!(request.encode()[..20], Command::Position.encode()[..20]);
        }
        let input = Request {
            id: 7,
            ordinary: false,
            command: Command::DragPress { x: 12, y: 34 },
        };
        assert_eq!(Request::decode(&input.encode()), Some(input));
        assert!(
            Request::decode(
                &Request {
                    ordinary: true,
                    ..input
                }
                .encode()
            )
            .is_none()
        );
        assert!(Request::decode(&Request { id: 0, ..input }.encode()).is_none());
        let heartbeat = Request {
            id: 0,
            ordinary: false,
            command: Command::Heartbeat,
        };
        assert_eq!(Request::decode(&heartbeat.encode()), Some(heartbeat));
        assert!(Request::decode(&Request { id: 1, ..heartbeat }.encode()).is_none());
        assert!(
            Request::decode(
                &Request {
                    ordinary: true,
                    ..heartbeat
                }
                .encode()
            )
            .is_none()
        );
        let reply = Reply([42; REPLY_SIZE]).with_id(input.id);
        assert_eq!(reply.id(), input.id);
        assert_eq!(reply.0[..96], [42; 96]);
    }
    #[test]
    fn positioned_drag_press_is_one_bounded_command() {
        for cmd in [
            Command::DragPress { x: 0, y: 65535 },
            Command::DragPress { x: 123, y: 456 },
        ] {
            assert_eq!(Command::decode(&cmd.encode()), Some(cmd));
        }
        assert_eq!(Command::decode(&Command::DragPress { x: -1, y: 0 }.encode()), None);
        assert_eq!(Command::decode(&Command::DragPress { x: 0, y: 65536 }.encode()), None);
    }
    #[test]
    fn relative_motion_is_distinct_bounded_and_preserves_signed_counts() {
        let cmd = Command::MoveRelative { dx: -123, dy: 456 };
        let frame = cmd.encode();
        assert_eq!(u32::from_le_bytes(frame[4..8].try_into().unwrap()), 7);
        assert_eq!(Command::decode(&frame), Some(cmd));
        assert!(Command::decode(&Command::MoveRelative { dx: i32::MIN, dy: 0 }.encode()).is_none());
        assert_eq!(
            u32::from_le_bytes(Command::Move { x: 1, y: 2 }.encode()[4..8].try_into().unwrap()),
            3
        );
    }

    #[test]
    fn rejects_malformed_and_unbounded_input() {
        let valid = Command::Move { x: 65535, y: 0 }.encode();
        assert_eq!(Command::decode(&valid), Some(Command::Move { x: 65535, y: 0 }));
        assert!(Command::decode(&valid[..23]).is_none());
        for cmd in [
            Command::Move { x: -1, y: 0 },
            Command::Button { button: 5, down: true },
            Command::Wheel {
                amount: i32::MIN,
                horizontal: false,
            },
            Command::Key {
                code: 256,
                scan: true,
                extended: false,
                down: true,
            },
        ] {
            assert!(Command::decode(&cmd.encode()).is_none());
        }
        for index in [0, 4, 20] {
            let mut bad = valid;
            bad[index] = 255;
            assert!(Command::decode(&bad).is_none());
        }
    }
}

/// Fixed diagnostic categories only: never carries key values or arbitrary strings.
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum ErrorStage {
    Peer = 1,
    Liveness = 2,
    Decode = 3,
    Desktop = 4,
    Inject = 5,
    Cursor = 6,
    Release = 7,
}
pub fn error_stage_name(stage: u32) -> &'static str {
    match stage {
        1 => "peer authentication",
        2 => "client liveness",
        3 => "frame validation",
        4 => "input desktop attachment/cleanup",
        5 => "SendInput",
        6 => "GetCursorPos",
        7 => "held-input release",
        _ => "unknown stage",
    }
}
