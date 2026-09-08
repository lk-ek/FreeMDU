//! Experimental, read-only T4223C support based on labelled ID498 captures.
//! Motion and post-run interpretations are hypotheses, not physical feedback.
use crate::device::{
    Action, Device, DeviceKind, Error, Interface, Property, PropertyKind, Result, Value, private,
};
use alloc::{boxed::Box, format, string::ToString};
use embedded_io_async::{Read, Write};

/// Confirmed diagnostic read key. Full access is never used by monitoring.
pub const READ_KEY: u16 = 0x2b2c;
/// Repeated HALT acknowledgement in field tests; not a verified write test.
pub const FULL_KEY: u16 = 0x0f2f;

/// Decode only program selector positions observed on the T4223C.
pub fn program(raw: u8) -> &'static str {
    match raw {
        15 => "Ende",
        14 => "Koch/Bunt Schranktrocken+",
        1 => "Koch/Bunt Schranktrocken / Schonen",
        3 => "Koch/Bunt Buegelfeucht",
        2 => "Koch/Bunt Mangelfeucht",
        6 => "Glaetten",
        7 => "Finish Wolle",
        8 => "Pflegeleicht Schranktrocken+",
        9 => "Pflegeleicht Schranktrocken",
        13 => "Pflegeleicht Schranktrocken / Schonen",
        12 => "Pflegeleicht Buegelfeucht",
        4 => "20 min warm",
        5 => "15 min kalt",
        _ => "Unknown",
    }
}
/// Door triplet interpretation; disagreement remains unknown.
pub fn door(raw: [u8; 3]) -> &'static str {
    match raw {
        [0, 0, 0] => "Closed",
        [1, 1, 1] => "Open",
        _ => "Unknown",
    }
}
/// This is not a finished/ready detector: normal completion is not validated.
pub fn running(raw: u8) -> &'static str {
    match raw {
        0xaa => "Running",
        0x55 => "Not running",
        _ => "Unknown",
    }
}

/// Raw direction marker; physical movement is not verified.
pub fn motion(raw: u8) -> &'static str {
    match raw {
        0 => "Marker 0",
        1 => "Marker 1",
        2 => "Marker 2",
        _ => "Unknown",
    }
}
/// Stateless observations deliberately cannot assert that a cycle completed.
pub fn phase(selector: u8, run: u8, motor: u8) -> &'static str {
    match (selector, run, motor) {
        (_, 0xaa, 0..=2) => "Program active",
        (15, 0x55, 0) => "Selector at Ende",
        (_, 0x55, 1..=2) => "Inactive / waiting / interrupted / post-run",
        (_, 0x55, 0) => "Inactive / waiting / interrupted / post-run",
        _ => "Unknown",
    }
}

/// A closed door with an inactive marker does not prove readiness or completion.
pub fn observed_phase(selector: u8, run: u8, motor: u8, triplet: [u8; 3]) -> &'static str {
    match (door(triplet), run) {
        ("Open", 0x55) => "Door open / inactive",
        ("Closed", _) => phase(selector, run, motor),
        _ => "Unknown",
    }
}

/// HA identifiers are namespaced so ID410 identifiers remain unchanged.
pub const PROPERTIES: &[Property] = &[
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_motion",
        name: "Dryer direction marker (raw)",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_phase",
        name: "Dryer observed phase (experimental)",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_post_run_raw",
        name: "Dryer marker 0260 raw",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_motion_raw",
        name: "Dryer motor marker 027e raw",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_transition_raw",
        name: "Dryer transition marker 027f raw",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_program",
        name: "Dryer selected program",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_door",
        name: "Dryer door",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_run_state",
        name: "Dryer run state (experimental)",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_program_raw",
        name: "Dryer program raw",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_door_raw",
        name: "Dryer door triplet raw",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_run_raw",
        name: "Dryer run marker raw",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_flags_raw",
        name: "Dryer flags 026a/027e raw",
        unit: None,
    },
    Property {
        kind: PropertyKind::Operation,
        id: "dryer_software_id",
        name: "Dryer software ID",
        unit: None,
    },
];

