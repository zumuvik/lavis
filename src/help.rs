pub use crate::response::Response;

use crate::{
    aliases::AliasStore,
    commands::{
        CommandDefinition, CommandRisk, HelpRequest, canonical_command, command_by_name,
        command_description as command_text_description, command_summary as command_text_summary,
        commands, module_for_command,
    },
    external_modules::{manager::ExternalCommandRef, manifest::ExternalModuleDescriptor},
    i18n::Locale,
    modules::{
        ModuleCapability, ModuleOrigin, ModuleSpec, commands_for_module, module_by_name,
        module_description, modules, validate_external_origin,
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
    render_with_external_locale(
        request,
        prefix,
        aliases,
        external_command_refs,
        external_descriptors,
        Locale::Russian,
    )
}

/// Built-in documentation is owned by Lavis and is therefore localized here.
/// Third-party manifest text remains its supplied Russian fallback.
pub fn render_with_external_locale(
    request: &HelpRequest,
    prefix: &str,
    aliases: &AliasStore,
    external_command_refs: &[ExternalCommandRef],
    external_descriptors: &[ExternalModuleDescriptor],
    locale: Locale,
) -> RenderedHelp {
    match request {
        HelpRequest::Overview => render_overview_with_external(
            prefix,
            aliases,
            external_descriptors,
            external_command_refs,
            locale,
        ),
        HelpRequest::Topic(topic) => render_topic(
            topic,
            prefix,
            aliases,
            external_command_refs,
            external_descriptors,
            locale,
        ),
        HelpRequest::Invalid => plain(locale, invalid_help_usage(locale, prefix)),
    }
}

fn english_risk(risk: CommandRisk) -> &'static str {
    match risk {
        CommandRisk::ReadOnly => "read only",
        CommandRisk::PersistentStateChange => "persistent state change",
        CommandRisk::RestrictedProcess => "restricted process",
        CommandRisk::ArbitraryProcess => "arbitrary process",
        CommandRisk::Privileged => "privileged operation",
        CommandRisk::ExternalCodeInstall => "external code installation",
    }
}

pub fn render_modules_overview(prefix: &str) -> RenderedHelp {
    render_modules_overview_with_external_locale(prefix, &[], &[], Locale::Russian)
}

pub fn render_modules_invalid_usage(prefix: &str, locale: Locale) -> RenderedHelp {
    plain(
        locale,
        match locale {
            Locale::English => format!("⚠️ Usage: {prefix}modules"),
            Locale::Russian => format!("⚠️ Использование: {prefix}modules"),
        },
    )
}

pub fn render_modules_overview_with_external(
    prefix: &str,
    external_descriptors: &[ExternalModuleDescriptor],
    external_command_refs: &[ExternalCommandRef],
) -> RenderedHelp {
    render_modules_overview_with_external_locale(
        prefix,
        external_descriptors,
        external_command_refs,
        Locale::Russian,
    )
}

