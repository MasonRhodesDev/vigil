use vigil_core::PowerAction;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    User,
    Field,
    Reveal,
    Session,
    Power(PowerAction),
    Cancel(PowerAction),
    Confirm(PowerAction),
}

const ORDER: [Stop; 6] = [
    Stop::User,
    Stop::Field,
    Stop::Reveal,
    Stop::Session,
    Stop::Power(PowerAction::Reboot),
    Stop::Power(PowerAction::Poweroff),
];

impl Stop {
    pub fn name(self) -> &'static str {
        match self {
            Stop::User => "user",
            Stop::Field => "field",
            Stop::Reveal => "reveal",
            Stop::Session => "session",
            Stop::Power(action) => action_name(action),
            Stop::Cancel(_) => "cancel",
            Stop::Confirm(_) => "confirm",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Stop::User => "Arrow keys pick the user. Enter returns.",
            Stop::Field => "Press Enter to continue. Tab for more.",
            Stop::Reveal => "Space or Alt+F8 shows or hides the password.",
            Stop::Session => "Arrow keys pick the session. Enter returns.",
            Stop::Power(PowerAction::Reboot) => "Enter asks before restarting.",
            Stop::Power(PowerAction::Poweroff) => "Enter asks before shutting down.",
            Stop::Cancel(_) | Stop::Confirm(_) => "Tab to choose, Enter to select, Esc to go back.",
        }
    }
}

pub fn action_name(action: PowerAction) -> &'static str {
    match action {
        PowerAction::Reboot => "reboot",
        PowerAction::Poweroff => "poweroff",
    }
}

