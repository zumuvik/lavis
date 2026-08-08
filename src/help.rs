pub use crate::response::Response;

use crate::{
    aliases::AliasStore,
    commands::{
        CommandDefinition, CommandRisk, HelpRequest, canonical_command, command_by_name, commands,
        module_for_command,
    },
    external_modules::{manager::ExternalCommandRef, manifest::ExternalModuleDescriptor},
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
    render_with_external(request, prefix, aliases, &[], &[])
}

pub fn render_with_external(
    request: &HelpRequest,
    prefix: &str,
    aliases: &AliasStore,
    external_command_refs: &[ExternalCommandRef],
    external_descriptors: &[ExternalModuleDescriptor],
) -> RenderedHelp {
    match request {
        HelpRequest::Overview => {
            render_overview_with_external(prefix, external_descriptors, external_command_refs)
        }
        HelpRequest::Topic(topic) => render_topic(
            topic,
            prefix,
            aliases,
            external_command_refs,
            external_descriptors,
        ),
        HelpRequest::Invalid => plain(format!("⚠️ Использование: {prefix}help [команда]")),
    }
}

pub fn render_modules_overview(prefix: &str) -> RenderedHelp {
    render_modules_overview_with_external(prefix, &[], &[])
}

pub fn render_modules_overview_with_external(
    prefix: &str,
    external_descriptors: &[ExternalModuleDescriptor],
    external_command_refs: &[ExternalCommandRef],
) -> RenderedHelp {
    let mut module_parts: Vec<String> = Vec::new();
    for module in modules() {
        module_parts.push(format!("{} {}", module.icon, module.name));
    }
    for desc in external_descriptors {
        let has_active = external_command_refs.iter().any(|r| r.module_id == desc.id);
        if has_active {
            module_parts.push(format!("📦 {} ({})", desc.display_name, desc.id));
        }
    }

    let mut cmd_names: Vec<String> = Vec::new();
    for command in commands() {
        cmd_names.push(format!("{prefix}{}", command.name));
    }
    for ref_ in external_command_refs {
        cmd_names.push(format!("{prefix}{}.{}", ref_.module_id, ref_.command_name));
    }

    let total = modules().len()
        + external_descriptors
            .iter()
            .filter(|d| external_command_refs.iter().any(|r| r.module_id == d.id))
            .count();
    let cmd_total = commands().len() + external_command_refs.len();

    let heading = format!("🧩 Модули Lavis: {total}");
    let primary = format!(
        "Модули: {}\nКоманды ({cmd_total}): {}\n\nИспользуйте {prefix}help <команда или модуль> для подробностей.",
        module_parts.join(", "),
        cmd_names.join(", "),
    );

    documentation(heading, primary, core_provenance())
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

fn render_overview_with_external(
    prefix: &str,
    external_descriptors: &[ExternalModuleDescriptor],
    external_command_refs: &[ExternalCommandRef],
) -> RenderedHelp {
    let mut body: Vec<String> = modules()
        .iter()
        .map(|module| {
            let names = commands_for_module(module.id)
                .map(|command| format!("{prefix}{}", command.name))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} {}: {names}", module.icon, module.name)
        })
        .collect();
    for desc in external_descriptors {
        let cmd_names: Vec<String> = external_command_refs
            .iter()
            .filter(|r| r.module_id == desc.id)
            .map(|r| format!("{prefix}{}.{}", r.module_id, r.command_name))
            .collect();
        if !cmd_names.is_empty() {
            body.push(format!(
                "📦 {} ({}): {}",
                desc.display_name,
                desc.id,
                cmd_names.join(", ")
            ));
        }
    }
    let total_modules = modules().len()
        + external_descriptors
            .iter()
            .filter(|d| external_command_refs.iter().any(|r| r.module_id == d.id))
            .count();
    let total_commands = commands().len() + external_command_refs.len();
    documentation(
        format!(
            "🛠 Справка Lavis: {} модулей, {} команд",
            total_modules, total_commands
        ),
        format!(
            "{}\n\nИспользуйте {prefix}help <команда или модуль> для подробностей.",
            body.join("\n")
        ),
        core_provenance(),
    )
}

