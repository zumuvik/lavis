use crate::i18n::{Locale, ResponseText, external_module_provenance, response_text};

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
    pub fn four_blockquotes_with_locale(locale: Locale, text: String) -> Self {
        let text = truncate_utf16_with_locale(locale, &text);
        let mut entities = Vec::with_capacity(4);
        let mut offset = 0usize;
        for section in text.split("\n\n") {
            if let (Some(offset), Some(length)) =
                (utf16_i32_len(&text[..offset]), utf16_i32_len(section))
                && length > 0
            {
                entities.push(
                    grammers_client::tl::types::MessageEntityBlockquote {
                        offset,
                        length,
                        collapsed: false,
                    }
                    .into(),
                );
            }
            offset += section.len() + 2;
        }
        if entities.len() == 4 {
            Self { text, entities }
        } else {
            Self::plain_with_locale(locale, text)
        }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self::plain_with_locale(Locale::English, text)
    }

    pub fn plain_with_locale(locale: Locale, text: impl Into<String>) -> Self {
        Self {
            text: truncate_utf16_with_locale(locale, &text.into()),
            entities: Vec::new(),
        }
    }

    pub fn preformatted(text: impl Into<String>) -> Self {
        Self::preformatted_with_locale(Locale::English, text)
    }

    pub fn preformatted_with_locale(locale: Locale, text: impl Into<String>) -> Self {
        let text = truncate_utf16_with_locale(locale, &text.into());
        let Some(length) = utf16_i32_len(&text) else {
            return Self::plain_with_locale(locale, text);
        };
        if text.is_empty() {
            return Self::plain_with_locale(locale, text);
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
        Self::collapsed_with_locale(Locale::English, heading, body)
    }

    pub fn collapsed_with_locale(
        locale: Locale,
        heading: String,
        body: String,
    ) -> RenderedResponse {
        let prefix = format!("{heading}\n\n");
        let text = truncate_utf16_with_locale(locale, &format!("{prefix}{body}"));
        let Some(body) = text.strip_prefix(&prefix) else {
            return RenderedResponse {
                response: Self::plain_with_locale(locale, text),
                entity_fallback: true,
            };
        };
        let Some(offset) = utf16_i32_len(&prefix) else {
            return RenderedResponse {
                response: Self::plain_with_locale(locale, text),
                entity_fallback: true,
            };
        };
        let Some(length) = utf16_i32_len(body) else {
            return RenderedResponse {
                response: Self::plain_with_locale(locale, text),
                entity_fallback: true,
            };
        };
        if length == 0 {
            return RenderedResponse {
                response: Self::plain_with_locale(locale, text),
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
        locale: Locale,
        heading: String,
        primary: String,
        provenance: String,
    ) -> RenderedResponse {
        if heading.is_empty() || primary.is_empty() || provenance.is_empty() {
            return documentation_fallback(locale, heading, primary, provenance);
        }
        let separators = "\n\n";
        let Some(heading_units) = utf16_i32_len(&heading).map(|length| length as usize) else {
            return documentation_fallback(locale, heading, primary, provenance);
        };
        let Some(provenance_units) = utf16_i32_len(&provenance).map(|length| length as usize)
        else {
            return documentation_fallback(locale, heading, primary, provenance);
        };
        let separator_units = separators.encode_utf16().count();
        let reserved = heading_units
            .saturating_add(provenance_units)
            .saturating_add(separator_units.saturating_mul(2));
        if reserved >= MAX_UTF16_UNITS {
            return documentation_fallback(locale, heading, primary, provenance);
        }
        let available_primary = MAX_UTF16_UNITS - reserved;
        let rendered_primary = if primary.encode_utf16().count() <= available_primary {
            primary
        } else {
            truncate_to_utf16(
                &primary,
                available_primary,
                response_text(locale, ResponseText::DocumentationTruncated),
            )
        };
        if rendered_primary.is_empty() {
            return documentation_fallback(locale, heading, rendered_primary, provenance);
        }

        let text = format!("{heading}{separators}{rendered_primary}{separators}{provenance}");
        let Some(primary_offset) = utf16_i32_len(&format!("{heading}{separators}")) else {
            return documentation_fallback(locale, heading, rendered_primary, provenance);
        };
        let Some(primary_length) = utf16_i32_len(&rendered_primary) else {
            return documentation_fallback(locale, heading, rendered_primary, provenance);
        };
        let Some(provenance_offset) = utf16_i32_len(&format!(
            "{heading}{separators}{rendered_primary}{separators}"
        )) else {
            return documentation_fallback(locale, heading, rendered_primary, provenance);
        };
        let Some(provenance_length) = utf16_i32_len(&provenance) else {
            return documentation_fallback(locale, heading, rendered_primary, provenance);
        };
        if text.encode_utf16().count() > MAX_UTF16_UNITS
            || primary_length == 0
            || provenance_length == 0
        {
            return documentation_fallback(locale, heading, rendered_primary, provenance);
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
    locale: Locale,
    heading: String,
    primary: String,
    provenance: String,
) -> RenderedResponse {
    let provenance_line = format!("{}: {provenance}", source_label(locale));
    let provenance_units = provenance_line.encode_utf16().count();
    if provenance_units > MAX_UTF16_UNITS {
        return RenderedResponse {
            response: Response::plain_with_locale(locale, provenance_line),
            entity_fallback: true,
        };
    }

    let labels = format!(
        "{}: \n{}: \n",
        documentation_label(locale),
        primary_label(locale)
    );
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
    let suffix = response_text(locale, ResponseText::DocumentationTruncated);
    let heading = truncate_to_utf16(&heading, heading_budget, suffix);
    let primary = truncate_to_utf16(&primary, primary_budget, suffix);
    let text = format!(
        "{}: {heading}\n{}: {primary}\n{provenance_line}",
        documentation_label(locale),
        primary_label(locale)
    );
    RenderedResponse {
        response: Response {
            text,
            entities: Vec::new(),
        },
        entity_fallback: true,
    }
}

fn documentation_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Documentation",
        Locale::Russian => "Документация",
    }
}

fn primary_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Primary",
        Locale::Russian => "Основное",
    }
}

