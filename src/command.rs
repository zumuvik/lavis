#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub args: String,
}

pub fn parse(input: &str, prefix: &str) -> Option<Command> {
    if prefix.is_empty() {
        return None;
    }

    let command_text = input.strip_prefix(prefix)?.trim_start();
    let mut parts = command_text.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();

    if name.is_empty() {
        return None;
    }

    let args = parts.next().unwrap_or_default().trim().to_owned();
    Some(Command {
        name: name.to_owned(),
        args,
    })
}

#[cfg(test)]
mod tests {
    use super::{Command, parse};

    #[test]
    fn parses_command_without_arguments() {
        assert_eq!(
            parse(",ping", ","),
            Some(Command {
                name: "ping".to_owned(),
                args: String::new(),
            })
        );
    }

    #[test]
    fn parses_command_arguments() {
        assert_eq!(
            parse(",edit hello world", ","),
            Some(Command {
                name: "edit".to_owned(),
                args: "hello world".to_owned(),
            })
        );
    }

    #[test]
    fn ignores_empty_plain_and_prefix_only_input() {
        assert_eq!(parse("", ","), None);
        assert_eq!(parse("hello world", ","), None);
        assert_eq!(parse(",", ","), None);
        assert_eq!(parse(",   ", ","), None);
        assert_eq!(parse(".ping", ","), None);
    }

    #[test]
    fn supports_configured_prefix_and_repeated_whitespace() {
        assert_eq!(
            parse("!edit    hello   world", "!"),
            Some(Command {
                name: "edit".to_owned(),
                args: "hello   world".to_owned(),
            })
        );
    }

    #[test]
    fn supports_unicode_arguments() {
        assert_eq!(
            parse(",say Привет, 世界", ","),
            Some(Command {
                name: "say".to_owned(),
                args: "Привет, 世界".to_owned(),
            })
        );
    }
}