fn render_topic(
    topic: &str,
    prefix: &str,
    aliases: &AliasStore,
    external_command_refs: &[ExternalCommandRef],
    external_descriptors: &[ExternalModuleDescriptor],
) -> RenderedHelp {
    // 1. Built-in canonical command
    if let Some(command) = canonical_command(topic) {
        return render_command_card(command, prefix);
    }
    // 2. Active external namespaced command
    if topic.contains('.')
        && let Some(rendered) = render_external_namespaced_command(
            topic,
            prefix,
            external_command_refs,
            external_descriptors,
        )
    {
        return rendered;
    }
    // 3. Alias
    if let Some(rendered) = render_alias(topic, prefix, aliases) {
        return rendered;
    }
    // 4. Built-in module
    if let Some(module) = module_by_name(topic) {
        return render_module_card(module, prefix);
    }
    // 5. Active external module
    if let Some(rendered) =
        render_external_module_card(topic, prefix, external_descriptors, external_command_refs)
    {
        return rendered;
    }
    // 6. Unknown
    plain(format!(
        "❓ Неизвестная команда или модуль: {topic}\nИспользуйте {prefix}help для списка команд."
    ))
}

fn render_external_namespaced_command(
    dotted: &str,
    prefix: &str,
    external_command_refs: &[ExternalCommandRef],
    external_descriptors: &[ExternalModuleDescriptor],
) -> Option<RenderedHelp> {
    let dot = dotted.find('.')?;
    let module_id = &dotted[..dot];
    let command_name = &dotted[dot + 1..];

    let desc = external_descriptors.iter().find(|d| d.id == module_id)?;
    let cmd = desc.commands.iter().find(|c| c.name == command_name)?;
    let ref_ = external_command_refs
        .iter()
        .find(|r| r.module_id == module_id && r.command_name == command_name)?;

    let examples: Vec<String> = cmd
        .examples
        .iter()
        .map(|ex| format!("{prefix}{}.{} {}", module_id, command_name, ex))
        .collect();

    let primary = format!(
        "{}\n\nИспользование: {prefix}{}.{} {}\nМодуль: {} v{}\nАвтор: {}\nВозможности: {}\nРиск: внешний код, запускается без песочницы\n\nПримеры:\n{}",
        ref_.description_ru,
        module_id,
        command_name,
        cmd.usage,
        desc.display_name,
        desc.version,
        desc.author,
        desc.capabilities
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        examples.join("\n"),
    );

    Some(documentation(
        format!("🔌 {prefix}{}", dotted),
        primary,
        external_provenance(),
    ))
}

fn render_external_module_card(
    id: &str,
    prefix: &str,
    external_descriptors: &[ExternalModuleDescriptor],
    external_command_refs: &[ExternalCommandRef],
) -> Option<RenderedHelp> {
    let desc = external_descriptors.iter().find(|d| d.id == id)?;

    let mut cmd_lines: Vec<String> = Vec::new();
    for cmd in &desc.commands {
        let active = external_command_refs
            .iter()
            .any(|r| r.module_id == desc.id && r.command_name == cmd.name);
        if active {
            cmd_lines.push(format!(
                "{prefix}{}.{} — {}",
                desc.id, cmd.name, cmd.summary_ru
            ));
        }
    }

    let cap_strs: Vec<&str> = desc.capabilities.iter().map(|c| c.as_str()).collect();

    let has_active = external_command_refs.iter().any(|r| r.module_id == desc.id);
    let active = if has_active {
        "активен"
    } else {
        "не активен"
    };

    let primary = format!(
        "id: {}\nавтор: {} v{}\nстатус: {active}\nкоманд: {}\n\nКоманды:\n{}\n\nВозможности: {}\n\n⚠️ Внешний модуль запускается отдельным процессом, но не помещается в системную песочницу. Включайте только код, которому доверяете.",
        desc.id,
        desc.author,
        desc.version,
        desc.commands.len(),
        cmd_lines.join("\n"),
        cap_strs.join(", "),
    );

    Some(documentation(
        format!("📦 Модуль {}", desc.display_name),
        primary,
        external_provenance(),
    ))
}

