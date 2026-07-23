use crate::commands::{commands, CommandDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleId {
    Core,
    System,
    Aliases,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleOrigin {
    Builtin,
    External {
        author: &'static str,
        version: &'static str,
        source: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleCapability {
    TelegramRpc,
    PersistentStateRead,
    PersistentStateWrite,
    HostInformation,
    RestrictedProcess,
    Network,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleSpec {
    pub id: ModuleId,
    pub name: &'static str,
    pub description_ru: &'static str,
    pub icon: &'static str,
    pub origin: ModuleOrigin,
    pub capabilities: &'static [ModuleCapability],
    pub unloadable: bool,
    pub replaceable: bool,
}

const MODULE_SPECS: [ModuleSpec; 3] = [
    ModuleSpec {
        id: ModuleId::Core,
        name: "core",
        description_ru: "Основные команды Lavis.",
        icon: "🧩",
        origin: ModuleOrigin::Builtin,
        capabilities: &[
            ModuleCapability::TelegramRpc,
            ModuleCapability::PersistentStateRead,
            ModuleCapability::PersistentStateWrite,
            ModuleCapability::HostInformation,
        ],
        unloadable: false,
        replaceable: false,
    },
    ModuleSpec {
        id: ModuleId::System,
        name: "system",
        description_ru: "Безопасно ограниченная системная информация.",
        icon: "🖥",
        origin: ModuleOrigin::Builtin,
        capabilities: &[
            ModuleCapability::HostInformation,
            ModuleCapability::RestrictedProcess,
        ],
        unloadable: false,
        replaceable: false,
    },
    ModuleSpec {
        id: ModuleId::Aliases,
        name: "aliases",
        description_ru: "Постоянные псевдонимы команд.",
        icon: "🔗",
        origin: ModuleOrigin::Builtin,
        capabilities: &[
            ModuleCapability::PersistentStateRead,
            ModuleCapability::PersistentStateWrite,
        ],
        unloadable: false,
        replaceable: false,
    },
];

pub fn modules() -> &'static [ModuleSpec] {
    &MODULE_SPECS
}

pub fn module_by_id(id: ModuleId) -> Option<&'static ModuleSpec> {
    modules().iter().find(|module| module.id == id)
}

pub fn module_by_name(name: &str) -> Option<&'static ModuleSpec> {
    modules()
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case(name))
}

pub fn module_definition(id: ModuleId) -> &'static ModuleSpec {
    module_by_id(id).unwrap_or_else(|| unreachable!("all ModuleId values are registered"))
}

pub fn commands_for_module(id: ModuleId) -> impl Iterator<Item = &'static CommandDefinition> {
    commands().iter().filter(move |command| command.module == id)
}

const EXTERNAL_AUTHOR_MAX: usize = 64;
const EXTERNAL_VERSION_MAX: usize = 32;
const EXTERNAL_SOURCE_MAX: usize = 256;

pub fn validate_external_origin(origin: &ModuleOrigin) -> bool {
    match origin {
        ModuleOrigin::Builtin => true,
        ModuleOrigin::External {
            author,
            version,
            source,
        } => {
            valid_external_value(author, EXTERNAL_AUTHOR_MAX)
                && valid_external_value(version, EXTERNAL_VERSION_MAX)
                && valid_external_value(source, EXTERNAL_SOURCE_MAX)
                && !is_absolute_local_source(source)
        }
    }
}

fn valid_external_value(value: &str, max_chars: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= max_chars
        && !value.chars().any(|character| character.is_control() || is_bidi_control(character))
}

