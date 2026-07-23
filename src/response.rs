pub const MAX_UTF16_UNITS: usize = 4096;
pub const TRUNCATION_SUFFIX: &str = "… output truncated";
pub const DOCUMENTATION_TRUNCATION_SUFFIX: &str = "… описание сокращено";

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub text: String,
    pub entities: Vec<grammers_client::tl::enums::MessageEntity>,
}

pub struct RenderedResponse {
    pub response: Response,
    pub entity_fallback: bool,
}

impl Response {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: truncate_utf16(&text.into()),
            entities: Vec::new(),
        }
    }

    pub fn preformatted(text: impl Into<String>) -> Self {
        let text = truncate_utf16(&text.into());
        let Some(length) = utf16_i32_len(&text) else {
            return Self::plain(text);
        };
        if text.is_empty() {
            return Self::plain(text);
        }

        Self {
            text,
            entities: vec![
                grammers_client::tl::types::MessageEntityPre {
                    offset: 0,
                    length,
                    language: String::new(),
                }
                .into(),
            ],
        }
    }

    pub fn collapsed(heading: String, body: String) -> RenderedResponse {
        let prefix = format!("{heading}\n\n");
        let text = truncate_utf16(&format!("{prefix}{body}"));
        let Some(body) = text.strip_prefix(&prefix) else {
            return RenderedResponse {
                response: Self::plain(text),
                entity_fallback: true,
            };
        };
        let Some(offset) = utf16_i32_len(&prefix) else {
            return RenderedResponse {
                response: Self::plain(text),
                entity_fallback: true,
            };
        };
        let Some(length) = utf16_i32_len(body) else {
            return RenderedResponse {
                response: Self::plain(text),
                entity_fallback: true,
            };
        };
        if length == 0 {
            return RenderedResponse {
                response: Self::plain(text),
                entity_fallback: true,
            };
        }

        RenderedResponse {
            response: Self {
                text,
                entities: vec![
                    grammers_client::tl::types::MessageEntityBlockquote {
                        offset,
                        length,
                        collapsed: true,
                    }
                    .into(),
                ],
            },
            entity_fallback: false,
        }
    }

    pub fn documentation_card(
        heading: String,
        primary: String,
        provenance: String,
    ) -> RenderedResponse {
        if heading.is_empty() || primary.is_empty() || provenance.is_empty() {
            return documentation_fallback(heading, primary, provenance);
        }
        let separators = "\n\n";
        let Some(heading_units) = utf16_i32_len(&heading).map(|length| length as usize) else {
            return documentation_fallback(heading, primary, provenance);
        };
        let Some(provenance_units) = utf16_i32_len(&provenance).map(|length| length as usize)
        else {
            return documentation_fallback(heading, primary, provenance);
        };
        let separator_units = separators.encode_utf16().count();
        let reserved = heading_units
            .saturating_add(provenance_units)
            .saturating_add(separator_units.saturating_mul(2));
        if reserved >= MAX_UTF16_UNITS {
            return documentation_fallback(heading, primary, provenance);
        }
        let available_primary = MAX_UTF16_UNITS - reserved;
        let rendered_primary = if primary.encode_utf16().count() <= available_primary {
            primary
        } else {
            truncate_to_utf16(&primary, available_primary, DOCUMENTATION_TRUNCATION_SUFFIX)
        };
        if rendered_primary.is_empty() {
            return documentation_fallback(heading, rendered_primary, provenance);
        }

        let text = format!("{heading}{separators}{rendered_primary}{separators}{provenance}");
        let Some(primary_offset) = utf16_i32_len(&format!("{heading}{separators}")) else {
            return documentation_fallback(heading, rendered_primary, provenance);
        };
        let Some(primary_length) = utf16_i32_len(&rendered_primary) else {
            return documentation_fallback(heading, rendered_primary, provenance);
        };
        let Some(provenance_offset) = utf16_i32_len(&format!(
            "{heading}{separators}{rendered_primary}{separators}"
        )) else {
            return documentation_fallback(heading, rendered_primary, provenance);
        };
        let Some(provenance_length) = utf16_i32_len(&provenance) else {
            return documentation_fallback(heading, rendered_primary, provenance);
        };
        if text.encode_utf16().count() > MAX_UTF16_UNITS
            || primary_length == 0
            || provenance_length == 0
        {
            return documentation_fallback(heading, rendered_primary, provenance);
        }

        RenderedResponse {
            response: Self {
                text,
                entities: vec![
                    grammers_client::tl::types::MessageEntityBlockquote {
                        offset: primary_offset,
                        length: primary_length,
                        collapsed: true,
                    }
                    .into(),
                    grammers_client::tl::types::MessageEntityBlockquote {
                        offset: provenance_offset,
                        length: provenance_length,
                        collapsed: false,
                    }
                    .into(),
                ],
            },
            entity_fallback: false,
        }
    }
}