/// Three 16-byte reads form one short observation; not an atomic MCU snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Block at 0x00b0; selector at offset 6.
    pub program: [u8; 16],
    /// Block at 0x0260; door triplet at offsets 5..8.
    pub door: [u8; 16],
    /// Block at 0x0270; running marker at offset 0.
    pub run: [u8; 16],
}
impl Snapshot {
    /// Read only addresses already validated by labelled dumps.
    pub async fn read<P: Read + Write>(intf: &mut Interface<P>) -> Result<Self, P::Error> {
        Ok(Self {
            program: intf.read_memory(0x00b0).await?,
            door: intf.read_memory(0x0260).await?,
            run: intf.read_memory(0x0270).await?,
        })
    }
    /// Interpret one property, retaining unknown values in the raw entities.
    pub fn value<E>(&self, prop: &Property) -> Result<Value, E> {
        Ok(match prop.id {
            "dryer_motion" => motion(self.run[14]).to_string().into(),
            "dryer_phase" => observed_phase(
                self.program[6], self.run[0], self.run[14],
                [self.door[5], self.door[6], self.door[7]],
            )
                .to_string()
                .into(),
            "dryer_post_run_raw" => format!("0x{:02x}", self.door[0]).into(),
            "dryer_motion_raw" => format!("0x{:02x}", self.run[14]).into(),
            "dryer_transition_raw" => format!("0x{:02x}", self.run[15]).into(),

            "dryer_program" => program(self.program[6]).to_string().into(),
            "dryer_door" => door([self.door[5], self.door[6], self.door[7]])
                .to_string()
                .into(),
            "dryer_run_state" => running(self.run[0]).to_string().into(),
            "dryer_program_raw" => format!("0x{:02x}", self.program[6]).into(),
            "dryer_door_raw" => format!(
                "{:02x} {:02x} {:02x}",
                self.door[5], self.door[6], self.door[7]
            )
            .into(),
            "dryer_run_raw" => format!("0x{:02x}", self.run[0]).into(),
            "dryer_flags_raw" => format!("{:02x} {:02x}", self.door[10], self.run[14]).into(),
            "dryer_software_id" => 498u32.into(),
            _ => return Err(Error::UnknownProperty),
        })
    }
}

