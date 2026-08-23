use crate::command::Command;
use crate::i18n::Locale;
use crate::modules::ModuleId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    Start,
    Language,
    Ping,
    Stats,
    Info,
    Help,
    Fastfetch,
    Alias,
    Prefix,
    Modules,
    Setup,
    Lm,
    Reboot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRisk {
    ReadOnly,
    PersistentStateChange,
    RestrictedProcess,
    ArbitraryProcess,
    Privileged,
    ExternalCodeInstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDefinition {
    pub kind: CommandKind,
    pub name: &'static str,
    pub usage: &'static str,
    pub examples: &'static [&'static str],
    pub risk: CommandRisk,
    pub icon: &'static str,
    pub aliasable: bool,
    pub module: ModuleId,
}

const COMMAND_SPECS: [CommandDefinition; 13] = [
    CommandDefinition {
        kind: CommandKind::Start,
        name: "start",
        usage: "start [en|ru|skip|bot]",
        examples: &["start", "start ru", "start skip", "start bot"],
        risk: CommandRisk::PersistentStateChange,
        icon: "👋",
        aliasable: false,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Language,
        name: "language",
        usage: "language [en|ru]",
        examples: &["language", "language en", "language ru"],
        risk: CommandRisk::PersistentStateChange,
        icon: "🌐",
        aliasable: false,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Help,
        name: "help",
        usage: "help [command]",
        examples: &["help", "help fastfetch"],
        risk: CommandRisk::ReadOnly,
        icon: "🛠",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Reboot,
        name: "reboot",
        usage: "reboot",
        examples: &["reboot"],
        risk: CommandRisk::Privileged,
        icon: "♻️",
        aliasable: false,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Modules,
        name: "modules",
        usage: "modules",
        examples: &["modules"],
        risk: CommandRisk::ReadOnly,
        icon: "🧩",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Ping,
        name: "ping",
        usage: "ping",
        examples: &["ping"],
        risk: CommandRisk::ReadOnly,
        icon: "🏓",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Prefix,
        name: "prefix",
        usage: "prefix [new-prefix|reset]",
        examples: &["prefix", "prefix .", "prefix reset"],
        risk: CommandRisk::PersistentStateChange,
        icon: "⚙️",
        aliasable: false,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Setup,
        name: "setup",
        usage: "setup [<username_bot>|auto|status|repair|cancel]",
        examples: &[
            "setup",
            "setup lavis_example_bot",
            "setup auto",
            "setup status",
            "setup repair",
            "setup cancel",
        ],
        risk: CommandRisk::Privileged,
        icon: "🛠",
        aliasable: false,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Lm,
        name: "lm",
        usage: "lm [list|info <id>|logs <id>|doctor [id]|install|confirm <approval-id>|cancel <approval-id>|enable <id>|disable <id>]",
        examples: &[
            "lm",
            "lm list",
            "lm info <id>",
            "lm logs <id>",
            "lm doctor [id]",
            "lm install",
            "lm confirm <approval-id>",
            "lm cancel <approval-id>",
            "lm enable <id>",
            "lm disable <id>",
        ],
        risk: CommandRisk::ExternalCodeInstall,
        icon: "📦",
        aliasable: false,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Stats,
        name: "stats",
        usage: "stats",
        examples: &["stats"],
        risk: CommandRisk::ReadOnly,
        icon: "📊",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Info,
        name: "info",
        usage: "info",
        examples: &["info"],
        risk: CommandRisk::ReadOnly,
        icon: "ℹ️",
        aliasable: true,
        module: ModuleId::Core,
    },
    CommandDefinition {
        kind: CommandKind::Fastfetch,
        name: "fastfetch",
        usage: "fastfetch [--no-profile] [--logo <...>] [--structure <...>] [--separator <text>] [--logo-padding-left <n>] [--logo-padding-right <n>] [--logo-padding-top <n>]",
        examples: &[
            "fastfetch",
            "fastfetch --logo arch",
            "fastfetch --no-profile",
        ],
        risk: CommandRisk::RestrictedProcess,
        icon: "🖥",
        aliasable: true,
        module: ModuleId::System,
    },
    CommandDefinition {
        kind: CommandKind::Alias,
        name: "alias",
        usage: "alias [list|add <name> <command> [arguments...]|show <name>|del <name>]",
        examples: &["alias list", "alias add sys fastfetch", "alias del sys"],
        risk: CommandRisk::PersistentStateChange,
        icon: "🔗",
        aliasable: false,
        module: ModuleId::Aliases,
    },
];

