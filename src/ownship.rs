//! Which object in the exporter's stream is the pilot's own aircraft, and the
//! static properties it has accumulated (restated in every segment, spec §5).
//!
//! Three signals, in order (spec §6):
//! 1. flight-data properties only ever present on the local player's
//!    aircraft (source A: Tacview Advanced's exporter);
//! 2. a `pilot_name` override from config, matched against `Pilot=`;
//! 3. a stream that only ever shows ONE object (source B: our own Export.lua
//!    hook serving the ownship alone).
//!
//! Once chosen, the id sticks until `-id` or a new header.

pub const FLIGHT_DATA_KEYS: &[&str] = &[
    "AOA",
    "AOS",
    "IAS",
    "CAS",
    "TAS",
    "Mach",
    "AGL",
    "VerticalGForce",
    "LongitudinalGForce",
    "LateralGForce",
    "Throttle",
    "PilotHeadYaw",
    "PilotHeadPitch",
    "PilotHeadRoll",
    "FlapsPosition",
    "Flaps",
    "LandingGear",
    "PitchControlInput",
    "RollControlInput",
    "YawControlInput",
];

pub const STATIC_KEYS: &[&str] = &[
    "Type",
    "Name",
    "Pilot",
    "Group",
    "Coalition",
    "Country",
    "Color",
    "CallSign",
    "Registration",
];

#[derive(Debug, Default)]
pub struct OwnshipTracker {
    pilot_name: Option<String>,
    chosen: Option<String>,
    /// Every object id seen on this connection (for the single-object rule).
    seen: Vec<String>,
    /// The ownship's static properties, newest value per key, in first-seen order.
    statics: Vec<(String, String)>,
}

impl OwnshipTracker {
    pub fn new(pilot_name: Option<String>) -> Self {
        Self {
            pilot_name: pilot_name.filter(|s| !s.trim().is_empty()),
            ..Default::default()
        }
    }

    /// A new connection header: forget the choice and the object set.
    pub fn reset(&mut self) {
        self.chosen = None;
        self.seen.clear();
        self.statics.clear();
    }

    pub fn chosen(&self) -> Option<&str> {
        self.chosen.as_deref()
    }

    pub fn statics(&self) -> &[(String, String)] {
        &self.statics
    }

    pub fn pilot(&self) -> Option<&str> {
        self.statics
            .iter()
            .find(|(k, _)| k == "Pilot")
            .map(|(_, v)| v.as_str())
    }

    pub fn removed(&mut self, id: &str) {
        if self.chosen.as_deref() == Some(id) {
            tracing::info!(id, "ownship object removed");
            self.chosen = None;
            self.statics.clear();
        }
        self.seen.retain(|s| s != id);
    }

    /// Feed one object line. Returns true when the line belongs to the ownship
    /// (chosen now or earlier) and should be written.
    pub fn observe(&mut self, id: &str, props: &[(String, String)]) -> bool {
        if !self.seen.iter().any(|s| s == id) {
            self.seen.push(id.to_string());
        }
        if self.chosen.is_none() {
            let by_flight_data = props
                .iter()
                .any(|(k, _)| FLIGHT_DATA_KEYS.contains(&k.as_str()));
            let by_name = self
                .pilot_name
                .as_deref()
                .is_some_and(|name| props.iter().any(|(k, v)| k == "Pilot" && v == name));
            if by_flight_data || by_name {
                tracing::info!(id, by_flight_data, by_name, "ownship chosen");
                self.chosen = Some(id.to_string());
            }
        }
        if self.chosen.as_deref() == Some(id) {
            self.merge_statics(props);
            return true;
        }
        false
    }

    /// The single-object rule (source B): called at a frame boundary once the
    /// connection has shown at least `min_frames` frames with exactly one
    /// object. The caller decides the threshold.
    pub fn adopt_single_object(&mut self) -> bool {
        if self.chosen.is_none() && self.seen.len() == 1 {
            let id = self.seen[0].clone();
            tracing::info!(id, "ownship chosen: the only object in the stream");
            self.chosen = Some(id);
            return true;
        }
        false
    }

    fn merge_statics(&mut self, props: &[(String, String)]) {
        for (k, v) in props {
            if !STATIC_KEYS.contains(&k.as_str()) {
                continue;
            }
            if let Some(slot) = self.statics.iter_mut().find(|(sk, _)| sk == k) {
                slot.1 = v.clone();
            } else {
                self.statics.push((k.clone(), v.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn flight_data_marks_the_ownship_and_statics_accumulate() {
        let mut t = OwnshipTracker::new(None);
        assert!(!t.observe(
            "b01",
            &p(&[
                ("T", "1|2|3"),
                ("Type", "Air+FixedWing"),
                ("Pilot", "Other")
            ])
        ));
        assert!(t.observe(
            "a02",
            &p(&[
                ("T", "1|2|3"),
                ("Type", "Air+FixedWing"),
                ("Pilot", "Me"),
                ("AOA", "3.1")
            ])
        ));
        assert_eq!(t.chosen(), Some("a02"));
        assert!(t.observe("a02", &p(&[("T", "1|2|4"), ("Name", "F-16C_50")])));
        assert!(
            !t.observe("b01", &p(&[("T", "1|2|3"), ("AOA", "9")])),
            "a later flight-data object does not displace the choice"
        );
        assert_eq!(t.pilot(), Some("Me"));
        assert_eq!(t.statics().len(), 3);
        t.removed("a02");
        assert_eq!(t.chosen(), None);
    }

    #[test]
    fn pilot_name_override_and_single_object_rule() {
        let mut t = OwnshipTracker::new(Some("Airboss".into()));
        assert!(!t.observe("c03", &p(&[("Pilot", "Someone")])));
        assert!(t.observe("c04", &p(&[("Pilot", "Airboss")])));
        assert_eq!(t.chosen(), Some("c04"));

        let mut s = OwnshipTracker::new(None);
        assert!(!s.observe("1", &p(&[("T", "1|2|3")])));
        assert!(s.adopt_single_object());
        assert!(s.observe("1", &p(&[("T", "1|2|4")])));
        let mut two = OwnshipTracker::new(None);
        two.observe("1", &p(&[("T", "1")]));
        two.observe("2", &p(&[("T", "1")]));
        assert!(!two.adopt_single_object());
    }
}