fn render_command_card(command: &CommandDefinition, prefix: &str) -> RenderedHelp {
    let Some(module) = module_for_command(command) else {
        return plain(format!(
            "⚠️ Метаданные команды {prefix}{} недоступны",
            command.name
        ));
    };
    let primary = if command.name == "fastfetch" {
        fastfetch_primary(prefix, command, module.name)
    } else if command.name == "alias" {
        alias_primary(prefix, command, module.name)
    } else if command.name == "lm" {
        lm_primary(prefix, command, module.name)
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
        "{}\n\nИспользование: {prefix}{}\nМодуль: {module_name}\nРиск: {}\n\nПримеры:\n{prefix}fastfetch --logo NixOS\n{prefix}fastfetch --logo-padding-right 3\n{prefix}fastfetch --separator \" -> \"\n{prefix}fastfetch --structure OS:Kernel:CPU\n\nЛоготипы: none, Alpine, Arch, Debian, Fedora, FreeBSD, Linux, MacOS, NixOS, OpenBSD, Ubuntu, Windows.\nСтруктура: title, separator, os, kernel, uptime, cpu, memory, gpu, packages, shell, terminal, terminalsize, host, display, wm, de, theme, icons, font, cursor, disk, swap, localip, battery, poweradapter, locale.\nРазделитель: 1–64 печатных ASCII-символа.\nОтступ логотипа: --logo-padding-left <n>, --logo-padding-right <n>, --logo-padding-top <n>; 0–32.\n\nПоля профиля: logo_padding_left, logo_padding_right, logo_padding_top (0–32, целые числа).\n\n{prefix}fastfetch --no-profile не читает профиль. Профиль: $XDG_CONFIG_HOME/lavis/fastfetch.json или $HOME/.config/lavis/fastfetch.json.\nМинимальный JSON: {{ \"version\": 1 }}\nПриоритет: значения Fastfetch по умолчанию < профиль < параметры команды.\nПсевдоним: {prefix}alias add sys fastfetch --logo arch; затем {prefix}sys.\n\nКавычки группируют аргументы для разбора shell-words; оболочка не запускается, а shell-метасимволы остаются данными. Каждый процесс запускается только с --config none --pipe; нативные конфиги и пресеты Fastfetch запрещены. Вывод может раскрыть данные хоста, сети, дисплея, питания и оборудования.",
        command.description_ru,
        command.usage,
        risk_label(command.risk)
    )
}

fn alias_primary(prefix: &str, command: &CommandDefinition, module_name: &str) -> String {
    format!(
        "{}\n\nИспользование: {prefix}{}\nМодуль: {module_name}\nРиск: {}\n\nПримеры:\n{prefix}alias list\n{prefix}alias add sys fastfetch --logo arch\n{prefix}alias show sys\n{prefix}alias del sys\n\nПсевдонимы позволяют вызывать канонические команды под другим именем с заранее заданными аргументами. Канонические команды имеют приоритет над псевдонимами: псевдоним не может переопределить встроенную команду с тем же именем. Псевдонимы постоянны и сохраняются между сессиями.",
        command.description_ru,
        command.usage,
        risk_label(command.risk)
    )
}