pub fn command_summary(kind: CommandKind, locale: Locale) -> &'static str {
    match (kind, locale) {
        (CommandKind::Start, Locale::English) => "Start the tutorial or companion setup",
        (CommandKind::Start, Locale::Russian) => "Начать обучение",
        (CommandKind::Language, Locale::English) => "Show or change the interface language",
        (CommandKind::Language, Locale::Russian) => "Показать или изменить язык",
        (CommandKind::Ping, Locale::English) => "Measure Telegram latency",
        (CommandKind::Ping, Locale::Russian) => "Измерить задержку Telegram",
        (CommandKind::Stats, Locale::English) => "Show runtime statistics",
        (CommandKind::Stats, Locale::Russian) => "Показать статистику работы",
        (CommandKind::Info, Locale::English) => "Show Lavis project information",
        (CommandKind::Info, Locale::Russian) => "Показать информацию о Lavis",
        (CommandKind::Help, Locale::English) => "Show command help",
        (CommandKind::Help, Locale::Russian) => "Показать справку",
        (CommandKind::Fastfetch, Locale::English) => "Show safe system information",
        (CommandKind::Fastfetch, Locale::Russian) => "Показать системную информацию",
        (CommandKind::Alias, Locale::English) => "Manage command aliases",
        (CommandKind::Alias, Locale::Russian) => "Управлять псевдонимами команд",
        (CommandKind::Prefix, Locale::English) => "Show or change the command prefix",
        (CommandKind::Prefix, Locale::Russian) => "Показать или изменить префикс",
        (CommandKind::Modules, Locale::English) => "List built-in modules",
        (CommandKind::Modules, Locale::Russian) => "Список внутренних модулей",
        (CommandKind::Setup, Locale::English) => "Configure the companion workspace",
        (CommandKind::Setup, Locale::Russian) => "Настроить companion",
        (CommandKind::Lm, Locale::English) => "Inspect and install external modules",
        (CommandKind::Lm, Locale::Russian) => "Проверить и установить внешний модуль",
        (CommandKind::Reboot, Locale::English) => "Restart Lavis",
        (CommandKind::Reboot, Locale::Russian) => "Перезапустить Lavis",
    }
}

pub fn command_description(kind: CommandKind, locale: Locale) -> &'static str {
    match (kind, locale) {
        (CommandKind::Start, Locale::English) => {
            "Start the tutorial or the single safe companion and private workspace setup flow."
        }
        (CommandKind::Start, Locale::Russian) => {
            "Запускает последовательное обучение Lavis или единый безопасный сценарий настройки companion и приватного workspace."
        }
        (CommandKind::Language, Locale::English) => {
            "Shows the selected interface language or saves a new one without resetting settings."
        }
        (CommandKind::Language, Locale::Russian) => {
            "Показывает выбранный язык интерфейса или сохраняет новый без сброса настроек."
        }
        (CommandKind::Help, Locale::English) => {
            "Shows a command overview or detailed help for a command, alias, or module."
        }
        (CommandKind::Help, Locale::Russian) => {
            "Показывает обзор команд или подробную справку о команде, псевдониме либо модуле."
        }
        (CommandKind::Reboot, Locale::English) => {
            "Edits the command message to restarting status and, after successful startup, to a confirmation with truncated whole seconds; it creates no separate message."
        }
        (CommandKind::Reboot, Locale::Russian) => {
            "Редактирует сообщение команды в статус перезапуска, а после успешного запуска — в подтверждение с целым временем в секундах с усечением дробной части; отдельное сообщение не создаётся."
        }
        (CommandKind::Modules, Locale::English) => {
            "Lists statically registered Lavis modules and their commands."
        }
        (CommandKind::Modules, Locale::Russian) => {
            "Перечисляет статически зарегистрированные модули Lavis и их команды."
        }
        (CommandKind::Ping, Locale::English) => {
            "Measures a real MTProto RPC request through the current authorized session."
        }
        (CommandKind::Ping, Locale::Russian) => {
            "Измеряет время реального MTProto RPC-запроса через текущую авторизованную сессию."
        }
        (CommandKind::Prefix, Locale::English) => "Shows the active prefix or saves a new one.",
        (CommandKind::Prefix, Locale::Russian) => {
            "Показывает активный префикс или сохраняет новый."
        }
        (CommandKind::Setup, Locale::English) => {
            "Starts or manages companion setup for the specified user."
        }
        (CommandKind::Setup, Locale::Russian) => {
            "Запускает или управляет настройкой companion для указанного пользователя."
        }
        (CommandKind::Lm, Locale::English) => {
            "Shows external modules or starts a reviewed installation with separate confirmation."
        }
        (CommandKind::Lm, Locale::Russian) => {
            "Показывает внешние модули или запускает проверяемую установку с отдельным подтверждением."
        }
        (CommandKind::Stats, Locale::English) => {
            "Shows Telegram latency, Lavis and host uptime, memory, command count, and package version."
        }
        (CommandKind::Stats, Locale::Russian) => {
            "Показывает задержку Telegram, время работы Lavis и хоста, память, число команд и версию пакета."
        }
        (CommandKind::Info, Locale::English) => {
            "Shows owner, version, current commit, upstream status, active prefix, module counts, host, and OS."
        }
        (CommandKind::Info, Locale::Russian) => {
            "Показывает владельца, версию, текущий коммит, статус основной ветки, активный префикс, количество модулей, хост и ОС."
        }
        (CommandKind::Fastfetch, Locale::English) => {
            "Runs Fastfetch only with restricted safe display options."
        }
        (CommandKind::Fastfetch, Locale::Russian) => {
            "Запускает Fastfetch только с ограниченными безопасными параметрами отображения."
        }
        (CommandKind::Alias, Locale::English) => {
            "Creates, shows, and deletes persistent aliases for canonical commands."
        }
        (CommandKind::Alias, Locale::Russian) => {
            "Создаёт, показывает и удаляет постоянные псевдонимы канонических команд."
        }
    }
}