pub fn render_modules_overview_with_external_locale(
    prefix: &str,
    external_descriptors: &[ExternalModuleDescriptor],
    external_command_refs: &[ExternalCommandRef],
    locale: Locale,
) -> RenderedHelp {
    let mut module_parts: Vec<String> = Vec::new();
    for module in modules() {
        module_parts.push(format!(
            "{} {} — {}",
            module.icon,
            module.name,
            module_description(module.id, locale)
        ));
    }
    for desc in external_descriptors {
        let has_active = external_command_refs.iter().any(|r| r.module_id == desc.id);
        if has_active {
            module_parts.push(format!("📦 {} ({})", desc.display_name, desc.id));
        }
    }

    let mut cmd_names: Vec<String> = Vec::new();
    for command in commands() {
        cmd_names.push(format!(
            "{prefix}{} — {}",
            command.name,
            command_summary(command, locale)
        ));
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

    let heading = match locale {
        Locale::English => format!("🧩 Lavis modules: {total}"),
        Locale::Russian => format!("🧩 Модули Lavis: {total}"),
    };
    let primary = match locale {
        Locale::English => format!(
            "Modules: {}\nCommands ({cmd_total}): {}\n\nUse {prefix}help <command or module> for details.",
            module_parts.join(", "),
            cmd_names.join(", "),
        ),
        Locale::Russian => format!(
            "Модули: {}\nКоманды ({cmd_total}): {}\n\nИспользуйте {prefix}help <команда или модуль> для подробностей.",
            module_parts.join(", "),
            cmd_names.join(", "),
        ),
    };

    documentation(locale, heading, primary, core_provenance(locale))
}

#[cfg(test)]
fn render_module_card(module: &ModuleSpec, prefix: &str) -> RenderedHelp {
    render_module_card_locale(module, prefix, Locale::Russian)
}

fn render_module_card_locale(module: &ModuleSpec, prefix: &str, locale: Locale) -> RenderedHelp {
    if !validate_external_origin(&module.origin) {
        return documentation(
            locale,
            invalid_module_heading(locale).to_owned(),
            invalid_module_primary(locale).to_owned(),
            invalid_module_provenance(locale).to_owned(),
        );
    }
    let command_list = commands_for_module(module.id)
        .map(|command| {
            format!(
                "{prefix}{} — {}",
                command.usage,
                command_summary(command, locale)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let primary = module_primary(module, &command_list, locale);
    documentation(
        locale,
        module_heading(module, locale),
        primary,
        module_provenance(module, locale),
    )
}

fn render_overview_with_external(
    prefix: &str,
    aliases: &AliasStore,
    external_descriptors: &[ExternalModuleDescriptor],
    external_command_refs: &[ExternalCommandRef],
    locale: Locale,
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
    if !aliases.aliases().is_empty() {
        body.push(format!(
            "{}{}",
            aliases_overview_label(locale),
            aliases
                .aliases()
                .keys()
                .map(|name| format!("{prefix}{name}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let total_modules = modules().len()
        + external_descriptors
            .iter()
            .filter(|d| external_command_refs.iter().any(|r| r.module_id == d.id))
            .count();
    let total_commands = commands().len() + external_command_refs.len() + aliases.aliases().len();
    documentation(
        locale,
        overview_heading(locale, total_modules, total_commands),
        format!(
            "{}\n\n{}",
            body.join("\n"),
            overview_instruction(locale, prefix)
        ),
        core_provenance(locale),
    )
}

fn render_topic(
    topic: &str,
    prefix: &str,
    aliases: &AliasStore,
    external_command_refs: &[ExternalCommandRef],
    external_descriptors: &[ExternalModuleDescriptor],
    locale: Locale,
) -> RenderedHelp {
    // 1. Built-in canonical command
    if let Some(command) = canonical_command(topic) {
        return render_command_card(command, prefix, locale);
    }
    // 2. Active external namespaced command
    if topic.contains('.')
        && let Some(rendered) = render_external_namespaced_command(
            topic,
            prefix,
            external_command_refs,
            external_descriptors,
            locale,
        )
    {
        return rendered;
    }
    // 3. Alias
    if let Some(rendered) = render_alias(topic, prefix, aliases, locale) {
        return rendered;
    }
    // 4. Built-in module
    if let Some(module) = module_by_name(topic) {
        return render_module_card_locale(module, prefix, locale);
    }
    // 5. Active external module
    if let Some(rendered) = render_external_module_card(
        topic,
        prefix,
        external_descriptors,
        external_command_refs,
        locale,
    ) {
        return rendered;
    }
    // 6. Unknown
    plain(locale, unknown_topic(locale, topic, prefix))
}

fn render_external_namespaced_command(
    dotted: &str,
    prefix: &str,
    external_command_refs: &[ExternalCommandRef],
    external_descriptors: &[ExternalModuleDescriptor],
    locale: Locale,
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
        "{}\n\n{}: {prefix}{}.{} {}\n{}: {} v{}\n{}: {}\n{}: {}\n{}: {}\n\n{}:\n{}",
        ref_.description_ru,
        usage_label(locale),
        module_id,
        command_name,
        cmd.usage,
        module_label(locale),
        desc.display_name,
        desc.version,
        author_label(locale),
        desc.author,
        capabilities_label(locale),
        desc.capabilities
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        risk_label_for_external(locale),
        external_risk(locale),
        examples_label(locale),
        examples.join("\n"),
    );

    Some(documentation(
        locale,
        format!("🔌 {prefix}{}", dotted),
        primary,
        external_provenance(locale),
    ))
}

fn render_external_module_card(
    id: &str,
    prefix: &str,
    external_descriptors: &[ExternalModuleDescriptor],
    external_command_refs: &[ExternalCommandRef],
    locale: Locale,
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
    let active = external_status(locale, has_active);

    let primary = format!(
        "id: {}\n{}: {} v{}\n{}: {active}\n{}: {}\n\n{}:\n{}\n\n{}: {}\n\n{}",
        desc.id,
        author_label(locale),
        desc.author,
        desc.version,
        status_label(locale),
        command_count_label(locale),
        desc.commands.len(),
        commands_label(locale),
        cmd_lines.join("\n"),
        capabilities_label(locale),
        cap_strs.join(", "),
        external_module_warning(locale),
    );

    Some(documentation(
        locale,
        external_module_heading(locale, &desc.display_name),
        primary,
        external_provenance(locale),
    ))
}

fn render_command_card(command: &CommandDefinition, prefix: &str, locale: Locale) -> RenderedHelp {
    let Some(module) = module_for_command(command) else {
        return plain(
            locale,
            command_metadata_unavailable(locale, prefix, command.name),
        );
    };
    let primary = if command.name == "fastfetch" {
        fastfetch_primary(prefix, command, module.name, locale)
    } else if command.name == "alias" {
        alias_primary(prefix, command, module.name, locale)
    } else if command.name == "lm" {
        lm_primary(prefix, command, module.name, locale)
    } else {
        generic_command_primary(command, prefix, module.name, locale)
    };
    documentation(
        locale,
        format!("{} {prefix}{}", command.icon, command.usage),
        primary,
        module_provenance(module, locale),
    )
}

fn generic_command_primary(
    command: &CommandDefinition,
    prefix: &str,
    module_name: &str,
    locale: Locale,
) -> String {
    let examples = command
        .examples
        .iter()
        .map(|example| format!("{prefix}{example}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\n\n{}: {prefix}{}\n{}: {module_name}\n{}: {}\n{}:\n{examples}",
        command_description(command, locale),
        usage_label(locale),
        command.usage,
        module_label(locale),
        risk_label_for_external(locale),
        risk_label(command.risk, locale),
        examples_label(locale),
    )
}

fn fastfetch_primary(
    prefix: &str,
    command: &CommandDefinition,
    module_name: &str,
    locale: Locale,
) -> String {
    if locale == Locale::English {
        return format!(
            "{}\n\nUsage: {prefix}{}\nModule: {module_name}\nRisk: {}\n\nExamples:\n{prefix}fastfetch --logo NixOS\n{prefix}fastfetch --logo-padding-right 3\n{prefix}fastfetch --separator \" -> \"\n{prefix}fastfetch --structure OS:Kernel:CPU\n\nLogos: none, Alpine, Arch, Debian, Fedora, FreeBSD, Linux, MacOS, NixOS, OpenBSD, Ubuntu, Windows.\nStructure: title, separator, os, kernel, uptime, cpu, memory, gpu, packages, shell, terminal, terminalsize, host, display, wm, de, theme, icons, font, cursor, disk, swap, localip, battery, poweradapter, locale.\nSeparator: 1–64 printable ASCII characters.\nLogo padding: --logo-padding-left <n>, --logo-padding-right <n>, --logo-padding-top <n>; 0–32.\n\nProfile fields: logo_padding_left, logo_padding_right, logo_padding_top (integers from 0–32).\n\n{prefix}fastfetch --no-profile does not read the profile. Profile: $XDG_CONFIG_HOME/lavis/fastfetch.json or $HOME/.config/lavis/fastfetch.json.\nMinimal JSON: {{ \"version\": 1 }}\nPrecedence: Fastfetch defaults < profile < command options.\nAlias: {prefix}alias add sys fastfetch --logo arch; then {prefix}sys.\n\nQuotes group arguments for shell-words parsing; no shell is run and shell metacharacters remain data. Each process runs only with --config none --pipe; native Fastfetch configurations and presets are prohibited. Output may reveal host, network, display, power, and hardware data.",
            command_description(command, locale),
            command.usage,
            risk_label(command.risk, locale)
        );
    }
    format!(
        "{}\n\nИспользование: {prefix}{}\nМодуль: {module_name}\nРиск: {}\n\nПримеры:\n{prefix}fastfetch --logo NixOS\n{prefix}fastfetch --logo-padding-right 3\n{prefix}fastfetch --separator \" -> \"\n{prefix}fastfetch --structure OS:Kernel:CPU\n\nЛоготипы: none, Alpine, Arch, Debian, Fedora, FreeBSD, Linux, MacOS, NixOS, OpenBSD, Ubuntu, Windows.\nСтруктура: title, separator, os, kernel, uptime, cpu, memory, gpu, packages, shell, terminal, terminalsize, host, display, wm, de, theme, icons, font, cursor, disk, swap, localip, battery, poweradapter, locale.\nРазделитель: 1–64 печатных ASCII-символа.\nОтступ логотипа: --logo-padding-left <n>, --logo-padding-right <n>, --logo-padding-top <n>; 0–32.\n\nПоля профиля: logo_padding_left, logo_padding_right, logo_padding_top (0–32, целые числа).\n\n{prefix}fastfetch --no-profile не читает профиль. Профиль: $XDG_CONFIG_HOME/lavis/fastfetch.json или $HOME/.config/lavis/fastfetch.json.\nМинимальный JSON: {{ \"version\": 1 }}\nПриоритет: значения Fastfetch по умолчанию < профиль < параметры команды.\nПсевдоним: {prefix}alias add sys fastfetch --logo arch; затем {prefix}sys.\n\nКавычки группируют аргументы для разбора shell-words; оболочка не запускается, а shell-метасимволы остаются данными. Каждый процесс запускается только с --config none --pipe; нативные конфиги и пресеты Fastfetch запрещены. Вывод может раскрыть данные хоста, сети, дисплея, питания и оборудования.",
        command_description(command, locale),
        command.usage,
        risk_label(command.risk, locale)
    )
}

fn alias_primary(
    prefix: &str,
    command: &CommandDefinition,
    module_name: &str,
    locale: Locale,
) -> String {
    if locale == Locale::English {
        return format!(
            "{}\n\nUsage: {prefix}{}\nModule: {module_name}\nRisk: {}\n\nExamples:\n{prefix}alias list\n{prefix}alias add sys fastfetch --logo arch\n{prefix}alias show sys\n{prefix}alias del sys\n\nAliases invoke canonical commands under another name with stored arguments. Canonical commands take priority over aliases: an alias cannot override a built-in command with the same name. Aliases are persistent and survive sessions.",
            command_description(command, locale),
            command.usage,
            risk_label(command.risk, locale)
        );
    }
    format!(
        "{}\n\nИспользование: {prefix}{}\nМодуль: {module_name}\nРиск: {}\n\nПримеры:\n{prefix}alias list\n{prefix}alias add sys fastfetch --logo arch\n{prefix}alias show sys\n{prefix}alias del sys\n\nПсевдонимы позволяют вызывать канонические команды под другим именем с заранее заданными аргументами. Канонические команды имеют приоритет над псевдонимами: псевдоним не может переопределить встроенную команду с тем же именем. Псевдонимы постоянны и сохраняются между сессиями.",
        command_description(command, locale),
        command.usage,
        risk_label(command.risk, locale)
    )
}

fn lm_primary(
    prefix: &str,
    command: &CommandDefinition,
    module_name: &str,
    locale: Locale,
) -> String {
    if locale == Locale::English {
        return format!(
            "{}\n\nUsage: {prefix}{}\nModule: {module_name}\nRisk: {}\n\n{prefix}lm list — list modules; {prefix}lm info <id> — details; {prefix}lm logs <id> — most recent runtime error; {prefix}lm doctor [<id>] — diagnose module state. In Saved Messages, attach a .lmod and send {prefix}lm install: code does not run; an inspection plan is shown.\nReview the plan. Confirm the full ApprovalId: {prefix}lm confirm <approval-id>; cancel: {prefix}lm cancel <approval-id>.\n\n{prefix}lm enable <id> and {prefix}lm disable <id> change state only for the next restart.\n\nApprovalId is a one-time Crockford Base32 identifier XXXX-XXXX-XXXX-XXXX, valid for exactly 10 minutes and cannot be reused.\n\nAfter installation, a module remains disabled and does not start automatically. ⚠️ An external module is executable code without a system sandbox.",
            command_description(command, locale),
            command.usage,
            risk_label(command.risk, locale)
        );
    }
    format!(
        "{}\n\nИспользование: {prefix}{}\nМодуль: {module_name}\nРиск: {}\n\n{prefix}lm list — список модулей; {prefix}lm info <id> — сведения; {prefix}lm logs <id> — последняя runtime-ошибка; {prefix}lm doctor [<id>] — диагностика состояния модулей. В Saved Messages прикрепите .lmod и отправьте {prefix}lm install: код не запускается, показывается inspection-план.\nПроверьте план. Подтвердите полный ApprovalId: {prefix}lm confirm <approval-id>; отмена: {prefix}lm cancel <approval-id>.\n\n{prefix}lm enable <id> и {prefix}lm disable <id> изменяют состояние только для следующего перезапуска.\n\nApprovalId — одноразовый Crockford Base32 идентификатор XXXX-XXXX-XXXX-XXXX, действует ровно 10 минут и не может быть использовано повторно.\n\nПосле установки модуль остаётся disabled и не запускается автоматически. ⚠️ Внешний модуль — исполняемый код без системной песочницы.",
        command_description(command, locale),
        command.usage,
        risk_label(command.risk, locale)
    )
}

fn render_alias(
    topic: &str,
    prefix: &str,
    aliases: &AliasStore,
    locale: Locale,
) -> Option<RenderedHelp> {
    let alias = aliases.lookup(topic)?;
    let command = command_by_name(&alias.target)?;
    let module = module_for_command(command)?;
    let stored = if alias.args.is_empty() {
        no_stored_arguments(locale).to_owned()
    } else {
        shell_words::join(&alias.args)
    };
    Some(documentation(
        locale,
        format!("🔗 {prefix}{topic}"),
        alias_primary_for_topic(topic, prefix, command, module.name, &stored, locale),
        module_provenance(module, locale),
    ))
}

fn invalid_help_usage(locale: Locale, prefix: &str) -> String {
    match locale {
        Locale::English => format!("⚠️ Usage: {prefix}help [command or module]"),
        Locale::Russian => format!("⚠️ Использование: {prefix}help [команда]"),
    }
}

fn overview_heading(locale: Locale, modules: usize, commands: usize) -> String {
    match locale {
        Locale::English => format!("🛠 Lavis help: {modules} modules, {commands} commands"),
        Locale::Russian => format!("🛠 Справка Lavis: {modules} модулей, {commands} команд"),
    }
}

/// The alias overview label is localized, but the alias list itself and the
/// command count are locale-independent: the same runtime state must report
/// the same semantic counts in every locale.
fn aliases_overview_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "🔗 Aliases: ",
        Locale::Russian => "🔗 Псевдонимы: ",
    }
}

fn overview_instruction(locale: Locale, prefix: &str) -> String {
    match locale {
        Locale::English => format!("Use {prefix}help <command or module> for details."),
        Locale::Russian => {
            format!("Используйте {prefix}help <команда или модуль> для подробностей.")
        }
    }
}

fn unknown_topic(locale: Locale, topic: &str, prefix: &str) -> String {
    match locale {
        Locale::English => {
            format!("❓ Unknown command or module: {topic}\nUse {prefix}help to list commands.")
        }
        Locale::Russian => format!(
            "❓ Неизвестная команда или модуль: {topic}\nИспользуйте {prefix}help для списка команд."
        ),
    }
}

fn command_summary(command: &CommandDefinition, locale: Locale) -> &'static str {
    command_text_summary(command.kind, locale)
}

fn command_description(command: &CommandDefinition, locale: Locale) -> &'static str {
    command_text_description(command.kind, locale)
}

fn module_primary(module: &ModuleSpec, commands: &str, locale: Locale) -> String {
    match locale {
        Locale::English => format!(
            "{}\n\nCommands:\n{commands}\n\nCapabilities: {}\nPolicy: {}",
            module_description(module.id, locale),
            capability_labels(module.capabilities, locale),
            module_policy(module, locale)
        ),
        Locale::Russian => format!(
            "{}\n\nКоманды:\n{commands}\n\nВозможности: {}\nПолитика: {}",
            module_description(module.id, locale),
            capability_labels(module.capabilities, locale),
            module_policy(module, locale)
        ),
    }
}

fn module_heading(module: &ModuleSpec, locale: Locale) -> String {
    match locale {
        Locale::English => format!("{} Module {}", module.icon, module.name),
        Locale::Russian => format!("{} Модуль {}", module.icon, module.name),
    }
}

fn invalid_module_heading(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "⚠️ Invalid module origin metadata",
        Locale::Russian => "⚠️ Некорректные метаданные происхождения модуля",
    }
}
fn invalid_module_primary(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "External module data is not displayed.",
        Locale::Russian => "Внешние данные модуля не отображаются.",
    }
}
fn invalid_module_provenance(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "⚠️ Origin metadata was rejected by the Lavis core.",
        Locale::Russian => "⚠️ Метаданные происхождения отклонены ядром Lavis.",
    }
}
fn usage_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Usage",
        Locale::Russian => "Использование",
    }
}
fn module_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Module",
        Locale::Russian => "Модуль",
    }
}
fn author_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Author",
        Locale::Russian => "Автор",
    }
}
fn capabilities_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Capabilities",
        Locale::Russian => "Возможности",
    }
}
fn risk_label_for_external(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Risk",
        Locale::Russian => "Риск",
    }
}
fn examples_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Examples",
        Locale::Russian => "Примеры",
    }
}
fn status_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "status",
        Locale::Russian => "статус",
    }
}
fn command_count_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "commands",
        Locale::Russian => "команд",
    }
}
fn commands_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Commands",
        Locale::Russian => "Команды",
    }
}
fn external_status(locale: Locale, active: bool) -> &'static str {
    match (locale, active) {
        (Locale::English, true) => "active",
        (Locale::English, false) => "inactive",
        (Locale::Russian, true) => "активен",
        (Locale::Russian, false) => "не активен",
    }
}
fn external_risk(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "external code, runs without a sandbox",
        Locale::Russian => "внешний код, запускается без песочницы",
    }
}
fn external_module_warning(locale: Locale) -> &'static str {
    match locale {
        Locale::English => {
            "⚠️ The external module runs in a separate process but is not placed in a system sandbox. Enable only code you trust."
        }
        Locale::Russian => {
            "⚠️ Внешний модуль запускается отдельным процессом, но не помещается в системную песочницу. Включайте только код, которому доверяете."
        }
    }
}
fn external_module_heading(locale: Locale, name: &str) -> String {
    match locale {
        Locale::English => format!("📦 Module {name}"),
        Locale::Russian => format!("📦 Модуль {name}"),
    }
}
fn command_metadata_unavailable(locale: Locale, prefix: &str, name: &str) -> String {
    match locale {
        Locale::English => format!("⚠️ Metadata for command {prefix}{name} is unavailable"),
        Locale::Russian => format!("⚠️ Метаданные команды {prefix}{name} недоступны"),
    }
}
fn no_stored_arguments(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "none",
        Locale::Russian => "нет",
    }
}