fn documentation_fallback(
    heading: String,
    primary: String,
    provenance: String,
) -> RenderedResponse {
    let provenance_line = format!("Источник: {provenance}");
    let provenance_units = provenance_line.encode_utf16().count();
    if provenance_units > MAX_UTF16_UNITS {
        return RenderedResponse {
            response: Response::plain(provenance_line),
            entity_fallback: true,
        };
    }

    let labels = "Документация: \nОсновное: \n";
    let reserved = labels
        .encode_utf16()
        .count()
        .saturating_add(provenance_units)
        .saturating_add(1);
    if reserved > MAX_UTF16_UNITS {
        return RenderedResponse {
            response: Response {
                text: provenance_line,
                entities: Vec::new(),
            },
            entity_fallback: true,
        };
    }

    let content_budget = MAX_UTF16_UNITS - reserved;
    let heading_budget = content_budget / 2;
    let primary_budget = content_budget - heading_budget;
    let heading = truncate_to_utf16(&heading, heading_budget, DOCUMENTATION_TRUNCATION_SUFFIX);
    let primary = truncate_to_utf16(&primary, primary_budget, DOCUMENTATION_TRUNCATION_SUFFIX);
    let text = format!("Документация: {heading}\nОсновное: {primary}\n{provenance_line}");
    RenderedResponse {
        response: Response {
            text,
            entities: Vec::new(),
        },
        entity_fallback: true,
    }
}

pub fn truncate_utf16(text: &str) -> String {
    if text.encode_utf16().count() <= MAX_UTF16_UNITS {
        return text.to_owned();
    }
    let suffix_units = TRUNCATION_SUFFIX.encode_utf16().count();
    let limit = MAX_UTF16_UNITS.saturating_sub(suffix_units);
    let mut end = 0;
    let mut units = 0usize;
    let mut last_newline = None;
    for (index, character) in text.char_indices() {
        let character_units = character.len_utf16();
        if units.saturating_add(character_units) > limit {
            break;
        }
        units += character_units;
        end = index + character.len_utf8();
        if character == '\n' {
            last_newline = Some(end);
        }
    }
    let end = last_newline.unwrap_or(end);
    format!("{}{}", &text[..end], TRUNCATION_SUFFIX)
}

fn truncate_to_utf16(text: &str, limit: usize, suffix: &str) -> String {
    if text.encode_utf16().count() <= limit {
        return text.to_owned();
    }
    let suffix_units = suffix.encode_utf16().count();
    if limit <= suffix_units {
        return String::new();
    }
    let mut end = 0;
    let mut units = 0usize;
    for (index, character) in text.char_indices() {
        if units.saturating_add(character.len_utf16()) > limit - suffix_units {
            break;
        }
        units += character.len_utf16();
        end = index + character.len_utf8();
    }
    format!("{}{}", &text[..end], suffix)
}

