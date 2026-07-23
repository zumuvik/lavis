pub use crate::response::Response;

use crate::{
    aliases::AliasStore,
    commands::{
        CommandDefinition, CommandRisk, HelpRequest, canonical_command, command_by_name,
        commands, module_for_command,
    },
    modules::{
        ModuleCapability, ModuleOrigin, ModuleSpec, commands_for_module, module_by_name, modules,
        validate_external_origin,
    },
    response::RenderedResponse,
};

pub struct RenderedHelp {
    pub response: Response,
    pub entity_fallback: bool,
}

pub fn render(request: &HelpRequest, prefix: &str, aliases: &AliasStore) -> RenderedHelp {
    match request {
        HelpRequest::Overview => render_overview(prefix),
        HelpRequest::Topic(topic) => render_topic(topic, prefix, aliases),
        HelpRequest::Invalid => plain(format!("⚠️ Использование: {prefix}help [команда]")),
    }
}

pub fn render_modules_overview(prefix: &str) -> RenderedHelp {
    let module_names = modules()
        .iter()
        .map(|module| format!("{} {}", module.icon, module.name))
        .collect::<Vec<_>>()
        .join(", ");
    let command_names = commands()
        .iter()
        .map(|command| format!("{prefix}{}", command.name))
        .collect::<Vec<_>>()
        .join(", ");
    documentation(
        format!("🧩 Модули Lavis: {}", modules().len()),
        format!(
            "Модули: {module_names}\nКоманды ({}): {command_names}\n\nИспользуйте {prefix}help <команда или модуль> для подробностей.",
            commands().len()
        ),
        core_provenance(),
    )
}

fn render_module_card(module: &ModuleSpec, prefix: &str) -> RenderedHelp {
    if !validate_external_origin(&module.origin) {
        return documentation(
            "⚠️ Некорректные метаданные происхождения модуля".to_owned(),
            "Внешние данные модуля не отображаются.".to_owned(),
            "⚠️ Метаданные происхождения отклонены ядром Lavis.".to_owned(),
        );
    }
    let command_list = commands_for_module(module.id)
        .map(|command| format!("{prefix}{} — {}", command.usage, command.summary_ru))
        .collect::<Vec<_>>()
        .join("\n");
    let primary = format!(
        "{}\n\nКоманды:\n{command_list}\n\nВозможности: {}\nПолитика: {}",
        module.description_ru,
        capability_labels(module.capabilities),
        module_policy(module)
    );
    documentation(
        format!("{} Модуль {}", module.icon, module.name),
        primary,
        module_provenance(module),
    )
}

fn render_overview(prefix: &str) -> RenderedHelp {
    let body = modules()
        .iter()
        .map(|module| {
            let names = commands_for_module(module.id)
                .map(|command| format!("{prefix}{}", command.name))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} {}: {names}", module.icon, module.name)
        })
        .collect::<Vec<_>>()
        .join("\n");
    documentation(
        format!("🛠 Справка Lavis: {} модулей, {} команд", modules().len(), commands().len()),
        format!("{body}\n\nИспользуйте {prefix}help <команда или модуль> для подробностей."),
        core_provenance(),
    )
}

fn render_topic(topic: &str, prefix: &str, aliases: &AliasStore) -> RenderedHelp {
    if let Some(command) = canonical_command(topic) {
        return render_command_card(command, prefix);
    }
    if let Some(rendered) = render_alias(topic, prefix, aliases) {
        return rendered;
    }
    if let Some(module) = module_by_name(topic) {
        return render_module_card(module, prefix);
    }
    plain(format!(
        "❓ Неизвестная команда или модуль: {topic}\nИспользуйте {prefix}help для списка команд."
    ))
}

fn render_command_card(command: &CommandDefinition, prefix: &str) -> RenderedHelp {
    let Some(module) = module_for_command(command) else {
        return plain(format!("⚠️ Метаданные команды {prefix}{} недоступны", command.name));
    };
    let primary = if command.name == "fastfetch" {
        fastfetch_primary(prefix, command, module.name)
    } else {
        generic_command_primary(command, prefix, module.name)
    };
    documentation(
        format!("{} {prefix}{}", command.icon, command.usage),
        primary,
        module_provenance(module),
    )
}