fn alias_primary_for_topic(
    topic: &str,
    prefix: &str,
    command: &CommandDefinition,
    module: &str,
    stored: &str,
    locale: Locale,
) -> String {
    match locale {
        Locale::English => format!(
            "Aliases invoke canonical commands under another name with stored arguments. Canonical commands take priority over aliases: an alias cannot override a built-in command with the same name.\n\nExample: {prefix}alias add sys fastfetch --logo arch; then {prefix}sys.\nStored arguments: {stored}.\n\n{prefix}{topic} invokes {prefix}{}; stored arguments are combined with invocation arguments.\nTarget module: {module}\nTarget risk: {}",
            command.name,
            risk_label(command.risk, locale)
        ),
        Locale::Russian => format!(
            "Псевдонимы позволяют вызывать канонические команды под другим именем с заранее заданными аргументами. Канонические команды имеют приоритет над псевдонимами: псевдоним не может переопределить встроенную команду с тем же именем.\n\nПример: {prefix}alias add sys fastfetch --logo arch; затем {prefix}sys.\nСохранённые аргументы: {stored}.\n\n{prefix}{topic} вызывает {prefix}{}; сохранённые аргументы объединяются с аргументами вызова.\nЦелевой модуль: {module}\nРиск цели: {}",
            command.name,
            risk_label(command.risk, locale)
        ),
    }
}

