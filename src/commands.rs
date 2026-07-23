use crate::command::Command;
use crate::modules::ModuleId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    Ping,
    Stats,
    Help,
    Fastfetch,
    Alias,
    Prefix,
    Modules,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDefinition {
    pub kind: CommandKind,
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub icon: &'static str,
    pub aliasable: bool,
    pub module: ModuleId,
}

pub struct CommandRegistry([CommandDefinition; 7]);

impl CommandRegistry {
    pub fn iter(&self) -> impl Iterator<Item = &CommandDefinition> {
        self.0.iter()
    }

    pub fn canonical_iter(&self) -> impl Iterator<Item = &CommandDefinition> {
        self.0.iter()
    }
}

pub const COMMANDS: CommandRegistry = CommandRegistry([
    CommandDefinition {
        kind: CommandKind::Help,
        name: "help",
        usage: "help [command]",
        summary: "Show command help",
        description: "Shows the command overview or detailed help for a command, alias, or module.",
        icon: "🛠",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Modules,
        name: "modules",
        usage: "modules",
        summary: "List internal modules",
        description: "Lists the statically registered Lavis modules and their commands.",
        icon: "🧩",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Ping,
        name: "ping",
        usage: "ping",
        summary: "Measure Telegram latency",
        description: "Measures a real Telegram MTProto RPC round-trip over the existing authenticated connection.",
        icon: "🏓",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Prefix,
        name: "prefix",
        usage: "prefix [new-prefix|reset]",
        summary: "Show or change the command prefix",
        description: "Shows the active prefix or persists a new prefix.",
        icon: "⚙️",
        aliasable: false,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Stats,
        name: "stats",
        usage: "stats",
        summary: "Show runtime statistics",
        description: "Shows fresh Telegram RPC latency, Lavis process uptime, host uptime, resident memory, command count, and package version.",
        icon: "📊",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Fastfetch,
        name: "fastfetch",
        usage: "fastfetch [--no-profile] [--logo <...>] [--structure <...>] [--separator <text>]",
        summary: "Показать системную информацию",
        description: "Разрешены только --no-profile, --logo none|Alpine|Arch|Debian|Fedora|FreeBSD|Linux|MacOS|NixOS|OpenBSD|Ubuntu|Windows, --structure из title:separator:os:kernel:uptime:cpu:memory:gpu:packages:shell:terminal:terminalsize и --separator.",
        icon: "🖥",
        aliasable: true,
        module: ModuleId::System,
    },
    CommandDefinition {
        kind: CommandKind::Alias,
        name: "alias",
        usage: "alias [list|add <name> <command> [arguments...]|show <name>|del <name>]",
        summary: "Manage command aliases",
        description: "Manages persistent aliases for canonical commands.",
        icon: "🔗",
        aliasable: false,
        module: ModuleId::Aliases,
    },
]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpRequest {
    Overview,
    Topic(String),
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Ping,
    Stats,
    Help(HelpRequest),
    Fastfetch(String),
    Alias(AliasRequest),
    Prefix(PrefixRequest),
    Modules(ModulesRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasRequest {
    List,
    Add {
        name: String,
        target: String,
        args: Vec<String>,
    },
    Delete {
        name: String,
    },
    Show {
        name: String,
    },
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefixRequest {
    Show,
    Set(String),
    Reset,
    Invalid,
}

impl Action {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Stats => "stats",
            Self::Help(_) => "help",
            Self::Fastfetch(_) => "fastfetch",
            Self::Alias(_) => "alias",
            Self::Prefix(_) => "prefix",
            Self::Modules(_) => "modules",
        }
    }
}

pub fn dispatch(command: &Command) -> Option<Action> {
    let definition = canonical_command(&command.name)?;
    match definition.kind {
        CommandKind::Ping => Some(Action::Ping),
        CommandKind::Stats => Some(Action::Stats),
        CommandKind::Help => Some(Action::Help(parse_help_request(&command.args))),
        CommandKind::Fastfetch => Some(Action::Fastfetch(command.args.clone())),
        CommandKind::Alias => Some(Action::Alias(parse_alias_request(&command.args))),
        CommandKind::Prefix => Some(Action::Prefix(parse_prefix_request(&command.args))),
        CommandKind::Modules => Some(Action::Modules(parse_modules_request(&command.args))),
    }
}

pub fn definition(kind: CommandKind) -> &'static CommandDefinition {
    COMMANDS
        .canonical_iter()
        .find(|command| command.kind == kind)
        .unwrap_or_else(|| unreachable!("all CommandKind values are registered"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModulesRequest {
    Overview,
    Invalid,
}

fn parse_modules_request(args: &str) -> ModulesRequest {
    if args.trim().is_empty() {
        ModulesRequest::Overview
    } else {
        ModulesRequest::Invalid
    }
}

fn parse_prefix_request(args: &str) -> PrefixRequest {
    let value = args.trim();
    if value.is_empty() {
        PrefixRequest::Show
    } else if value == "reset" {
        PrefixRequest::Reset
    } else if value.chars().any(char::is_whitespace) {
        PrefixRequest::Invalid
    } else {
        PrefixRequest::Set(value.to_owned())
    }
}

fn parse_alias_request(args: &str) -> AliasRequest {
    let tokens = match shell_words::split(args) {
        Ok(tokens) => tokens,
        Err(_) => return AliasRequest::Invalid,
    };
    match tokens.as_slice() {
        [] => AliasRequest::List,
        [command] if command == "list" => AliasRequest::List,
        [command, name, target, args @ ..] if command == "add" => AliasRequest::Add {
            name: name.clone(),
            target: target.clone(),
            args: args.to_vec(),
        },
        [command, name] if matches!(command.as_str(), "del" | "delete" | "remove") => {
            AliasRequest::Delete { name: name.clone() }
        }
        [command, name] if command == "show" => AliasRequest::Show { name: name.clone() },
        _ => AliasRequest::Invalid,
    }
}

pub fn canonical_command(name: &str) -> Option<&'static CommandDefinition> {
    COMMANDS
        .canonical_iter()
        .find(|command| command.name == name)
}

fn parse_help_request(args: &str) -> HelpRequest {
    let mut topics = args.split_whitespace();
    let Some(topic) = topics.next() else {
        return HelpRequest::Overview;
    };
    if topics.next().is_some() {
        return HelpRequest::Invalid;
    }
    let normalized = topic.to_ascii_lowercase();
    HelpRequest::Topic(normalized)
}

#[cfg(test)]
mod tests {
    use super::{Action, AliasRequest, COMMANDS, HelpRequest, ModulesRequest, dispatch};
    use crate::command::Command;
    use crate::modules::{
        MODULES, ModuleId, commands_for_module, module_by_name, module_definition,
    };
    use std::collections::HashSet;

    #[test]
    fn dispatches_ping() {
        let command = Command {
            name: "ping".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), Some(Action::Ping));
    }

    #[test]
    fn dispatches_stats() {
        let command = Command {
            name: "stats".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), Some(Action::Stats));
    }

    #[test]
    fn dispatches_help_overview_and_ascii_case_insensitive_topics() {
        let overview = Command {
            name: "help".to_owned(),
            args: String::new(),
        };
        let topic = Command {
            name: "help".to_owned(),
            args: "PING".to_owned(),
        };
        let alias_topic = Command {
            name: "help".to_owned(),
            args: "alias".to_owned(),
        };

        assert_eq!(
            dispatch(&overview),
            Some(Action::Help(HelpRequest::Overview))
        );
        assert_eq!(
            dispatch(&topic),
            Some(Action::Help(HelpRequest::Topic("ping".to_owned())))
        );
        assert_eq!(
            dispatch(&alias_topic),
            Some(Action::Help(HelpRequest::Topic("alias".to_owned())))
        );
        assert_eq!(
            dispatch(&Command {
                name: "help".to_owned(),
                args: "prefix".to_owned(),
            }),
            Some(Action::Help(HelpRequest::Topic("prefix".to_owned())))
        );
    }

    #[test]
    fn dispatches_invalid_help_requests() {
        let unknown = Command {
            name: "help".to_owned(),
            args: "missing".to_owned(),
        };
        let invalid = Command {
            name: "help".to_owned(),
            args: "ping extra".to_owned(),
        };

        assert_eq!(
            dispatch(&unknown),
            Some(Action::Help(HelpRequest::Topic("missing".to_owned())))
        );
        assert_eq!(dispatch(&invalid), Some(Action::Help(HelpRequest::Invalid)));
    }

    #[test]
    fn registry_invariants_and_ownership_are_complete() {
        let names = COMMANDS
            .canonical_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "help",
                "modules",
                "ping",
                "prefix",
                "stats",
                "fastfetch",
                "alias"
            ]
        );
        let module_names = MODULES.iter().map(|module| module.name).collect::<Vec<_>>();
        assert_eq!(module_names, ["core", "system", "aliases"]);
        assert_eq!(
            module_names.iter().copied().collect::<HashSet<_>>().len(),
            MODULES.len()
        );
        assert_eq!(
            MODULES
                .iter()
                .map(|module| module.id)
                .collect::<HashSet<_>>()
                .len(),
            MODULES.len()
        );
        assert_eq!(
            names.iter().copied().collect::<HashSet<_>>().len(),
            names.len()
        );
        assert_eq!(
            COMMANDS
                .canonical_iter()
                .map(|definition| definition.kind)
                .collect::<HashSet<_>>()
                .len(),
            names.len()
        );
        assert!(MODULES.iter().all(|module| !module.description.is_empty()));
        assert!(
            MODULES
                .iter()
                .all(|module| commands_for_module(module.id).next().is_some())
        );
        assert!(names.iter().all(|name| module_by_name(name).is_none()));
        assert_eq!(
            module_by_name("CORE").map(|module| module.id),
            Some(ModuleId::Core)
        );
        assert_eq!(module_definition(ModuleId::Aliases).name, "aliases");
        for definition in COMMANDS.canonical_iter() {
            assert!(!definition.usage.is_empty());
            assert!(!definition.summary.is_empty());
            assert!(!definition.description.is_empty());
            assert!(!definition.icon.is_empty());
            assert!(MODULES.iter().any(|module| module.id == definition.module));
            if matches!(definition.name, "alias" | "prefix") {
                assert!(!definition.aliasable);
                continue;
            }
            let command = Command {
                name: definition.name.to_owned(),
                args: String::new(),
            };
            assert!(dispatch(&command).is_some());
        }
        assert_eq!(
            commands_for_module(ModuleId::Core)
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["help", "modules", "ping", "prefix", "stats"]
        );
        assert_eq!(
            commands_for_module(ModuleId::System)
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["fastfetch"]
        );
        assert_eq!(
            commands_for_module(ModuleId::Aliases)
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["alias"]
        );
    }

    #[test]
    fn dispatches_modules_and_rejects_arguments() {
        let modules = |args: &str| {
            dispatch(&Command {
                name: "modules".to_owned(),
                args: args.to_owned(),
            })
        };
        assert_eq!(
            modules("  \t"),
            Some(Action::Modules(ModulesRequest::Overview))
        );
        assert_eq!(
            modules("core"),
            Some(Action::Modules(ModulesRequest::Invalid))
        );
    }

    #[test]
    fn parses_prefix_without_shell_word_semantics() {
        let prefix = |args: &str| {
            dispatch(&Command {
                name: "prefix".to_owned(),
                args: args.to_owned(),
            })
        };
        assert_eq!(prefix(""), Some(Action::Prefix(super::PrefixRequest::Show)));
        assert_eq!(
            prefix("reset"),
            Some(Action::Prefix(super::PrefixRequest::Reset))
        );
        assert_eq!(
            prefix("."),
            Some(Action::Prefix(super::PrefixRequest::Set(".".to_owned())))
        );
        assert_eq!(
            prefix("' .'"),
            Some(Action::Prefix(super::PrefixRequest::Invalid))
        );
    }

    #[test]
    fn dispatches_fastfetch_and_alias_requests() {
        assert_eq!(
            dispatch(&Command {
                name: "fastfetch".to_owned(),
                args: "--logo none".to_owned()
            }),
            Some(Action::Fastfetch("--logo none".to_owned()))
        );
        assert_eq!(
            dispatch(&Command {
                name: "alias".to_owned(),
                args: "add sys fastfetch --logo none".to_owned()
            }),
            Some(Action::Alias(AliasRequest::Add {
                name: "sys".to_owned(),
                target: "fastfetch".to_owned(),
                args: vec!["--logo".to_owned(), "none".to_owned()],
            }))
        );
    }

    #[test]
    fn parses_alias_show_and_all_delete_spellings() {
        let alias = |args: &str| {
            dispatch(&Command {
                name: "alias".to_owned(),
                args: args.to_owned(),
            })
        };

        assert_eq!(
            alias("show Mini"),
            Some(Action::Alias(AliasRequest::Show {
                name: "Mini".to_owned(),
            }))
        );
        assert_eq!(alias("show"), Some(Action::Alias(AliasRequest::Invalid)));
        assert_eq!(
            alias("show mini extra"),
            Some(Action::Alias(AliasRequest::Invalid))
        );
        for spelling in ["del", "delete", "remove"] {
            assert_eq!(
                alias(&format!("{spelling} mini")),
                Some(Action::Alias(AliasRequest::Delete {
                    name: "mini".to_owned(),
                }))
            );
        }
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