fn generic_command_primary(command: &CommandDefinition, prefix: &str, module_name: &str) -> String {
    let examples = command
        .examples
        .iter()
        .map(|example| format!("{prefix}{example}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\n\nИспользование: {prefix}{}\nМодуль: {module_name}\nРиск: {}\nПримеры:\n{examples}",
        command.description_ru,
        command.usage,
        risk_label(command.risk)
    )
}

fn fastfetch_primary(prefix: &str, command: &CommandDefinition, module_name: &str) -> String {
    format!(
        "{}\n\nИспользование: {prefix}{}\nМодуль: {module_name}\nРиск: {}\nПримеры: {prefix}fastfetch --logo arch; {prefix}fastfetch --structure OS:Kernel:CPU.\n\nЛоготипы: none, Alpine, Arch, Debian, Fedora, FreeBSD, Linux, MacOS, NixOS, OpenBSD, Ubuntu, Windows.\nСтруктура: title, separator, os, kernel, uptime, cpu, memory, gpu, packages, shell, terminal, terminalsize, host, display, wm, de, theme, icons, font, cursor, disk, swap, localip, battery, poweradapter, locale.\nРазделитель: 1–64 печатных ASCII-символа.\n\n{prefix}fastfetch --no-profile не читает профиль. Профиль: $XDG_CONFIG_HOME/lavis/fastfetch.json или $HOME/.config/lavis/fastfetch.json.\nМинимальный JSON: {{ \"version\": 1 }}\nПриоритет: значения Fastfetch по умолчанию < профиль < параметры команды.\nПсевдоним: {prefix}alias add sys fastfetch --logo arch; затем {prefix}sys.\n\nКавычки группируют аргументы для разбора shell-words; оболочка не запускается, а shell-метасимволы остаются данными. Каждый процесс запускается только с --config none --pipe; нативные конфиги и пресеты Fastfetch запрещены. Вывод может раскрыть данные хоста, сети, дисплея, питания и оборудования.",
        command.description_ru,
        command.usage,
        risk_label(command.risk)
    )
}

fn render_alias(topic: &str, prefix: &str, aliases: &AliasStore) -> Option<RenderedHelp> {
    let alias = aliases.lookup(topic)?;
    let command = command_by_name(&alias.target)?;
    let module = module_for_command(command)?;
    let stored = if alias.args.is_empty() {
        "нет".to_owned()
    } else {
        shell_words::join(&alias.args)
    };
    Some(documentation(
        format!("🔗 {prefix}{topic}"),
        format!(
            "Псевдоним вызывает {prefix}{}; аргументы вызова добавляются после сохранённых аргументов.\nСохранённые аргументы: {stored}\nЦелевой модуль: {}\nРиск цели: {}",
            command.name,
            module.name,
            risk_label(command.risk)
        ),
        module_provenance(module),
    ))
}

fn capability_labels(capabilities: &[ModuleCapability]) -> String {
    capabilities
        .iter()
        .map(|capability| match capability {
            ModuleCapability::TelegramRpc => "Telegram RPC",
            ModuleCapability::PersistentStateRead => "чтение постоянного состояния",
            ModuleCapability::PersistentStateWrite => "изменение постоянного состояния",
            ModuleCapability::HostInformation => "сведения о хосте",
            ModuleCapability::RestrictedProcess => "ограниченный процесс",
            ModuleCapability::Network => "сеть",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn module_policy(module: &ModuleSpec) -> String {
    format!(
        "выгрузка: {}; замена: {}",
        if module.unloadable { "разрешена" } else { "запрещена" },
        if module.replaceable { "разрешена" } else { "запрещена" }
    )
}

fn module_provenance(module: &ModuleSpec) -> String {
    match module.origin {
        ModuleOrigin::Builtin => {
            "Это встроенный модуль Lavis. Его нельзя выгрузить или заменить.".to_owned()
        }
        ModuleOrigin::External {
            author,
            version,
            source,
        } => format!(
            "Внешний модуль. Автор: {author}; версия: {version}; источник: {source}; возможности: {}; {}.",
            capability_labels(module.capabilities),
            module_policy(module)
        ),
    }
}

fn core_provenance() -> String {
    "Это встроенный модуль Lavis. Его нельзя выгрузить или заменить.".to_owned()
}

fn risk_label(risk: CommandRisk) -> &'static str {
    match risk {
        CommandRisk::ReadOnly => "только чтение",
        CommandRisk::PersistentStateChange => "изменение постоянного состояния",
        CommandRisk::RestrictedProcess => "ограниченный процесс",
        CommandRisk::ArbitraryProcess => "произвольный процесс",
        CommandRisk::Privileged => "привилегированная операция",
    }
}

fn documentation(heading: String, primary: String, provenance: String) -> RenderedHelp {
    let RenderedResponse {
        response,
        entity_fallback,
    } = Response::documentation_card(heading, primary, provenance);
    RenderedHelp {
        response,
        entity_fallback,
    }
}

fn plain(text: String) -> RenderedHelp {
    RenderedHelp {
        response: Response::plain(text),
        entity_fallback: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{render, render_module_card, render_modules_overview};
    use crate::{
        aliases::{Alias, AliasStore},
        commands::HelpRequest,
        modules::{ModuleCapability, ModuleId, ModuleOrigin, ModuleSpec},
    };
    use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    async fn aliases() -> AliasStore {
        AliasStore::load(PathBuf::from("/nonexistent/lavis-help-aliases.json"))
            .await
            .unwrap()
    }

    async fn aliases_with_core_alias() -> (AliasStore, PathBuf) {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("lavis-help-{nonce}"));
        fs::create_dir_all(&directory).unwrap();
        let mut aliases = AliasStore::load(directory.join("aliases.json")).await.unwrap();
        aliases
            .add(
                "core",
                Alias {
                    target: "fastfetch".to_owned(),
                    args: vec!["--logo".to_owned(), "arch".to_owned()],
                },
            )
            .await
            .unwrap();
        (aliases, directory)
    }

    #[tokio::test]
    async fn overview_has_stable_counts_order_and_active_prefix() {
        let response = render(&HelpRequest::Overview, "🦀", &aliases().await).response;
        assert!(response.text.starts_with("🛠 Справка Lavis: 3 модулей, 7 команд"));
        assert!(response.text.find("🧩 core").unwrap() < response.text.find("🖥 system").unwrap());
        assert!(response.text.contains("🦀fastfetch"));
        assert!(response.text.ends_with("Используйте 🦀help <команда или модуль> для подробностей."));
        assert_eq!(response.entities.len(), 2);
    }

    #[tokio::test]
    async fn command_cards_use_documentation_entities_and_symbolic_fastfetch_paths() {
        let response = render(&HelpRequest::Topic("fastfetch".to_owned()), "🦀", &aliases().await).response;
        assert_eq!(response.entities.len(), 2);
        assert!(response.text.contains("$XDG_CONFIG_HOME/lavis/fastfetch.json"));
        assert!(response.text.contains("$HOME/.config/lavis/fastfetch.json"));
        assert!(response.text.contains("--config none"));
        assert!(response.text.contains("🦀fastfetch --no-profile"));
        assert!(response.text.contains("shell-words"));
        assert!(response.text.contains("shell-метасимволы остаются данными"));
        assert!(!response.text.contains("/tmp/"));
        let grammers_client::tl::enums::MessageEntity::Blockquote(primary) = &response.entities[0]
        else {
            panic!("expected primary blockquote");
        };
        let grammers_client::tl::enums::MessageEntity::Blockquote(provenance) = &response.entities[1]
        else {
            panic!("expected provenance blockquote");
        };
        let units = response.text.encode_utf16().collect::<Vec<_>>();
        let primary_end = usize::try_from(primary.offset).unwrap()
            + usize::try_from(primary.length).unwrap();
        let provenance_start = usize::try_from(provenance.offset).unwrap();
        let provenance_end = provenance_start + usize::try_from(provenance.length).unwrap();
        assert!(primary.collapsed);
        assert!(!provenance.collapsed);
        assert!(primary_end <= provenance_start);
        assert_eq!(
            String::from_utf16(&units[provenance_start..provenance_end]).unwrap(),
            "Это встроенный модуль Lavis. Его нельзя выгрузить или заменить."
        );
    }

    #[tokio::test]
    async fn canonical_commands_precede_aliases_and_aliases_precede_modules() {
        let (aliases, directory) = aliases_with_core_alias().await;
        let canonical = render(&HelpRequest::Topic("help".to_owned()), "!", &aliases).response;
        assert!(canonical.text.starts_with("🛠 !help"));
        let alias = render(&HelpRequest::Topic("CORE".to_owned()), "!", &aliases).response;
        assert!(alias.text.starts_with("🔗 !CORE"));
        assert!(alias.text.contains("Целевой модуль: system"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn module_card_is_case_insensitive_and_has_deterministic_policy() {
        let response = render(&HelpRequest::Topic("SyStEm".to_owned()), "!", &aliases().await).response;
        assert!(response.text.contains("Безопасно ограниченная системная информация."));
        assert!(response.text.contains("Возможности: сведения о хосте, ограниченный процесс"));
        assert!(response.text.contains("выгрузка: запрещена; замена: запрещена"));
        assert!(response.text.ends_with("Это встроенный модуль Lavis. Его нельзя выгрузить или заменить."));
        assert_eq!(response.entities.len(), 2);
    }

    #[test]
    fn external_fixture_uses_external_provenance() {
        let fixture = ModuleSpec {
            id: ModuleId::Core,
            name: "fixture",
            description_ru: "Тестовый модуль.",
            icon: "🧪",
            origin: ModuleOrigin::External {
                author: "Автор",
                version: "1.0.0",
                source: "https://example.invalid/module",
            },
            capabilities: &[ModuleCapability::Network],
            unloadable: true,
            replaceable: true,
        };
        let response = render_module_card(&fixture, ",").response;
        assert!(response.text.contains("Внешний модуль. Автор: Автор; версия: 1.0.0"));
        assert!(response.text.contains("возможности: сеть"));
        assert!(!response.text.contains("Это встроенный модуль"));
    }

    #[test]
    fn invalid_external_fixture_never_renders_external_provenance() {
        let fixture = ModuleSpec {
            id: ModuleId::Core,
            name: "invalid",
            description_ru: "Тестовый модуль.",
            icon: "🧪",
            origin: ModuleOrigin::External {
                author: "Автор\n",
                version: "1.0.0",
                source: "https://example.invalid/module",
            },
            capabilities: &[ModuleCapability::Network],
            unloadable: true,
            replaceable: true,
        };
        let rendered = render_module_card(&fixture, ",");
        assert!(!rendered.entity_fallback);
        assert_eq!(rendered.response.entities.len(), 2);
        assert!(rendered.response.text.contains("Внешние данные модуля не отображаются."));
        assert!(rendered
            .response
            .text
            .contains("Метаданные происхождения отклонены ядром Lavis."));
        assert!(!rendered.response.text.contains("Внешний модуль"));
    }

    #[tokio::test]
    async fn modules_overview_matches_help_registry_counts() {
        let rendered = render_modules_overview(".");
        assert!(rendered.response.text.contains("Модули (3)"));
        assert!(rendered.response.text.contains("Команды (7)"));
        assert!(rendered.response.text.contains(".modules"));
        assert_eq!(rendered.response.entities.len(), 2);
        let grammers_client::tl::enums::MessageEntity::Blockquote(primary) =
            &rendered.response.entities[0]
        else {
            panic!("expected primary blockquote");
        };
        let grammers_client::tl::enums::MessageEntity::Blockquote(provenance) =
            &rendered.response.entities[1]
        else {
            panic!("expected provenance blockquote");
        };
        let units = rendered.response.text.encode_utf16().collect::<Vec<_>>();
        let primary_start = usize::try_from(primary.offset).unwrap();
        let primary_end = primary_start + usize::try_from(primary.length).unwrap();
        let provenance_start = usize::try_from(provenance.offset).unwrap();
        let provenance_end = provenance_start + usize::try_from(provenance.length).unwrap();
        assert!(primary.collapsed);
        assert!(!provenance.collapsed);
        assert!(primary_end <= provenance_start);
        assert_eq!(
            String::from_utf16(&units[provenance_start..provenance_end]).unwrap(),
            "Это встроенный модуль Lavis. Его нельзя выгрузить или заменить."
        );
    }
}