fn is_absolute_local_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    source.starts_with('/')
        || source.starts_with('\\')
        || source
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ModuleCapability, ModuleId, ModuleOrigin, ModuleSpec, module_by_name, modules,
        validate_external_origin,
    };
    use crate::commands::{CommandRisk, commands, module_for_command};
    use std::collections::HashSet;

    fn external_fixture() -> ModuleSpec {
        ModuleSpec {
            id: ModuleId::Core,
            name: "fixture",
            description_ru: "Тестовый внешний модуль.",
            icon: "🧪",
            origin: ModuleOrigin::External {
                author: "Тест",
                version: "1.0.0",
                source: "https://example.invalid/module",
            },
            capabilities: &[ModuleCapability::Network],
            unloadable: true,
            replaceable: true,
        }
    }

    #[test]
    fn module_registry_metadata_is_complete_and_builtin_only() {
        let module_names = modules()
            .iter()
            .map(|module| module.name)
            .collect::<Vec<_>>();
        assert_eq!(module_names, ["core", "system", "aliases"]);
        assert_eq!(
            modules().iter().map(|module| module.id).collect::<HashSet<_>>().len(),
            modules().len()
        );
        assert_eq!(
            module_names
                .iter()
                .map(|name| name.to_ascii_lowercase())
                .collect::<HashSet<_>>()
                .len(),
            modules().len()
        );
        assert_eq!(
            module_by_name("SyStEm").map(|module| module.id),
            Some(ModuleId::System)
        );
        for module in modules() {
            assert!(!module.description_ru.is_empty());
            assert!(!module.icon.is_empty());
            assert!(matches!(module.origin, ModuleOrigin::Builtin));
            assert!(validate_external_origin(&module.origin));
            assert!(!module.unloadable && !module.replaceable);
            assert_eq!(
                module.capabilities.iter().collect::<HashSet<_>>().len(),
                module.capabilities.len()
            );
            assert!(!module.capabilities.contains(&ModuleCapability::Network));
        }
    }

    #[test]
    fn external_fixture_carries_renderable_provenance_without_joining_production_registry() {
        let fixture = external_fixture();
        let ModuleOrigin::External { author, version, source } = fixture.origin else {
            panic!("expected external fixture");
        };
        assert_eq!(
            (author, version, source),
            ("Тест", "1.0.0", "https://example.invalid/module")
        );
        assert!(validate_external_origin(&fixture.origin));
        assert!(!modules()
            .iter()
            .any(|module| matches!(module.origin, ModuleOrigin::External { .. })));
    }

    #[test]
    fn external_origin_validator_rejects_untrusted_metadata() {
        let external = |author, version, source| ModuleOrigin::External {
            author,
            version,
            source,
        };
        assert!(!validate_external_origin(&external(
            "",
            "1",
            "https://example.invalid"
        )));
        assert!(!validate_external_origin(&external(
            "Автор",
            "",
            "https://example.invalid"
        )));
        assert!(!validate_external_origin(&external(
            "Автор", "1", ""
        )));
        assert!(!validate_external_origin(&external(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "1",
            "https://example.invalid",
        )));
        assert!(!validate_external_origin(&external(
            "Автор\n",
            "1",
            "https://example.invalid"
        )));
        assert!(!validate_external_origin(&external(
            "Автор",
            "1\u{0001}",
            "https://example.invalid"
        )));
        assert!(!validate_external_origin(&external(
            "Автор\u{202e}",
            "1",
            "https://example.invalid"
        )));
        assert!(!validate_external_origin(&external(
            "Автор",
            "1",
            "https://example.invalid/\u{202e}"
        )));
        assert!(!validate_external_origin(&external(
            "Автор\u{2028}",
            "1",
            "https://example.invalid"
        )));
        assert!(!validate_external_origin(&external(
            "Автор",
            "1\u{2029}",
            "https://example.invalid"
        )));
        assert!(!validate_external_origin(&external(
            "Автор",
            "1",
            "/tmp/module"
        )));
        assert!(!validate_external_origin(&external(
            "Автор",
            "1",
            "C:\\module"
        )));
        assert!(!validate_external_origin(&external(
            "Автор",
            "1",
            "file:///tmp/module"
        )));
        assert!(!validate_external_origin(&external(
            "Автор",
            "1",
            "FILE:///etc/passwd"
        )));
    }

    #[test]
    fn command_risks_require_matching_module_capabilities() {
        for command in commands() {
            let module = module_for_command(command).unwrap();
            match command.risk {
                CommandRisk::ReadOnly => {}
                CommandRisk::PersistentStateChange => {
                    assert!(module.capabilities.contains(&ModuleCapability::PersistentStateWrite));
                }
                CommandRisk::RestrictedProcess => {
                    assert!(module.capabilities.contains(&ModuleCapability::RestrictedProcess));
                }
                CommandRisk::ArbitraryProcess | CommandRisk::Privileged => {
                    panic!("production command has prohibited risk");
                }
            }
        }
    }
}