fn source_label(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "Source",
        Locale::Russian => "Источник",
    }
}

pub fn truncate_utf16(text: &str) -> String {
    truncate_utf16_with_locale(Locale::English, text)
}

pub fn truncate_utf16_with_locale(locale: Locale, text: &str) -> String {
    if text.encode_utf16().count() <= MAX_UTF16_UNITS {
        return text.to_owned();
    }
    let suffix = response_text(locale, ResponseText::Truncated);
    let suffix_units = suffix.encode_utf16().count();
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
    format!("{}{}", &text[..end], suffix)
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

pub fn sanitize_external_output(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if c == '\n' || c == '\t' {
            out.push(c);
        } else if c.is_control() || is_bidi_control(c) {
        } else {
            out.push(c);
        }
    }
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

impl Response {
    pub fn external_result(
        locale: Locale,
        module_text: &str,
        display_name: &str,
        module_id: &str,
        version: &str,
    ) -> Self {
        let sanitized = sanitize_external_output(module_text);
        let provenance = format!(
            "\n\n{}",
            external_module_provenance(locale, display_name, module_id, version)
        );
        let provenance_units = provenance.encode_utf16().count();
        if sanitized.encode_utf16().count() + provenance_units <= MAX_UTF16_UNITS {
            let text = format!("{sanitized}{provenance}");
            return Self::plain_with_locale(locale, text);
        }
        let available = MAX_UTF16_UNITS.saturating_sub(provenance_units);
        let truncated = truncate_to_utf16(
            &sanitized,
            available,
            response_text(locale, ResponseText::Truncated),
        );
        let text = format!("{truncated}{provenance}");
        Self::plain_with_locale(locale, text)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DOCUMENTATION_TRUNCATION_SUFFIX, MAX_UTF16_UNITS, Response, TRUNCATION_SUFFIX,
        sanitize_external_output, truncate_utf16,
    };
    use crate::i18n::Locale;

    #[test]
    fn truncates_non_bmp_text_at_utf16_boundaries() {
        let text = "🦀".repeat(MAX_UTF16_UNITS);
        let output = truncate_utf16(&text);

        assert!(output.ends_with(TRUNCATION_SUFFIX));
        assert!(output.encode_utf16().count() <= MAX_UTF16_UNITS);
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }

    #[test]
    fn four_blockquotes_use_utf16_offsets_for_each_section() {
        let text = [
            "┌ first 🦀\n├ owner\n└ version",
            "┌ source\n├ commit\n└ upstream",
            "┌ runtime\n├ prefix\n└ modules",
            "┌ environment\n├ host\n└ os",
        ]
        .join("\n\n");
        let response = Response::four_blockquotes_with_locale(Locale::English, text.clone());

        assert_eq!(response.text, text);
        assert_eq!(response.entities.len(), 4);
        let mut byte_offset = 0;
        for (entity, section) in response.entities.iter().zip(text.split("\n\n")) {
            let grammers_client::tl::enums::MessageEntity::Blockquote(entity) = entity else {
                panic!("expected blockquote entity");
            };
            assert_eq!(
                entity.offset,
                text[..byte_offset].encode_utf16().count() as i32
            );
            assert_eq!(entity.length, section.encode_utf16().count() as i32);
            assert!(!entity.collapsed);
            byte_offset += section.len() + 2;
        }
    }

    #[test]
    fn locale_aware_plain_and_collapsed_truncation_use_selected_suffix() {
        for (locale, suffix) in [
            (Locale::English, "… output truncated"),
            (Locale::Russian, "… вывод сокращён"),
        ] {
            let plain = Response::plain_with_locale(locale, "🦀".repeat(MAX_UTF16_UNITS));
            let preformatted =
                Response::preformatted_with_locale(locale, "🦀".repeat(MAX_UTF16_UNITS));
            let collapsed = Response::collapsed_with_locale(
                locale,
                "Heading".to_owned(),
                "🦀".repeat(MAX_UTF16_UNITS),
            );
            assert!(plain.text.ends_with(suffix));
            assert!(preformatted.text.ends_with(suffix));
            assert!(collapsed.response.text.ends_with(suffix));
            assert!(plain.text.encode_utf16().count() <= MAX_UTF16_UNITS);
            assert!(preformatted.text.encode_utf16().count() <= MAX_UTF16_UNITS);
            assert!(collapsed.response.text.encode_utf16().count() <= MAX_UTF16_UNITS);
        }
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
            Locale::Russian,
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
            Locale::Russian,
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
    fn documentation_truncation_uses_selected_locale_suffix() {
        for (locale, suffix) in [
            (Locale::English, "… documentation truncated"),
            (Locale::Russian, "… описание сокращено"),
        ] {
            let rendered = Response::documentation_card(
                locale,
                "Heading".to_owned(),
                "🦀".repeat(MAX_UTF16_UNITS),
                "Provenance".to_owned(),
            );
            assert!(rendered.response.text.contains(suffix));
            assert!(rendered.response.text.encode_utf16().count() <= MAX_UTF16_UNITS);
        }
    }

    #[test]
    fn documentation_card_falls_back_for_empty_or_unrepresentable_sections() {
        let empty = Response::documentation_card(
            Locale::Russian,
            "Заголовок".to_owned(),
            String::new(),
            "Источник".to_owned(),
        );
        assert!(empty.entity_fallback);
        assert!(empty.response.entities.is_empty());
        assert!(empty.response.text.starts_with("Документация:"));

        let oversized = Response::documentation_card(
            Locale::Russian,
            "З".repeat(MAX_UTF16_UNITS),
            "Основное".to_owned(),
            "Источник".to_owned(),
        );
        assert!(oversized.entity_fallback);
        assert!(oversized.response.entities.is_empty());
        assert!(oversized.response.text.encode_utf16().count() <= MAX_UTF16_UNITS);
        assert!(oversized.response.text.ends_with("Источник: Источник"));

        let primary_and_heading = Response::documentation_card(
            Locale::Russian,
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

    #[test]
    fn documentation_fallback_localizes_wrapper_labels() {
        let english = Response::documentation_card(
            Locale::English,
            "Heading".to_owned(),
            String::new(),
            "third-party content".to_owned(),
        );
        let russian = Response::documentation_card(
            Locale::Russian,
            "Заголовок".to_owned(),
            String::new(),
            "стороннее содержимое".to_owned(),
        );
        assert!(
            english
                .response
                .text
                .starts_with("Documentation: Heading\nPrimary:")
        );
        assert!(
            english
                .response
                .text
                .ends_with("Source: third-party content")
        );
        assert!(
            russian
                .response
                .text
                .starts_with("Документация: Заголовок\nОсновное:")
        );
        assert!(
            russian
                .response
                .text
                .ends_with("Источник: стороннее содержимое")
        );
    }

    #[test]
    fn external_result_preserves_provenance() {
        let result =
            Response::external_result(Locale::Russian, "Привет мир", "Тест", "test", "1.0.0");
        assert!(result.text.contains("Привет мир"));
        assert!(
            result
                .text
                .contains("⚠️ Внешний модуль «Тест» (test v1.0.0) — код без песочницы.")
        );
        assert!(result.text.encode_utf16().count() <= MAX_UTF16_UNITS);
    }

    #[test]
    fn external_result_localizes_framing_without_translating_module_text() {
        let module_text = "raw module: Привет";
        let english = Response::external_result(Locale::English, module_text, "Name", "id", "1");
        let russian = Response::external_result(Locale::Russian, module_text, "Name", "id", "1");
        assert!(english.text.starts_with(module_text));
        assert!(russian.text.starts_with(module_text));
        assert!(
            english
                .text
                .ends_with("⚠️ External module «Name» (id v1) — code runs without a sandbox.")
        );
        assert!(
            russian
                .text
                .ends_with("⚠️ Внешний модуль «Name» (id v1) — код без песочницы.")
        );
    }

    #[test]
    fn external_provenance_preserves_brace_containing_manifest_metadata() {
        for locale in [Locale::English, Locale::Russian] {
            let result = Response::external_result(
                locale,
                "module output",
                "name {id}",
                "id {version}",
                "version {prefix}",
            );
            assert!(result.text.contains("name {id}"));
            assert!(result.text.contains("id {version}"));
            assert!(result.text.contains("version {prefix}"));
        }
    }

    #[test]
    fn external_result_sanitizes_output() {
        let result = Response::external_result(
            Locale::Russian,
            "\x1b[31mкрасный\x1b[0m\n\t\x00bidi\u{202e}",
            "Тест",
            "t",
            "1.0",
        );
        assert!(!result.text.contains("\x1b["));
        assert!(!result.text.contains('\x00'));
        assert!(!result.text.contains('\u{202e}'));
        assert!(result.text.contains("красный"));
        assert!(result.text.contains('\n'));
        assert!(result.text.contains('\t'));
        assert!(
            result
                .text
                .contains("⚠️ Внешний модуль «Тест» (t v1.0) — код без песочницы.")
        );
    }

    #[test]
    fn external_result_truncates_when_text_overflows() {
        let long = "🦀".repeat(MAX_UTF16_UNITS);
        let result = Response::external_result(Locale::Russian, &long, "Длинный", "long", "0.1");
        assert!(result.text.contains("… вывод сокращён"));
        assert!(
            result
                .text
                .contains("⚠️ Внешний модуль «Длинный» (long v0.1) — код без песочницы.")
        );
        assert!(result.text.encode_utf16().count() <= MAX_UTF16_UNITS);
    }

    #[test]
    fn external_truncation_uses_selected_locale_suffix() {
        for (locale, suffix) in [
            (Locale::English, "… output truncated"),
            (Locale::Russian, "… вывод сокращён"),
        ] {
            let result =
                Response::external_result(locale, &"🦀".repeat(MAX_UTF16_UNITS), "Name", "id", "1");
            assert!(result.text.contains(suffix));
            assert!(result.text.encode_utf16().count() <= MAX_UTF16_UNITS);
        }
    }

    #[test]
    fn external_result_provenance_present_even_for_empty_text() {
        let result = Response::external_result(Locale::Russian, "", "Пусто", "empty", "0.0");
        assert!(
            result
                .text
                .contains("⚠️ Внешний модуль «Пусто» (empty v0.0) — код без песочницы.")
        );
        assert!(result.text.encode_utf16().count() <= MAX_UTF16_UNITS);
    }

    #[test]
    fn sanitize_removes_ansi_bidi_and_controls() {
        let cleaned = sanitize_external_output("a\x1b[1mb\x1b[0mc\u{200f}d\x00e");
        assert_eq!(cleaned, "abcde");
    }

    #[test]
    fn sanitize_preserves_newlines_and_tabs() {
        let cleaned = sanitize_external_output("line1\n\tindented\nline3");
        assert_eq!(cleaned, "line1\n\tindented\nline3");
    }

    #[test]
    fn sanitize_handles_ansi_without_terminator() {
        let cleaned = sanitize_external_output("hello\x1b[31m");
        assert_eq!(cleaned, "hello");
    }
}