/// Read-only tumble dryer profile; ID410 remains a separate device profile.
pub struct TumbleDryer<P: Read + Write> {
    intf: Interface<P>,
}
impl<P: Read + Write> TumbleDryer<P> {
    pub(crate) async fn initialize(mut intf: Interface<P>, _id: u16) -> Result<Self, P::Error> {
        intf.unlock_read_access(READ_KEY).await?;
        Ok(Self { intf })
    }
}
#[async_trait::async_trait(?Send)]
impl<P: Read + Write> Device<P> for TumbleDryer<P> {
    async fn connect(port: P) -> Result<Self, P::Error> {
        let mut intf = Interface::new(port);
        let id = intf.query_software_id().await?;
        if id != 498 {
            return Err(Error::UnknownSoftwareId(id));
        }
        Self::initialize(intf, id).await
    }
    fn interface(&mut self) -> &mut Interface<P> {
        &mut self.intf
    }
    fn software_id(&self) -> u16 {
        498
    }
    fn kind(&self) -> DeviceKind {
        DeviceKind::TumbleDryer
    }
    fn properties(&self) -> &'static [Property] {
        PROPERTIES
    }
    fn actions(&self) -> &'static [Action] {
        &[]
    }
    async fn query_property(&mut self, prop: &Property) -> Result<Value, P::Error> {
        Snapshot::read(&mut self.intf).await?.value(prop)
    }
    async fn trigger_action(&mut self, _: &Action, _: Option<&str>) -> Result<(), P::Error> {
        Err(Error::UnknownAction)
    }
}
impl<P: Read + Write> private::Sealed for TumbleDryer<P> {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labelled_captures_match_decoder() {
        let cases: &[(&[u8], u8, &str, &str)] = &[
            (
                include_bytes!("../../tests/fixtures/id498/0.bin"),
                15,
                "Closed",
                "Not running",
            ),
            (
                include_bytes!("../../tests/fixtures/id498/1.bin"),
                14,
                "Closed",
                "Not running",
            ),
            (
                include_bytes!("../../tests/fixtures/id498/2.bin"),
                8,
                "Closed",
                "Not running",
            ),
            (
                include_bytes!("../../tests/fixtures/id498/3.bin"),
                4,
                "Closed",
                "Not running",
            ),
            (
                include_bytes!("../../tests/fixtures/id498/4.bin"),
                5,
                "Closed",
                "Not running",
            ),
            (
                include_bytes!("../../tests/fixtures/id498/5.bin"),
                5,
                "Open",
                "Not running",
            ),
            (
                include_bytes!("../../tests/fixtures/id498/6.bin"),
                5,
                "Closed",
                "Running",
            ),
            (
                include_bytes!("../../tests/fixtures/id498/7.bin"),
                4,
                "Closed",
                "Running",
            ),
            (
                include_bytes!("../../tests/fixtures/id498/8.bin"),
                15,
                "Closed",
                "Not running",
            ),
        ];
        for (bytes, selector, expected_door, expected_run) in cases {
            assert_eq!(bytes.len(), 0x480);
            let snap = Snapshot {
                program: bytes[0xb0..0xc0].try_into().unwrap(),
                door: bytes[0x260..0x270].try_into().unwrap(),
                run: bytes[0x270..0x280].try_into().unwrap(),
            };
            assert_eq!(snap.program[6], *selector);
            assert_ne!(program(*selector), "Unknown");
            assert_eq!(
                door([snap.door[5], snap.door[6], snap.door[7]]),
                *expected_door
            );
            assert_eq!(running(snap.run[0]), *expected_run);
            for property in PROPERTIES {
                assert!(snap.value::<core::convert::Infallible>(property).is_ok());
            }
        }
    }
    #[test]
    fn unknown_is_never_finished_or_closed() {
        assert_eq!(program(0xff), "Unknown");
        assert_eq!(door([0, 1, 0]), "Unknown");
        assert_eq!(door([2, 2, 2]), "Unknown");
        assert_eq!(running(0), "Unknown");
        assert!(!PROPERTIES.iter().any(|p| p.id.contains("finished")));
    }
    #[test]
    fn validated_interruption_and_selectors() {
        assert_eq!(observed_phase(5, 0x55, 0, [1, 1, 1]), "Door open / inactive");
        assert_eq!(observed_phase(5, 0x55, 0, [0, 0, 0]), "Inactive / waiting / interrupted / post-run");
        assert_eq!(observed_phase(5, 0xaa, 1, [0, 0, 0]), "Program active");
        assert_eq!(observed_phase(5, 0xaa, 1, [1, 1, 1]), "Unknown");
        assert_eq!(observed_phase(5, 0x55, 0, [0, 1, 0]), "Unknown");
        for (raw, label) in [(1, "Koch/Bunt Schranktrocken / Schonen"), (3, "Koch/Bunt Buegelfeucht"), (2, "Koch/Bunt Mangelfeucht"), (6, "Glaetten"), (7, "Finish Wolle")] {
            assert_eq!(program(raw), label);
        }
    }
    #[test]
    fn post_run_motion_does_not_restart_program() {
        assert_eq!(phase(5, 0xaa, 0), "Program active");
        assert_eq!(
            phase(5, 0x55, 1),
            "Inactive / waiting / interrupted / post-run"
        );
        assert_eq!(phase(5, 0x55, 0), "Inactive / waiting / interrupted / post-run");
        assert_eq!(phase(15, 0x55, 0), "Selector at Ende");
        assert_eq!(phase(5, 0, 1), "Unknown");
        assert_eq!(motion(3), "Unknown");
    }
    #[test]
    fn repeated_door_toggle() {
        for (triplet, expected) in [
            ([0, 0, 0], "Closed"),
            ([1, 1, 1], "Open"),
            ([0, 0, 0], "Closed"),
        ] {
            assert_eq!(door(triplet), expected);
            assert_eq!(running(0x55), "Not running");
        }
    }
}