fn lm_primary(prefix: &str, command: &CommandDefinition, module_name: &str) -> String {
    format!(
        "{}\n\nИспользование: {prefix}{}\nМодуль: {module_name}\nРиск: {}\n\n{prefix}lm list — список модулей; {prefix}lm info <id> — сведения; {prefix}lm logs <id> — последняя runtime-ошибка; {prefix}lm doctor [<id>] — диагностика состояния модулей. В Saved Messages прикрепите .lmod и отправьте {prefix}lm install: код не запускается, показывается inspection-план.\nПроверьте план. Подтвердите полный ApprovalId: {prefix}lm confirm <approval-id>; отмена: {prefix}lm cancel <approval-id>.\n\n{prefix}lm enable <id> и {prefix}lm disable <id> изменяют состояние только для следующего перезапуска.\n\nApprovalId — одноразовый Crockford Base32 идентификатор XXXX-XXXX-XXXX-XXXX, действует ровно 10 минут и не может быть использовано повторно.\n\nПосле установки модуль остаётся disabled и не запускается автоматически. ⚠️ Внешний модуль — исполняемый код без системной песочницы.",
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
            "Псевдонимы позволяют вызывать канонические команды под другим именем с заранее заданными аргументами. Канонические команды имеют приоритет над псевдонимами: псевдоним не может переопределить встроенную команду с тем же именем.\n\nПример: {prefix}alias add sys fastfetch --logo arch; затем {prefix}sys.\nСохранённые аргументы: {stored}.\n\n{prefix}{} вызывает {prefix}{}; сохранённые аргументы объединяются с аргументами вызова.\nЦелевой модуль: {}\nРиск цели: {}",
            topic,
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
        if module.unloadable {
            "разрешена"
        } else {
            "запрещена"
        },
        if module.replaceable {
            "разрешена"
        } else {
            "запрещена"
        }
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

fn external_provenance() -> String {
    "Внешний модуль. Код запускается вне песочницы; доверяйте только проверенным модулям."
        .to_owned()
}

fn risk_label(risk: CommandRisk) -> &'static str {
    match risk {
        CommandRisk::ReadOnly => "только чтение",
        CommandRisk::PersistentStateChange => "изменение постоянного состояния",
        CommandRisk::RestrictedProcess => "ограниченный процесс",
        CommandRisk::ArbitraryProcess => "произвольный процесс",
        CommandRisk::Privileged => "привилегированная операция",
        CommandRisk::ExternalCodeInstall => "установка внешнего кода",
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
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    async fn aliases() -> AliasStore {
        AliasStore::load(PathBuf::from("/nonexistent/lavis-help-aliases.json"))
            .await
            .unwrap()
    }

    async fn aliases_with_core_alias() -> (AliasStore, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!("lavis-help-{nonce}-{seq}"));
        fs::create_dir_all(&directory).unwrap();
        let mut aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
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
        assert!(
            response
                .text
                .starts_with("🛠 Справка Lavis: 3 модулей, 10 команд")
        );
        assert!(response.text.find("🧩 core").unwrap() < response.text.find("🖥 system").unwrap());
        assert!(response.text.contains("🦀fastfetch"));
        assert!(
            response
                .text
                .ends_with("Это встроенный модуль Lavis. Его нельзя выгрузить или заменить.")
        );
        assert_eq!(response.entities.len(), 2);
    }

    #[tokio::test]
    async fn command_cards_use_documentation_entities_and_symbolic_fastfetch_paths() {
        let response = render(
            &HelpRequest::Topic("fastfetch".to_owned()),
            "🦀",
            &aliases().await,
        )
        .response;
        assert_eq!(response.entities.len(), 2);
        assert!(
            response
                .text
                .contains("$XDG_CONFIG_HOME/lavis/fastfetch.json")
        );
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
        let grammers_client::tl::enums::MessageEntity::Blockquote(provenance) =
            &response.entities[1]
        else {
            panic!("expected provenance blockquote");
        };
        let units = response.text.encode_utf16().collect::<Vec<_>>();
        let primary_end =
            usize::try_from(primary.offset).unwrap() + usize::try_from(primary.length).unwrap();
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
        let response = render(
            &HelpRequest::Topic("SyStEm".to_owned()),
            "!",
            &aliases().await,
        )
        .response;
        assert!(
            response
                .text
                .contains("Безопасно ограниченная системная информация.")
        );
        assert!(
            response
                .text
                .contains("Возможности: сведения о хосте, ограниченный процесс")
        );
        assert!(
            response
                .text
                .contains("выгрузка: запрещена; замена: запрещена")
        );
        assert!(
            response
                .text
                .ends_with("Это встроенный модуль Lavis. Его нельзя выгрузить или заменить.")
        );
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
        assert!(
            response
                .text
                .contains("Внешний модуль. Автор: Автор; версия: 1.0.0")
        );
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
        assert!(
            rendered
                .response
                .text
                .contains("Внешние данные модуля не отображаются.")
        );
        assert!(
            rendered
                .response
                .text
                .contains("Метаданные происхождения отклонены ядром Lavis.")
        );
        assert!(!rendered.response.text.contains("Внешний модуль"));
    }

    #[tokio::test]
    async fn modules_overview_matches_help_registry_counts() {
        let rendered = render_modules_overview(".");
        assert!(rendered.response.text.contains("Модули: "));
        assert!(rendered.response.text.contains("Команды (10)"));
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

    #[tokio::test]
    async fn user_created_alias_help_uses_active_prefix_and_explains_canonical_priority() {
        let (aliases, directory) = aliases_with_core_alias().await;
        let response = render(&HelpRequest::Topic("core".to_owned()), "🦀", &aliases).response;
        assert!(response.text.starts_with("🔗 🦀core"));
        assert!(
            response
                .text
                .contains("Псевдонимы позволяют вызывать канонические команды под другим именем")
        );
        assert!(
            response
                .text
                .contains("псевдоним не может переопределить встроенную команду")
        );
        assert!(
            response
                .text
                .contains("🦀alias add sys fastfetch --logo arch")
        );
        assert!(response.text.contains("🦀sys"));
        assert!(!response.text.contains(",alias"));
        assert!(response.text.contains("🦀fastfetch"));
        assert!(!response.text.contains("/home/"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn canonical_alias_help_has_usage_section_and_active_prefix() {
        let response = render(
            &HelpRequest::Topic("alias".to_owned()),
            "🦀",
            &aliases().await,
        )
        .response;
        assert!(response.text.starts_with("🔗 🦀alias"));
        assert!(response.text.contains("🦀alias list"));
        assert!(
            response
                .text
                .contains("🦀alias add sys fastfetch --logo arch")
        );
        assert!(response.text.contains("🦀alias show sys"));
        assert!(response.text.contains("🦀alias del sys"));
        assert!(
            response
                .text
                .contains("псевдоним не может переопределить встроенную команду")
        );
        assert!(
            response
                .text
                .contains("Псевдонимы позволяют вызывать канонические команды")
        );
        assert!(!response.text.contains(",alias"));
        assert!(!response.text.contains("/home/"));
        assert_eq!(response.entities.len(), 2);
        let grammers_client::tl::enums::MessageEntity::Blockquote(primary) = &response.entities[0]
        else {
            panic!("expected primary blockquote");
        };
        let grammers_client::tl::enums::MessageEntity::Blockquote(provenance) =
            &response.entities[1]
        else {
            panic!("expected provenance blockquote");
        };
        let units = response.text.encode_utf16().collect::<Vec<_>>();
        let primary_end =
            usize::try_from(primary.offset).unwrap() + usize::try_from(primary.length).unwrap();
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
    async fn setup_help_uses_the_active_prefix() {
        let response = render(
            &HelpRequest::Topic("setup".to_owned()),
            "🦀",
            &aliases().await,
        )
        .response;
        assert!(response.text.starts_with("🛠 🦀setup"));
        assert!(response.text.contains("🦀setup lavis_example_bot"));
        assert!(response.text.contains("Риск: привилегированная операция"));
    }

    #[tokio::test]
    async fn lm_help_describes_the_full_review_and_confirmation_flow() {
        let response =
            render(&HelpRequest::Topic("lm".to_owned()), "🦀", &aliases().await).response;

        assert!(response.text.starts_with("📦 🦀lm"));
        assert!(response.text.contains("🦀lm list"));
        assert!(response.text.contains("🦀lm info <id>"));
        assert!(response.text.contains("🦀lm enable <id>"));
        assert!(response.text.contains("🦀lm disable <id>"));
        assert!(response.text.contains("🦀lm install"));
        assert!(!response.text.contains("🦀lm install <source>"));
        assert!(response.text.contains("🦀lm confirm <approval-id>"));
        assert!(response.text.contains("🦀lm cancel <approval-id>"));
        assert!(response.text.contains("Saved Messages"));
        assert!(response.text.contains(".lmod"));
        assert!(response.text.contains("inspection"));
        assert!(response.text.contains("ApprovalId"));
        assert!(response.text.contains("XXXX-XXXX-XXXX-XXXX"));
        assert!(response.text.contains("ровно 10 минут"));
        assert!(response.text.contains("disabled"));
        assert!(response.text.contains("не запускается автоматически"));
        assert!(
            response
                .text
                .contains("исполняемый код без системной песочницы")
        );
        assert!(
            response
                .text
                .contains("не может быть использовано повторно")
        );
        assert!(response.text.contains("Риск: установка внешнего кода"));
    }

    #[tokio::test]
    async fn fastfetch_help_has_compact_examples_with_active_prefix() {
        let response = render(
            &HelpRequest::Topic("fastfetch".to_owned()),
            "🦀",
            &aliases().await,
        )
        .response;
        assert!(response.text.contains("🦀fastfetch --logo NixOS"));
        assert!(response.text.contains("🦀fastfetch --logo-padding-right 3"));
        assert!(response.text.contains("🦀fastfetch --separator \" -> \""));
        assert!(
            response
                .text
                .contains("🦀fastfetch --structure OS:Kernel:CPU")
        );
        assert!(!response.text.contains("/home/"));
        assert!(response.text.contains("🦀fastfetch --no-profile"));
    }

    #[tokio::test]
    async fn fastfetch_help_preserves_security_and_path_documentation() {
        let response = render(
            &HelpRequest::Topic("fastfetch".to_owned()),
            "🦀",
            &aliases().await,
        )
        .response;
        assert!(
            response
                .text
                .contains("$XDG_CONFIG_HOME/lavis/fastfetch.json")
        );
        assert!(response.text.contains("--config none"));
        assert!(response.text.contains("shell-words"));
        assert!(response.text.contains("shell-метасимволы остаются данными"));
        assert!(!response.text.contains("/tmp/"));
    }
}