pub fn parse_action(name: &str) -> Option<PowerAction> {
    match name {
        "reboot" => Some(PowerAction::Reboot),
        "poweroff" => Some(PowerAction::Poweroff),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Picker {
    pub index: usize,
    pub count: usize,
}

impl Picker {
    fn shown(self) -> bool {
        self.count > 1
    }

    fn next(self) -> usize {
        (self.index + 1) % self.count
    }

    fn prev(self) -> usize {
        (self.index + self.count - 1) % self.count
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Card {
    pub users: Picker,
    pub sessions: Picker,
    pub reveal: bool,
    pub power: bool,
    pub confirming: Option<PowerAction>,
}

impl Card {
    fn has(&self, stop: Stop) -> bool {
        if let Some(action) = self.confirming.filter(|_| self.power) {
            return matches!(stop, Stop::Cancel(a) | Stop::Confirm(a) if a == action);
        }
        match stop {
            Stop::User => self.users.shown(),
            Stop::Field => true,
            Stop::Reveal => self.reveal,
            Stop::Session => self.sessions.shown(),
            Stop::Power(_) => self.power,
            Stop::Cancel(_) | Stop::Confirm(_) => false,
        }
    }

    fn stops(&self) -> Vec<Stop> {
        match self.confirming.filter(|_| self.power) {
            Some(action) => vec![Stop::Cancel(action), Stop::Confirm(action)],
            None => ORDER.into_iter().filter(|stop| self.has(*stop)).collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Tab,
    BackTab,
    Enter,
    Space,
    Escape,
    Left,
    Right,
    Up,
    Down,
    RevealShortcut,
    Text,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Forward,
    Consume,
    User(usize),
    Session(usize),
    ToggleReveal,
    Arm(PowerAction),
    Disarm,
    Fire(PowerAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Focus {
    pub stop: Stop,
    pub ring: bool,
}

impl Default for Focus {
    fn default() -> Self {
        Self {
            stop: Stop::Field,
            ring: false,
        }
    }
}

impl Focus {
    pub fn settle(self, card: &Card) -> Focus {
        if card.has(self.stop) {
            return self;
        }
        let stop = match (card.confirming.filter(|_| card.power), self.stop) {
            (Some(action), _) => Stop::Cancel(action),
            (None, Stop::Cancel(action) | Stop::Confirm(action)) if card.power => {
                Stop::Power(action)
            }
            _ => Stop::Field,
        };
        Focus { stop, ..self }
    }

    pub fn property(self) -> &'static str {
        if self.ring || self.stop != Stop::Field {
            self.stop.name()
        } else {
            ""
        }
    }

    pub fn step(self, card: &Card, key: Key) -> (Focus, Effect) {
        let stop = self.settle(card).stop;
        let to = |stop| Focus { stop, ring: true };
        match (stop, key) {
            (_, Key::RevealShortcut) if card.reveal => (to(stop), Effect::ToggleReveal),
            (_, Key::Tab) => (to(cycle(card, stop, 1)), Effect::Consume),
            (_, Key::BackTab) => (to(cycle(card, stop, -1)), Effect::Consume),
            (Stop::Field, _) => (Focus { stop, ..self }, Effect::Forward),
            (Stop::Cancel(action) | Stop::Confirm(action), Key::Escape) => {
                (to(Stop::Power(action)), Effect::Disarm)
            }
            (_, Key::Escape) => (to(Stop::Field), Effect::Consume),
            (Stop::User, Key::Left | Key::Up) => (to(stop), Effect::User(card.users.prev())),
            (Stop::User, Key::Right | Key::Down | Key::Space) => {
                (to(stop), Effect::User(card.users.next()))
            }
            (Stop::Session, Key::Left | Key::Up) => {
                (to(stop), Effect::Session(card.sessions.prev()))
            }
            (Stop::Session, Key::Right | Key::Down | Key::Space) => {
                (to(stop), Effect::Session(card.sessions.next()))
            }
            (Stop::User | Stop::Session, Key::Enter) => (to(Stop::Field), Effect::Consume),
            (Stop::Reveal, Key::Space) => (to(stop), Effect::ToggleReveal),
            (Stop::Reveal, Key::Enter) => (to(Stop::Field), Effect::Forward),
            (Stop::Power(action), Key::Space | Key::Enter) => {
                (to(Stop::Cancel(action)), Effect::Arm(action))
            }
            (Stop::Cancel(action), Key::Space | Key::Enter) => {
                (to(Stop::Power(action)), Effect::Disarm)
            }
            (Stop::Confirm(action), Key::Space | Key::Enter) => {
                (to(Stop::Power(action)), Effect::Fire(action))
            }
            (Stop::Cancel(action) | Stop::Confirm(action), Key::Left) => {
                (to(Stop::Cancel(action)), Effect::Consume)
            }
            (Stop::Cancel(action) | Stop::Confirm(action), Key::Right) => {
                (to(Stop::Confirm(action)), Effect::Consume)
            }
            (Stop::Cancel(_) | Stop::Confirm(_), _) => (to(stop), Effect::Consume),
            (_, Key::Text) => (to(Stop::Field), Effect::Forward),
            _ => (to(stop), Effect::Consume),
        }
    }
}

fn cycle(card: &Card, from: Stop, by: isize) -> Stop {
    let stops = card.stops();
    let Some(at) = stops.iter().position(|stop| *stop == from) else {
        return Stop::Field;
    };
    let len = stops.len() as isize;
    stops[(at as isize + by).rem_euclid(len) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Card {
        Card {
            users: Picker { index: 0, count: 3 },
            sessions: Picker { index: 1, count: 2 },
            reveal: true,
            power: true,
            confirming: None,
        }
    }

    fn walk(card: &Card, key: Key, n: usize) -> Vec<&'static str> {
        let mut focus = Focus::default();
        (0..n)
            .map(|_| {
                focus = focus.step(card, key).0;
                focus.stop.name()
            })
            .collect()
    }

    #[test]
    fn tab_order_is_the_visual_order_and_wraps() {
        assert_eq!(
            walk(&full(), Key::Tab, 6),
            ["reveal", "session", "reboot", "poweroff", "user", "field"]
        );
        assert_eq!(
            walk(&full(), Key::BackTab, 6),
            ["user", "poweroff", "reboot", "session", "reveal", "field"]
        );
    }

    #[test]
    fn controls_the_card_does_not_show_are_skipped() {
        let lock = Card {
            reveal: true,
            ..Card::default()
        };
        assert_eq!(walk(&lock, Key::Tab, 3), ["reveal", "field", "reveal"]);
        assert_eq!(walk(&Card::default(), Key::Tab, 2), ["field", "field"]);
    }

    #[test]
    fn a_focused_control_that_disappears_falls_back_to_the_field() {
        let focus = Focus {
            stop: Stop::Session,
            ring: true,
        };
        let card = Card {
            sessions: Picker { index: 0, count: 1 },
            ..full()
        };
        assert_eq!(focus.settle(&card).stop, Stop::Field);
    }

    #[test]
    fn a_confirm_is_modal_and_never_fires_without_its_own_button() {
        let card = Card {
            confirming: Some(PowerAction::Poweroff),
            ..full()
        };
        let mut focus = Focus::default();
        for key in [
            Key::Text,
            Key::Space,
            Key::Enter,
            Key::Up,
            Key::RevealShortcut,
        ] {
            let (next, effect) = focus.step(&card, key);
            assert!(!matches!(effect, Effect::Fire(_)), "{key:?} fired");
            focus = next;
        }
        assert_eq!(walk(&card, Key::Tab, 3), ["confirm", "cancel", "confirm"]);
    }

    #[test]
    fn a_confirm_starts_on_cancel() {
        let focus = Focus {
            stop: Stop::Power(PowerAction::Reboot),
            ring: true,
        };
        assert_eq!(
            focus.step(&full(), Key::Enter),
            (
                Focus {
                    stop: Stop::Cancel(PowerAction::Reboot),
                    ring: true
                },
                Effect::Arm(PowerAction::Reboot)
            )
        );
    }

    #[test]
    fn the_field_forwards_everything_but_navigation() {
        let focus = Focus::default();
        for key in [
            Key::Space,
            Key::Enter,
            Key::Left,
            Key::Right,
            Key::Up,
            Key::Down,
            Key::Escape,
            Key::Text,
        ] {
            assert_eq!(
                focus.step(&full(), key),
                (focus, Effect::Forward),
                "{key:?}"
            );
        }
        assert_eq!(
            focus.property(),
            "",
            "no ring before the keyboard navigates"
        );
    }
}
