//! Link and organ state shared by the tray model, the menu builder, and the icon renderer.
//!
//! Everything in this module is pure data: no platform calls, no I/O.

/// Link liveness, shown by the menu-bar icon tint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LinkState {
    /// No link configured or requested; the body is disconnected.
    Off,
    /// A link is configured and being established.
    Connecting,
    /// The link is up and authenticated.
    Connected,
}

impl LinkState {
    /// Short lowercase label used in menu rows and logs.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
        }
    }

    /// RGBA tint of the menu-bar disc for this state.
    pub const fn tint(self) -> [u8; 4] {
        match self {
            Self::Off => [0x8E, 0x8E, 0x93, 0xFF],
            Self::Connecting => [0xE5, 0xC0, 0x5A, 0xFF],
            Self::Connected => [0x4E, 0xC9, 0x6B, 0xFF],
        }
    }
}

/// An organ line shown in the tray menu.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum Organ {
    /// Running named commands inside allowed roots.
    Hands,
    /// Microphone capture.
    Ears,
    /// Screen and camera capture.
    Eyes,
    /// Speech out.
    Voice,
    /// Call audio and assistance.
    Calls,
}

/// Every organ the v1 menu lists, in display order.
pub const ORGANS: [Organ; 5] = [
    Organ::Hands,
    Organ::Ears,
    Organ::Eyes,
    Organ::Voice,
    Organ::Calls,
];

impl Organ {
    /// Short lowercase label used in menu rows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hands => "hands",
            Self::Ears => "ears",
            Self::Eyes => "eyes",
            Self::Voice => "voice",
            Self::Calls => "calls",
        }
    }
}

/// Human-control posture for one organ, from local config/policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum OrganMode {
    /// The organ may act on its own.
    On,
    /// The organ asks before each action.
    Ask,
    /// The organ is disabled.
    Off,
}

impl OrganMode {
    /// Short lowercase label used in menu rows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Ask => "ask",
            Self::Off => "off",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_stable() {
        assert_eq!(LinkState::Off.label(), "off");
        assert_eq!(LinkState::Connecting.label(), "connecting");
        assert_eq!(LinkState::Connected.label(), "connected");
        assert_eq!(Organ::Hands.label(), "hands");
        assert_eq!(Organ::Ears.label(), "ears");
        assert_eq!(Organ::Eyes.label(), "eyes");
        assert_eq!(Organ::Voice.label(), "voice");
        assert_eq!(Organ::Calls.label(), "calls");
        assert_eq!(OrganMode::On.label(), "on");
        assert_eq!(OrganMode::Ask.label(), "ask");
        assert_eq!(OrganMode::Off.label(), "off");
    }

    #[test]
    fn tints_differ_per_state_and_are_opaque() {
        let states = [LinkState::Off, LinkState::Connecting, LinkState::Connected];
        let mut tints = states.map(LinkState::tint).to_vec();
        tints.sort_unstable();
        tints.dedup();
        assert_eq!(tints.len(), 3, "state tints must be distinct");
        for state in states {
            assert_eq!(state.tint()[3], 0xFF);
        }
    }
}