fn utf16_i32_len(text: &str) -> Option<i32> {
    i32::try_from(text.encode_utf16().count()).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        DOCUMENTATION_TRUNCATION_SUFFIX, MAX_UTF16_UNITS, Response, TRUNCATION_SUFFIX,
        truncate_utf16,
    };

    #[test]
    fn truncates_non_bmp_text_at_utf16_boundaries() {
        let text = "🦀".repeat(MAX_UTF16_UNITS);
        let output = truncate_utf16(&text);

        assert!(output.ends_with(TRUNCATION_SUFFIX));
        assert!(output.encode_utf16().count() <= MAX_UTF16_UNITS);
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }

    #[test]
    fn preformatted_entity_spans_final_text() {
        let response = Response::preformatted("  output\n");
        let grammers_client::tl::enums::MessageEntity::Pre(entity) = &response.entities[0] else {
            panic!("expected a preformatted entity");
        };

        assert_eq!(entity.offset, 0);
        assert_eq!(
            usize::try_from(entity.length).unwrap(),
            response.text.encode_utf16().count()
        );
    }

    #[test]
    fn empty_collapsed_body_does_not_create_an_entity() {
        let rendered = Response::collapsed("heading".to_owned(), String::new());

        assert!(rendered.response.entities.is_empty());
        assert!(rendered.entity_fallback);
    }

    #[test]
    fn documentation_card_covers_primary_then_provenance_with_two_blockquotes() {
        let rendered = Response::documentation_card(
            "📚 Заголовок".to_owned(),
            "Основной текст 🦀".to_owned(),
            "Источник: builtin".to_owned(),
        );
        assert!(!rendered.entity_fallback);
        assert_eq!(
            rendered.response.text,
            "📚 Заголовок\n\nОсновной текст 🦀\n\nИсточник: builtin"
        );
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
        assert!(primary.collapsed);
        assert!(!provenance.collapsed);
        let units = rendered.response.text.encode_utf16().collect::<Vec<_>>();
        let primary_start = usize::try_from(primary.offset).unwrap();
        let primary_end = primary_start + usize::try_from(primary.length).unwrap();
        let provenance_start = usize::try_from(provenance.offset).unwrap();
        let provenance_end = provenance_start + usize::try_from(provenance.length).unwrap();
        assert_eq!(
            String::from_utf16(&units[primary_start..primary_end]).unwrap(),
            "Основной текст 🦀"
        );
        assert_eq!(
            String::from_utf16(&units[provenance_start..provenance_end]).unwrap(),
            "Источник: builtin"
        );
        assert!(primary_end <= provenance_start);
        assert!(provenance_end <= units.len());
        assert!(units.len() <= MAX_UTF16_UNITS);
    }

    #[test]
    fn documentation_card_truncates_primary_before_complete_provenance() {
        let provenance = "Источник: внешний модуль".to_owned();
        let rendered = Response::documentation_card(
            "Заголовок".to_owned(),
            "🦀".repeat(MAX_UTF16_UNITS),
            provenance.clone(),
        );
        assert!(!rendered.entity_fallback);
        assert!(rendered.response.text.contains(&provenance));
        assert!(
            rendered
                .response
                .text
                .contains(DOCUMENTATION_TRUNCATION_SUFFIX)
        );
        assert!(rendered.response.text.encode_utf16().count() <= MAX_UTF16_UNITS);
        let grammers_client::tl::enums::MessageEntity::Blockquote(primary) =
            &rendered.response.entities[0]
        else {
            panic!("expected primary blockquote");
        };
        let grammers_client::tl::enums::MessageEntity::Blockquote(entity) =
            &rendered.response.entities[1]
        else {
            panic!("expected provenance blockquote");
        };
        let units = rendered.response.text.encode_utf16().collect::<Vec<_>>();
        let start = usize::try_from(entity.offset).unwrap();
        let end = start + usize::try_from(entity.length).unwrap();
        assert_eq!(String::from_utf16(&units[start..end]).unwrap(), provenance);
        let primary_end =
            usize::try_from(primary.offset).unwrap() + usize::try_from(primary.length).unwrap();
        assert!(primary_end <= start);
        assert!(end <= units.len());
    }

    #[test]
    fn documentation_card_falls_back_for_empty_or_unrepresentable_sections() {
        let empty = Response::documentation_card(
            "Заголовок".to_owned(),
            String::new(),
            "Источник".to_owned(),
        );
        assert!(empty.entity_fallback);
        assert!(empty.response.entities.is_empty());
        assert!(empty.response.text.starts_with("Документация:"));

        let oversized = Response::documentation_card(
            "З".repeat(MAX_UTF16_UNITS),
            "Основное".to_owned(),
            "Источник".to_owned(),
        );
        assert!(oversized.entity_fallback);
        assert!(oversized.response.entities.is_empty());
        assert!(oversized.response.text.encode_utf16().count() <= MAX_UTF16_UNITS);
        assert!(oversized.response.text.ends_with("Источник: Источник"));

        let primary_and_heading = Response::documentation_card(
            "🦀".repeat(MAX_UTF16_UNITS),
            "Основное".repeat(MAX_UTF16_UNITS),
            "полный источник".to_owned(),
        );
        assert!(primary_and_heading.entity_fallback);
        assert!(primary_and_heading.response.entities.is_empty());
        assert!(
            primary_and_heading
                .response
                .text
                .ends_with("Источник: полный источник")
        );
        assert!(primary_and_heading.response.text.encode_utf16().count() <= MAX_UTF16_UNITS);
    }
}
