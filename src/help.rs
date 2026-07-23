pub use crate::response::Response;
use crate::{
    aliases::AliasStore,
    commands::{HelpRequest, canonical_command},
    modules::{commands_for_module, module_by_name},
};

pub struct RenderedHelp {
    pub response: Response,
    pub entity_fallback: bool,
}

pub fn render(request: &HelpRequest, prefix: &str, aliases: &AliasStore) -> RenderedHelp {
    match request {
        HelpRequest::Overview => render_overview(prefix),
        HelpRequest::Topic(topic) => render_topic(topic, prefix, aliases),
        HelpRequest::Invalid => RenderedHelp {
            response: Response::plain(format!("⚠️ Usage: {prefix}help [command]")),
            entity_fallback: false,
        },
    }
}

fn render_overview(prefix: &str) -> RenderedHelp {
    render_quote(
        "🛠 Lavis commands".to_owned(),
        crate::modules::MODULES
            .iter()
            .map(|module| {
                let commands = commands_for_module(module.id)
                    .map(|definition| {
                        format!("{prefix}{} — {}", definition.usage, definition.summary)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "{} {} ({})\n{}",
                    module.icon,
                    module.name,
                    commands_for_module(module.id).count(),
                    commands
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

fn render_alias(topic: &str, prefix: &str, aliases: &AliasStore) -> Option<RenderedHelp> {
    let alias = aliases.lookup(topic)?;
    let preset = if alias.args.is_empty() {
        String::new()
    } else {
        format!(" {}", shell_words::join(&alias.args))
    };
    Some(render_quote(
        format!("🔗 {prefix}{topic}"),
        format!(
            "Alias for {prefix}{}{preset}\n\nUsage: {prefix}{topic} [arguments]",
            alias.target
        ),
    ))
}

fn render_topic(topic: &str, prefix: &str, aliases: &AliasStore) -> RenderedHelp {
    // Topic precedence is deliberate: canonical commands cannot be shadowed by aliases,
    // while an existing alias named after a module remains useful.
    if let Some(definition) = canonical_command(&topic.to_ascii_lowercase()) {
        return render_command(definition, prefix);
    }
    if let Some(rendered) = render_alias(topic, prefix, aliases) {
        return rendered;
    }
    if let Some(module) = module_by_name(topic) {
        return render_quote(
            format!("{} {} module", module.icon, module.name),
            format!(
                "{}\n\n{}",
                module.description,
                commands_for_module(module.id)
                    .map(|definition| format!(
                        "{prefix}{} — {}",
                        definition.usage, definition.summary
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        );
    }
    RenderedHelp {
        response: Response::plain(format!(
            "❓ Unknown command: {topic}\nUse {prefix}help to list available commands."
        )),
        entity_fallback: false,
    }
}

fn render_command(definition: &crate::commands::CommandDefinition, prefix: &str) -> RenderedHelp {
    render_quote(
        format!("{} {prefix}{}", definition.icon, definition.usage),
        format!(
            "{}\n\nUsage: {prefix}{}",
            definition.description, definition.usage
        ),
    )
}

fn render_quote(heading: String, body: String) -> RenderedHelp {
    let rendered = Response::collapsed(heading, body);
    RenderedHelp {
        response: rendered.response,
        entity_fallback: rendered.entity_fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::{Response, render};
    use crate::{
        aliases::{Alias, AliasStore},
        commands::{CommandKind, HelpRequest, definition},
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

    async fn aliases_with_mini() -> (AliasStore, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("lavis-help-alias-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let mut aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
        aliases
            .add(
                "mini",
                Alias {
                    target: "fastfetch".to_owned(),
                    args: Vec::new(),
                },
            )
            .await
            .unwrap();
        (aliases, directory)
    }

    #[tokio::test]
    async fn overview_uses_registry_order_and_configured_prefix_once_per_command() {
        let response = render(&HelpRequest::Overview, "!", &aliases().await).response;

        assert_eq!(
            response.text,
            "🛠 Lavis commands\n\n🧩 core (5)\n!help [command] — Show command help\n!modules — List internal modules\n!ping — Measure Telegram latency\n!prefix [new-prefix|reset] — Show or change the command prefix\n!stats — Show runtime statistics\n\n🖥 system (1)\n!fastfetch [--no-profile] [--logo <...>] [--structure <...>] [--separator <text>] — Показать системную информацию\n\n🔗 aliases (1)\n!alias [list|add <name> <command> [arguments...]|show <name>|del <name>] — Manage command aliases"
        );
        for command in [
            "help",
            "modules",
            "ping",
            "prefix",
            "stats",
            "fastfetch",
            "alias",
        ] {
            assert_eq!(response.text.matches(&format!("!{command}")).count(), 1);
        }
        assert_eq!(response.entities.len(), 1);
    }

    #[tokio::test]
    async fn command_details_keep_titles_outside_a_single_entity() {
        for (kind, title) in [
            (CommandKind::Ping, "🏓 ,ping\n\n"),
            (CommandKind::Stats, "📊 ,stats\n\n"),
            (CommandKind::Help, "🛠 ,help [command]\n\n"),
        ] {
            let response = render(
                &HelpRequest::Topic(definition(kind).name.to_owned()),
                ",",
                &aliases().await,
            )
            .response;
            let definition = definition(kind);

            assert!(response.text.starts_with(title));
            assert!(response.text.ends_with(&format!(
                "{}\n\nUsage: ,{}",
                definition.description, definition.usage
            )));
            assert_eq!(response.entities.len(), 1);
        }
    }

    #[tokio::test]
    async fn renders_unknown_and_invalid_help_plainly() {
        let unknown = render(&HelpRequest::Topic("foo".to_owned()), ",", &aliases().await).response;
        let invalid = render(&HelpRequest::Invalid, "!", &aliases().await).response;

        assert_eq!(
            unknown,
            Response::plain("❓ Unknown command: foo\nUse ,help to list available commands.")
        );
        assert_eq!(invalid, Response::plain("⚠️ Usage: !help [command]"));
    }

    #[tokio::test]
    async fn alias_help_uses_the_configured_alias_target() {
        let (aliases, directory) = aliases_with_mini().await;
        let response = render(&HelpRequest::Topic("mini".to_owned()), "!", &aliases).response;

        assert!(response.text.starts_with("🔗 !mini\n\n"));
        assert!(response.text.contains("Alias for !fastfetch"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn module_topics_are_case_insensitive_and_aliases_take_precedence() {
        let (mut store, directory) = aliases_with_mini().await;
        store
            .add(
                "core",
                Alias {
                    target: "ping".to_owned(),
                    args: Vec::new(),
                },
            )
            .await
            .unwrap();
        let alias = render(&HelpRequest::Topic("CORE".to_owned()), "!", &store).response;
        assert!(alias.text.starts_with("🔗 !CORE"));
        let modules = aliases().await;
        let system = render(&HelpRequest::Topic("SyStEm".to_owned()), "!", &modules).response;
        assert_eq!(
            system.text,
            "🖥 system module\n\nSystem information commands.\n\n!fastfetch [--no-profile] [--logo <...>] [--structure <...>] [--separator <text>] — Показать системную информацию"
        );
        assert_eq!(system.entities.len(), 1);
        let grammers_client::tl::enums::MessageEntity::Blockquote(entity) = &system.entities[0]
        else {
            panic!("expected a blockquote entity");
        };
        let units: Vec<u16> = system.text.encode_utf16().collect();
        let offset = usize::try_from(entity.offset).unwrap();
        let length = usize::try_from(entity.length).unwrap();
        assert_eq!(
            String::from_utf16(&units[offset..offset + length]).unwrap(),
            "System information commands.\n\n!fastfetch [--no-profile] [--logo <...>] [--structure <...>] [--separator <text>] — Показать системную информацию"
        );
        assert_eq!(
            length,
            "System information commands.\n\n!fastfetch [--no-profile] [--logo <...>] [--structure <...>] [--separator <text>] — Показать системную информацию"
                .encode_utf16()
                .count()
        );
        assert!(entity.collapsed);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn blockquote_uses_utf16_body_bounds() {
        let response = render(
            &HelpRequest::Topic("help".to_owned()),
            "🦀",
            &aliases().await,
        )
        .response;
        let text_units: Vec<u16> = response.text.encode_utf16().collect();
        let grammers_client::tl::enums::MessageEntity::Blockquote(entity) = &response.entities[0]
        else {
            panic!("expected a blockquote entity");
        };
        assert!(entity.collapsed);
        let offset = usize::try_from(entity.offset).unwrap();
        let length = usize::try_from(entity.length).unwrap();

        assert_eq!(
            String::from_utf16(&text_units[..offset]).unwrap(),
            "🛠 🦀help [command]\n\n"
        );
        assert_eq!(
            String::from_utf16(&text_units[offset..offset + length]).unwrap(),
            "Shows the command overview or detailed help for a command, alias, or module.\n\nUsage: 🦀help [command]"
        );
    }

    #[test]
    fn plain_response_has_no_entities() {
        assert!(Response::plain("plain").entities.is_empty());
    }
}