fn external_module_provenance(
    locale: Locale,
    author: &str,
    version: &str,
    source: &str,
    module: &ModuleSpec,
) -> String {
    match locale {
        Locale::English => format!(
            "External module. Author: {author}; version: {version}; source: {source}; capabilities: {}; {}.",
            capability_labels(module.capabilities, locale),
            module_policy(module, locale)
        ),
        Locale::Russian => format!(
            "Внешний модуль. Автор: {author}; версия: {version}; источник: {source}; возможности: {}; {}.",
            capability_labels(module.capabilities, locale),
            module_policy(module, locale)
        ),
    }
}

fn capability_labels(capabilities: &[ModuleCapability], locale: Locale) -> String {
    capabilities
        .iter()
        .map(|capability| match capability {
            ModuleCapability::TelegramRpc => "Telegram RPC",
            ModuleCapability::PersistentStateRead => match locale {
                Locale::English => "persistent-state read",
                Locale::Russian => "чтение постоянного состояния",
            },
            ModuleCapability::PersistentStateWrite => match locale {
                Locale::English => "persistent-state change",
                Locale::Russian => "изменение постоянного состояния",
            },
            ModuleCapability::HostInformation => match locale {
                Locale::English => "host information",
                Locale::Russian => "сведения о хосте",
            },
            ModuleCapability::RestrictedProcess => match locale {
                Locale::English => "restricted process",
                Locale::Russian => "ограниченный процесс",
            },
            ModuleCapability::Network => match locale {
                Locale::English => "network",
                Locale::Russian => "сеть",
            },
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn module_policy(module: &ModuleSpec, locale: Locale) -> String {
    let (unloadable, replaceable, allowed, prohibited) = match locale {
        Locale::English => ("unload", "replacement", "allowed", "prohibited"),
        Locale::Russian => ("выгрузка", "замена", "разрешена", "запрещена"),
    };
    format!(
        "{unloadable}: {}; {replaceable}: {}",
        if module.unloadable {
            allowed
        } else {
            prohibited
        },
        if module.replaceable {
            allowed
        } else {
            prohibited
        }
    )
}

fn module_provenance(module: &ModuleSpec, locale: Locale) -> String {
    match module.origin {
        ModuleOrigin::Builtin => core_provenance(locale),
        ModuleOrigin::External {
            author,
            version,
            source,
        } => external_module_provenance(locale, author, version, source, module),
    }
}

fn core_provenance(locale: Locale) -> String {
    match locale {
        Locale::English => {
            "This is a built-in Lavis module. It cannot be unloaded or replaced.".to_owned()
        }
        Locale::Russian => {
            "Это встроенный модуль Lavis. Его нельзя выгрузить или заменить.".to_owned()
        }
    }
}

fn external_provenance(locale: Locale) -> String {
    match locale {
        Locale::English => {
            "External module. Code runs outside a sandbox; trust only verified modules.".to_owned()
        }
        Locale::Russian => {
            "Внешний модуль. Код запускается вне песочницы; доверяйте только проверенным модулям."
                .to_owned()
        }
    }
}

fn risk_label(risk: CommandRisk, locale: Locale) -> &'static str {
    if locale == Locale::English {
        return english_risk(risk);
    }
    match risk {
        CommandRisk::ReadOnly => "только чтение",
        CommandRisk::PersistentStateChange => "изменение постоянного состояния",
        CommandRisk::RestrictedProcess => "ограниченный процесс",
        CommandRisk::ArbitraryProcess => "произвольный процесс",
        CommandRisk::Privileged => "привилегированная операция",
        CommandRisk::ExternalCodeInstall => "установка внешнего кода",
    }
}

fn documentation(
    locale: Locale,
    heading: String,
    primary: String,
    provenance: String,
) -> RenderedHelp {
    let RenderedResponse {
        response,
        entity_fallback,
    } = Response::documentation_card(locale, heading, primary, provenance);
    RenderedHelp {
        response,
        entity_fallback,
    }
}

fn plain(locale: Locale, text: String) -> RenderedHelp {
    RenderedHelp {
        response: Response::plain_with_locale(locale, text),
        entity_fallback: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        render, render_module_card, render_modules_invalid_usage, render_modules_overview,
        render_modules_overview_with_external_locale, render_with_external_locale,
    };
    use crate::{
        aliases::{Alias, AliasStore},
        commands::HelpRequest,
        external_modules::{
            manager::ExternalCommandRef,
            manifest::{ExternalCapability, ExternalCommandDescriptor, ExternalModuleDescriptor},
        },
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

    fn external_fixture() -> (ExternalModuleDescriptor, ExternalCommandRef) {
        let command = ExternalCommandDescriptor {
            name: "hello".to_owned(),
            summary_ru: "Внешняя сводка.".to_owned(),
            description_ru: "Внешнее описание из манифеста.".to_owned(),
            usage: "[name]".to_owned(),
            examples: vec!["Ada".to_owned()],
        };
        let reference = ExternalCommandRef {
            module_id: "fixture".to_owned(),
            command_name: command.name.clone(),
            summary_ru: command.summary_ru.clone(),
            description_ru: command.description_ru.clone(),
            usage: command.usage.clone(),
            examples: command.examples.clone(),
        };
        (
            ExternalModuleDescriptor {
                protocol_version: 1,
                id: "fixture".to_owned(),
                display_name: "Fixture module".to_owned(),
                version: "1.0.0".to_owned(),
                author: "Fixture author".to_owned(),
                entrypoint: PathBuf::from("/fixture/module"),
                module_dir: PathBuf::from("/fixture"),
                capabilities: vec![ExternalCapability::Network],
                default_command: None,
                subscriptions: vec![],
                telegram_methods: vec![],
                actions: vec![],
                commands: vec![command],
            },
            reference,
        )
    }

    #[tokio::test]
    async fn overview_has_stable_counts_order_and_active_prefix() {
        let response = render(&HelpRequest::Overview, "🦀", &aliases().await).response;
        assert!(
            response
                .text
                .starts_with("🛠 Справка Lavis: 3 модулей, 12 команд")
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
    async fn english_builtin_help_uses_typed_builtin_text_and_active_prefix() {
        let response = render_with_external_locale(
            &HelpRequest::Topic("start".to_owned()),
            "!",
            &aliases().await,
            &[],
            &[],
            crate::i18n::Locale::English,
        )
        .response;
        assert!(response.text.contains("Start the tutorial"));
        assert!(response.text.contains("!start bot"));
        assert!(!response.text.contains("Начать обучение"));
    }

    #[tokio::test]
    async fn english_overview_includes_aliases_and_active_external_modules() {
        let (aliases, directory) = aliases_with_core_alias().await;
        let (descriptor, reference) = external_fixture();
        let response = render_with_external_locale(
            &HelpRequest::Overview,
            "!",
            &aliases,
            &[reference],
            &[descriptor],
            crate::i18n::Locale::English,
        )
        .response;
        assert!(
            response
                .text
                .starts_with("🛠 Lavis help: 4 modules, 14 commands")
        );
        assert!(response.text.contains("!fastfetch"));
        assert!(response.text.contains("🔗 Aliases: !core"));
        assert!(
            response
                .text
                .contains("📦 Fixture module (fixture): !fixture.hello")
        );
        assert!(
            response
                .text
                .ends_with("This is a built-in Lavis module. It cannot be unloaded or replaced.")
        );
        assert_eq!(response.entities.len(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn overview_counts_are_locale_invariant_with_aliases() {
        let (aliases, directory) = aliases_with_core_alias().await;
        let (descriptor, reference) = external_fixture();
        let english = render_with_external_locale(
            &HelpRequest::Overview,
            "!",
            &aliases,
            std::slice::from_ref(&reference),
            std::slice::from_ref(&descriptor),
            crate::i18n::Locale::English,
        )
        .response;
        let russian = render_with_external_locale(
            &HelpRequest::Overview,
            "!",
            &aliases,
            &[reference],
            &[descriptor],
            crate::i18n::Locale::Russian,
        )
        .response;
        // Regression: the same runtime state must report the same semantic
        // module/command counts in every locale; only the localized text may
        // differ. Aliases used to be counted and listed only for English.
        assert!(
            english
                .text
                .starts_with("🛠 Lavis help: 4 modules, 14 commands")
        );
        assert!(
            russian
                .text
                .starts_with("🛠 Справка Lavis: 4 модулей, 14 команд")
        );
        assert!(english.text.contains("🔗 Aliases: !core"));
        assert!(russian.text.contains("🔗 Псевдонимы: !core"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn modules_overview_and_invalid_usage_are_localized() {
        let (descriptor, reference) = external_fixture();
        let english = render_modules_overview_with_external_locale(
            "!",
            std::slice::from_ref(&descriptor),
            std::slice::from_ref(&reference),
            crate::i18n::Locale::English,
        )
        .response;
        let russian = render_modules_overview_with_external_locale(
            "!",
            std::slice::from_ref(&descriptor),
            std::slice::from_ref(&reference),
            crate::i18n::Locale::Russian,
        )
        .response;
        assert!(english.text.starts_with("🧩 Lavis modules: 4"));
        assert!(english.text.contains("Core Lavis commands."));
        assert!(english.text.contains("!ping — Measure Telegram latency"));
        assert!(english.text.contains("📦 Fixture module (fixture)"));
        assert!(russian.text.starts_with("🧩 Модули Lavis: 4"));
        assert!(russian.text.contains("Основные команды Lavis."));
        assert!(russian.text.contains("!ping — Измерить задержку Telegram"));
        assert_eq!(
            render_modules_invalid_usage("!", crate::i18n::Locale::English)
                .response
                .text,
            "⚠️ Usage: !modules"
        );
        assert_eq!(
            render_modules_invalid_usage("!", crate::i18n::Locale::Russian)
                .response
                .text,
            "⚠️ Использование: !modules"
        );
    }

    #[tokio::test]
    async fn english_alias_and_external_help_keep_details_and_provenance() {
        let (aliases, directory) = aliases_with_core_alias().await;
        let (descriptor, reference) = external_fixture();
        let alias = render_with_external_locale(
            &HelpRequest::Topic("core".to_owned()),
            "!",
            &aliases,
            &[],
            &[],
            crate::i18n::Locale::English,
        )
        .response;
        assert!(alias.text.contains("Stored arguments: --logo arch."));
        assert!(alias.text.contains("Target module: system"));
        assert!(alias.text.contains("Target risk: restricted process"));
        assert!(
            alias
                .text
                .ends_with("This is a built-in Lavis module. It cannot be unloaded or replaced.")
        );
        assert_eq!(alias.entities.len(), 2);

        let external = render_with_external_locale(
            &HelpRequest::Topic("fixture.hello".to_owned()),
            "!",
            &aliases,
            &[reference],
            &[descriptor],
            crate::i18n::Locale::English,
        )
        .response;
        assert!(external.text.contains("Внешнее описание из манифеста."));
        assert!(external.text.contains("Usage: !fixture.hello [name]"));
        assert!(external.text.contains("Examples:\n!fixture.hello Ada"));
        assert!(external.text.ends_with(
            "External module. Code runs outside a sandbox; trust only verified modules."
        ));
        assert_eq!(external.entities.len(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn english_builtin_help_contains_no_cyrillic() {
        let response = render_with_external_locale(
            &HelpRequest::Topic("fastfetch".to_owned()),
            "!",
            &aliases().await,
            &[],
            &[],
            crate::i18n::Locale::English,
        )
        .response;
        assert!(
            !response
                .text
                .chars()
                .any(|character| ('А'..='я').contains(&character))
        );
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
        assert!(rendered.response.text.contains("Команды (12)"));
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
