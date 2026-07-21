use crate::command::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    EditMessage(&'static str),
}

pub fn dispatch(command: &Command) -> Option<Action> {
    match command.name.as_str() {
        "ping" => Some(Action::EditMessage("🏓 Pong!")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, dispatch};
    use crate::command::Command;

    #[test]
    fn dispatches_ping_to_an_edit() {
        let command = Command {
            name: "ping".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), Some(Action::EditMessage("🏓 Pong!")));
    }

    #[test]
    fn ignores_unknown_commands() {
        let command = Command {
            name: "unknown".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), None);
    }
}