pub fn commands() -> &'static [CommandDefinition] {
    &COMMAND_SPECS
}

pub fn command_by_kind(kind: CommandKind) -> Option<&'static CommandDefinition> {
    commands().iter().find(|command| command.kind == kind)
}

pub fn command_by_name(name: &str) -> Option<&'static CommandDefinition> {
    commands()
        .iter()
        .find(|command| command.name.eq_ignore_ascii_case(name))
}

pub fn module_for_command(
    command: &CommandDefinition,
) -> Option<&'static crate::modules::ModuleSpec> {
    crate::modules::module_by_id(command.module)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpRequest {
    Overview,
    Topic(String),
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalInvocation {
    pub module_id: String,
    pub command_name: String,
    pub arguments: String,
    pub argument_entities: Vec<crate::external_modules::protocol::CustomEmojiEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Start(StartRequest),
    Language(LanguageRequest),
    Ping,
    Stats,
    Info,
    Help(HelpRequest),
    Fastfetch(String),
    Alias(AliasRequest),
    Prefix(PrefixRequest),
    Modules(ModulesRequest),
    Setup(SetupRequest),
    Lm(LmRequest),
    Reboot,
    External(ExternalInvocation),
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
    pub fn name(&self) -> &str {
        match self {
            Self::Start(_) => "start",
            Self::Language(_) => "language",
            Self::Ping => "ping",
            Self::Stats => "stats",
            Self::Info => "info",
            Self::Help(_) => "help",
            Self::Fastfetch(_) => "fastfetch",
            Self::Alias(_) => "alias",
            Self::Prefix(_) => "prefix",
            Self::Modules(_) => "modules",
            Self::Setup(_) => "setup",
            Self::Lm(_) => "lm",
            Self::Reboot => "reboot",
            Self::External(_invocation) => {
                // Safe bounded string: module_id and command_name are validated ASCII
                // This is never user-controlled free text
                // Return a static prefix to avoid allocating
                "external"
            }
        }
    }
}

pub fn dispatch(command: &Command) -> Option<Action> {
    let definition = canonical_command(&command.name)?;
    match definition.kind {
        CommandKind::Start => Some(Action::Start(parse_start_request(&command.args))),
        CommandKind::Language => Some(Action::Language(parse_language_request(&command.args))),
        CommandKind::Ping => Some(Action::Ping),
        CommandKind::Stats => Some(Action::Stats),
        CommandKind::Info if command.args.trim().is_empty() => Some(Action::Info),
        CommandKind::Info => None,
        CommandKind::Help => Some(Action::Help(parse_help_request(&command.args))),
        CommandKind::Fastfetch => Some(Action::Fastfetch(command.args.clone())),
        CommandKind::Alias => Some(Action::Alias(parse_alias_request(&command.args))),
        CommandKind::Prefix => Some(Action::Prefix(parse_prefix_request(&command.args))),
        CommandKind::Modules => Some(Action::Modules(parse_modules_request(&command.args))),
        CommandKind::Setup => Some(Action::Setup(parse_setup_request(&command.args))),
        CommandKind::Lm => Some(Action::Lm(parse_lm_request(&command.args))),
        CommandKind::Reboot if command.args.trim().is_empty() => Some(Action::Reboot),
        CommandKind::Reboot => None,
    }
}

pub fn definition(kind: CommandKind) -> &'static CommandDefinition {
    command_by_kind(kind).unwrap_or_else(|| unreachable!("all CommandKind values are registered"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModulesRequest {
    Overview,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRequest {
    Begin,
    Locale(crate::i18n::Locale),
    Skip,
    Bot,
    Invalid,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageRequest {
    Show,
    Set(crate::i18n::Locale),
    Invalid,
}

fn parse_start_request(args: &str) -> StartRequest {
    let mut words = args.split_whitespace();
    let Some(word) = words.next() else {
        return StartRequest::Begin;
    };
    if words.next().is_some() {
        return StartRequest::Invalid;
    }
    match word.to_ascii_lowercase().as_str() {
        "skip" => StartRequest::Skip,
        "bot" => StartRequest::Bot,
        _ => crate::i18n::Locale::parse(word)
            .map(StartRequest::Locale)
            .unwrap_or(StartRequest::Invalid),
    }
}
fn parse_language_request(args: &str) -> LanguageRequest {
    let mut words = args.split_whitespace();
    let Some(word) = words.next() else {
        return LanguageRequest::Show;
    };
    if words.next().is_some() {
        return LanguageRequest::Invalid;
    }
    crate::i18n::Locale::parse(word)
        .map(LanguageRequest::Set)
        .unwrap_or(LanguageRequest::Invalid)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupRequest {
    Start,
    Username(String),
    Auto,
    Status,
    Repair,
    Cancel,
    Invalid,
}

/// A syntactically valid, canonical approval identifier.
///
/// Integration contract: the install runtime must issue and accept the unchanged
/// 19-character uppercase Crockford Base32 value in `XXXX-XXXX-XXXX-XXXX`
/// form exposed through `as_str()`. This command layer intentionally does not
/// depend on the runtime approval type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalId(String);

impl ApprovalId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LmRequest {
    Overview,
    List,
    Install,
    Confirm { approval_id: ApprovalId },
    Cancel { approval_id: ApprovalId },
    Info { id: String },
    Logs { id: String },
    Doctor { id: Option<String> },
    Enable { id: String },
    Disable { id: String },
    Invalid,
}

fn parse_modules_request(args: &str) -> ModulesRequest {
    if args.trim().is_empty() {
        ModulesRequest::Overview
    } else {
        ModulesRequest::Invalid
    }
}

fn parse_setup_request(args: &str) -> SetupRequest {
    let mut tokens = args.split_whitespace();
    let Some(token) = tokens.next() else {
        return SetupRequest::Start;
    };
    if tokens.next().is_some() {
        return SetupRequest::Invalid;
    }
    match token {
        "auto" => SetupRequest::Auto,
        "status" => SetupRequest::Status,
        "repair" => SetupRequest::Repair,
        "cancel" => SetupRequest::Cancel,
        username => SetupRequest::Username(username.to_owned()),
    }
}

fn parse_lm_request(args: &str) -> LmRequest {
    let mut tokens = args.split_whitespace();
    let Some(operation) = tokens.next() else {
        return LmRequest::Overview;
    };

    match operation {
        "list" if tokens.next().is_none() => LmRequest::List,
        "install" if tokens.next().is_none() => LmRequest::Install,
        "confirm" => parse_lm_approval(tokens)
            .map(|approval_id| LmRequest::Confirm { approval_id })
            .unwrap_or(LmRequest::Invalid),
        "cancel" => parse_lm_approval(tokens)
            .map(|approval_id| LmRequest::Cancel { approval_id })
            .unwrap_or(LmRequest::Invalid),
        "info" => parse_lm_id(tokens)
            .map(|id| LmRequest::Info { id })
            .unwrap_or(LmRequest::Invalid),
        "logs" => parse_lm_id(tokens)
            .map(|id| LmRequest::Logs { id })
            .unwrap_or(LmRequest::Invalid),
        "doctor" => {
            let mut id_tokens = tokens.clone();
            match (id_tokens.next(), id_tokens.next()) {
                (None, _) => LmRequest::Doctor { id: None },
                (Some(id), None) => LmRequest::Doctor {
                    id: Some(id.to_owned()),
                },
                _ => LmRequest::Invalid,
            }
        }
        "enable" => parse_lm_id(tokens)
            .map(|id| LmRequest::Enable { id })
            .unwrap_or(LmRequest::Invalid),
        "disable" => parse_lm_id(tokens)
            .map(|id| LmRequest::Disable { id })
            .unwrap_or(LmRequest::Invalid),
        _ => LmRequest::Invalid,
    }
}

fn parse_lm_id(mut tokens: std::str::SplitWhitespace<'_>) -> Option<String> {
    let id = tokens.next()?;
    (tokens.next().is_none()).then(|| id.to_owned())
}

fn parse_lm_approval(mut tokens: std::str::SplitWhitespace<'_>) -> Option<ApprovalId> {
    let value = tokens.next()?;
    if tokens.next().is_some() || !is_canonical_approval_id(value) {
        return None;
    }
    Some(ApprovalId(value.to_owned()))
}

fn is_canonical_approval_id(value: &str) -> bool {
    value.len() == 19
        && value.bytes().enumerate().all(|(index, byte)| match index {
            4 | 9 | 14 => byte == b'-',
            _ => matches!(
                byte,
                b'0'..=b'9'
                    | b'A'..=b'H'
                    | b'J'..=b'K'
                    | b'M'..=b'N'
                    | b'P'..=b'T'
                    | b'V'..=b'Z'
            ),
        })
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
    commands().iter().find(|command| command.name == name)
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
    use super::{
        Action, AliasRequest, CommandKind, CommandRisk, HelpRequest, LanguageRequest, LmRequest,
        ModulesRequest, SetupRequest, StartRequest, command_by_kind, command_by_name,
        command_description, command_summary, commands, dispatch, module_for_command,
    };
    use crate::command::Command;
    use crate::i18n::Locale;
    use crate::modules::{
        ModuleId, commands_for_module, module_by_name, module_definition, modules,
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
    fn dispatches_info() {
        let command = Command {
            name: "info".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), Some(Action::Info));
        assert_eq!(
            dispatch(&Command {
                name: "info".to_owned(),
                args: "extra".to_owned(),
            }),
            None
        );
    }

    #[test]
    fn dispatches_start_and_language_with_exact_arguments() {
        assert_eq!(
            dispatch(&Command {
                name: "start".into(),
                args: "ru".into()
            }),
            Some(Action::Start(StartRequest::Locale(
                crate::i18n::Locale::Russian
            )))
        );
        assert_eq!(
            dispatch(&Command {
                name: "start".into(),
                args: "group".into()
            }),
            Some(Action::Start(StartRequest::Invalid))
        );
        assert_eq!(
            dispatch(&Command {
                name: "language".into(),
                args: "en".into()
            }),
            Some(Action::Language(LanguageRequest::Set(
                crate::i18n::Locale::English
            )))
        );
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
        let names = commands()
            .iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "start",
                "language",
                "help",
                "reboot",
                "modules",
                "ping",
                "prefix",
                "setup",
                "lm",
                "stats",
                "info",
                "fastfetch",
                "alias"
            ]
        );
        let module_names = modules()
            .iter()
            .map(|module| module.name)
            .collect::<Vec<_>>();
        assert_eq!(module_names, ["core", "system", "aliases"]);
        assert_eq!(
            module_names.iter().copied().collect::<HashSet<_>>().len(),
            modules().len()
        );
        assert_eq!(
            modules()
                .iter()
                .map(|module| module.id)
                .collect::<HashSet<_>>()
                .len(),
            modules().len()
        );
        assert_eq!(
            names.iter().copied().collect::<HashSet<_>>().len(),
            names.len()
        );
        assert_eq!(
            commands()
                .iter()
                .map(|definition| definition.kind)
                .collect::<HashSet<_>>()
                .len(),
            names.len()
        );
        assert!(modules().iter().all(|module| {
            !crate::modules::module_description(module.id, Locale::English).is_empty()
                && !crate::modules::module_description(module.id, Locale::Russian).is_empty()
        }));
        assert!(
            modules()
                .iter()
                .all(|module| commands_for_module(module.id).next().is_some())
        );
        assert!(names.iter().all(|name| module_by_name(name).is_none()));
        assert_eq!(
            module_by_name("CORE").map(|module| module.id),
            Some(ModuleId::Core)
        );
        assert_eq!(module_definition(ModuleId::Aliases).name, "aliases");
        for definition in commands() {
            assert!(!definition.usage.is_empty());
            assert!(!command_summary(definition.kind, Locale::English).is_empty());
            assert!(!command_summary(definition.kind, Locale::Russian).is_empty());
            assert!(!command_description(definition.kind, Locale::English).is_empty());
            assert!(!command_description(definition.kind, Locale::Russian).is_empty());
            assert!(!definition.icon.is_empty());
            assert!(
                modules()
                    .iter()
                    .any(|module| module.id == definition.module)
            );
            if matches!(
                definition.name,
                "alias" | "prefix" | "setup" | "lm" | "reboot"
            ) {
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
            [
                "start", "language", "help", "reboot", "modules", "ping", "prefix", "setup", "lm",
                "stats", "info"
            ]
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
    fn static_registry_api_has_complete_safe_metadata() {
        let command_names = commands()
            .iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert_eq!(
            command_names
                .iter()
                .map(|name| name.to_ascii_lowercase())
                .collect::<HashSet<_>>()
                .len(),
            command_names.len()
        );
        assert_eq!(
            commands()
                .iter()
                .map(|command| command.kind)
                .collect::<HashSet<_>>()
                .len(),
            commands().len()
        );
        assert_eq!(
            command_by_name("PING").map(|command| command.kind),
            Some(CommandKind::Ping)
        );
        assert_eq!(
            command_by_kind(CommandKind::Fastfetch).map(|command| command.name),
            Some("fastfetch")
        );

        for command in commands() {
            assert!(!command.usage.is_empty());
            assert!(!command_summary(command.kind, Locale::English).is_empty());
            assert!(!command_summary(command.kind, Locale::Russian).is_empty());
            assert!(!command_description(command.kind, Locale::English).is_empty());
            assert!(!command_description(command.kind, Locale::Russian).is_empty());
            assert!(!command.icon.is_empty());
            assert!(!command.examples.is_empty());
            assert!(command.examples.iter().all(|example| {
                !example.starts_with(',')
                    && !example.starts_with('.')
                    && example.split_whitespace().next() == Some(command.name)
            }));
            assert!(module_for_command(command).is_some());
            assert!(!matches!(command.risk, CommandRisk::ArbitraryProcess));
        }
        assert_eq!(
            crate::modules::modules()
                .iter()
                .map(|module| crate::modules::commands_for_module(module.id).count())
                .sum::<usize>(),
            commands().len()
        );
        assert_eq!(
            command_by_kind(CommandKind::Fastfetch).unwrap().risk,
            CommandRisk::RestrictedProcess
        );
        for kind in [
            CommandKind::Help,
            CommandKind::Modules,
            CommandKind::Ping,
            CommandKind::Stats,
            CommandKind::Info,
        ] {
            assert_eq!(command_by_kind(kind).unwrap().risk, CommandRisk::ReadOnly);
        }
        for kind in [CommandKind::Prefix, CommandKind::Alias] {
            assert_eq!(
                command_by_kind(kind).unwrap().risk,
                CommandRisk::PersistentStateChange
            );
        }
        let setup = command_by_kind(CommandKind::Setup).unwrap();
        assert_eq!(setup.risk, CommandRisk::Privileged);
        assert_eq!(setup.module, ModuleId::Core);
        assert!(!setup.aliasable);
        let reboot = command_by_kind(CommandKind::Reboot).unwrap();
        assert_eq!(reboot.risk, CommandRisk::Privileged);
        assert_eq!(reboot.module, ModuleId::Core);
        assert!(!reboot.aliasable);
        let lm = command_by_kind(CommandKind::Lm).unwrap();
        assert_eq!(lm.risk, CommandRisk::ExternalCodeInstall);
        assert_eq!(lm.module, ModuleId::Core);
        assert!(!lm.aliasable);
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
    fn parses_setup_requests_with_exact_forms() {
        let setup = |args: &str| {
            dispatch(&Command {
                name: "setup".to_owned(),
                args: args.to_owned(),
            })
        };

        assert_eq!(setup("  \t"), Some(Action::Setup(SetupRequest::Start)));
        assert_eq!(
            setup("start"),
            Some(Action::Setup(SetupRequest::Username("start".to_owned())))
        );
        assert_eq!(
            setup("candidate"),
            Some(Action::Setup(SetupRequest::Username(
                "candidate".to_owned()
            )))
        );
        assert_eq!(setup("auto"), Some(Action::Setup(SetupRequest::Auto)));
        assert_eq!(setup("status"), Some(Action::Setup(SetupRequest::Status)));
        assert_eq!(setup("repair"), Some(Action::Setup(SetupRequest::Repair)));
        assert_eq!(setup("cancel"), Some(Action::Setup(SetupRequest::Cancel)));
        assert_eq!(
            setup("candidate extra"),
            Some(Action::Setup(SetupRequest::Invalid))
        );
    }

    #[test]
    fn parses_lm_requests_with_full_canonical_approval_ids_only() {
        let lm = |args: &str| {
            dispatch(&Command {
                name: "lm".to_owned(),
                args: args.to_owned(),
            })
        };
        let approval = "0123-4567-89AB-CDEF";

        assert_eq!(lm(" \t"), Some(Action::Lm(LmRequest::Overview)));
        assert_eq!(lm("list"), Some(Action::Lm(LmRequest::List)));
        assert_eq!(lm("install"), Some(Action::Lm(LmRequest::Install)));
        assert_eq!(
            lm("info echo"),
            Some(Action::Lm(LmRequest::Info {
                id: "echo".to_owned()
            }))
        );
        assert_eq!(
            lm("logs echo"),
            Some(Action::Lm(LmRequest::Logs {
                id: "echo".to_owned()
            }))
        );
        assert_eq!(
            lm("doctor"),
            Some(Action::Lm(LmRequest::Doctor { id: None }))
        );
        assert_eq!(
            lm("doctor echo"),
            Some(Action::Lm(LmRequest::Doctor {
                id: Some("echo".to_owned())
            }))
        );
        assert_eq!(
            lm("enable echo"),
            Some(Action::Lm(LmRequest::Enable {
                id: "echo".to_owned()
            }))
        );
        assert_eq!(
            lm("disable echo"),
            Some(Action::Lm(LmRequest::Disable {
                id: "echo".to_owned()
            }))
        );

        let Some(Action::Lm(LmRequest::Confirm { approval_id })) =
            lm(&format!("confirm {approval}"))
        else {
            panic!("expected canonical confirmation request");
        };
        assert_eq!(approval_id.as_str(), approval);

        let Some(Action::Lm(LmRequest::Cancel { approval_id })) = lm(&format!("cancel {approval}"))
        else {
            panic!("expected canonical cancellation request");
        };
        assert_eq!(approval_id.as_str(), approval);

        for invalid in [
            "list extra",
            "install https://example.invalid/module.lmod",
            "install extra",
            "doctor extra args",
            "confirm",
            "cancel",
            "confirm 0123-4567-89AB-CDE",
            "cancel 0123-4567-89AB-CDEFF",
            "confirm 0123-4567-89ab-CDEF",
            "cancel 0123-4567-89AI-CDEF",
            "confirm 0123456789ABCDEF",
            "confirm 0123-4567-89AB-CDEF extra",
            "unknown",
        ] {
            assert_eq!(
                lm(invalid),
                Some(Action::Lm(LmRequest::Invalid)),
                "{invalid}"
            );
        }
    }

    #[test]
    fn dispatches_reboot_without_arguments_only() {
        assert_eq!(
            dispatch(&Command {
                name: "reboot".to_owned(),
                args: String::new()
            }),
            Some(Action::Reboot)
        );
        assert_eq!(
            dispatch(&Command {
                name: "reboot".to_owned(),
                args: "now".to_owned()
            }),
            None
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
