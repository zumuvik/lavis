use serde::{Deserialize, Serialize};

/// The deliberately small set of supported interface languages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Locale {
    #[serde(rename = "en", alias = "english")]
    English,
    #[default]
    #[serde(rename = "ru", alias = "russian")]
    Russian,
}

impl Locale {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "en" => Some(Self::English),
            "ru" => Some(Self::Russian),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Russian => "ru",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Text {
    LanguageChoose,
    LanguageCurrent,
    LanguageChanged,
    LanguageUsage,
    OnboardingSelect,
    OnboardingIntro,
    OnboardingPrefix,
    OnboardingPing,
    OnboardingHelp,
    OnboardingModules,
    OnboardingExternal,
    OnboardingCompanion,
    OnboardingDone,
    OnboardingSkipped,
    StartUsage,
    PostAuthInvite,
    PostAuthFallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeText {
    PingFailed,
    Rebooting,
    LmUnavailable,
    LmStateUnavailable,
    LmListUnavailable,
}

pub fn runtime_text(locale: Locale, key: RuntimeText) -> &'static str {
    match (locale, key) {
        (Locale::English, RuntimeText::PingFailed) => "⚠️ Telegram ping failed",
        (Locale::Russian, RuntimeText::PingFailed) => "⚠️ Не удалось выполнить Telegram ping",
        (Locale::English, RuntimeText::Rebooting) => "♻️ Lavis is restarting…",
        (Locale::Russian, RuntimeText::Rebooting) => "♻️ Lavis перезапускается…",
        (Locale::English, RuntimeText::LmUnavailable) => {
            "⚠️ External module management is unavailable."
        }
        (Locale::Russian, RuntimeText::LmUnavailable) => {
            "⚠️ Управление внешними модулями недоступно."
        }
        (Locale::English, RuntimeText::LmStateUnavailable) => {
            "⚠️ External module state is unavailable."
        }
        (Locale::Russian, RuntimeText::LmStateUnavailable) => {
            "⚠️ Состояние внешних модулей недоступно."
        }
        (Locale::English, RuntimeText::LmListUnavailable) => {
            "⚠️ Could not read the external module list."
        }
        (Locale::Russian, RuntimeText::LmListUnavailable) => {
            "⚠️ Не удалось прочитать список внешних модулей."
        }
    }
}

pub fn text(locale: Locale, key: Text) -> &'static str {
    match (locale, key) {
        (Locale::English, Text::LanguageChoose) => "Choose language: en or ru.",
        (Locale::Russian, Text::LanguageChoose) => "Выберите язык: en или ru.",
        (Locale::English, Text::LanguageCurrent) => "Current language",
        (Locale::Russian, Text::LanguageCurrent) => "Текущий язык",
        (Locale::English, Text::LanguageChanged) => "Language saved",
        (Locale::Russian, Text::LanguageChanged) => "Язык сохранён",
        (Locale::English, Text::LanguageUsage) => "Usage: language [en|ru]",
        (Locale::Russian, Text::LanguageUsage) => "Использование: language [en|ru]",
        (Locale::English, Text::OnboardingSelect) => {
            "Welcome to Lavis. Choose: {prefix}start en or {prefix}start ru."
        }
        (Locale::Russian, Text::OnboardingSelect) => {
            "Добро пожаловать в Lavis. Выберите: {prefix}start en или {prefix}start ru."
        }
        (Locale::English, Text::OnboardingIntro) => {
            "Welcome! Lavis accepts your own outgoing commands."
        }
        (Locale::Russian, Text::OnboardingIntro) => {
            "Добро пожаловать! Lavis принимает только ваши исходящие команды."
        }
        (Locale::English, Text::OnboardingPrefix) => {
            "Commands start with {prefix}. Change it with {prefix}prefix."
        }
        (Locale::Russian, Text::OnboardingPrefix) => {
            "Команды начинаются с {prefix}. Изменить его: {prefix}prefix."
        }
        (Locale::English, Text::OnboardingPing) => "Check Telegram latency with {prefix}ping.",
        (Locale::Russian, Text::OnboardingPing) => "Проверьте задержку Telegram: {prefix}ping.",
        (Locale::English, Text::OnboardingHelp) => {
            "Browse every built-in command with {prefix}help."
        }
        (Locale::Russian, Text::OnboardingHelp) => "Откройте встроенные команды: {prefix}help.",
        (Locale::English, Text::OnboardingModules) => {
            "Built-in modules are listed by {prefix}modules."
        }
        (Locale::Russian, Text::OnboardingModules) => "Встроенные модули покажет {prefix}modules.",
        (Locale::English, Text::OnboardingExternal) => {
            "External modules run code: inspect and confirm them carefully with {prefix}lm."
        }
        (Locale::Russian, Text::OnboardingExternal) => {
            "Внешние модули запускают код: внимательно проверяйте их через {prefix}lm."
        }
        (Locale::English, Text::OnboardingCompanion) => {
            "A companion bot creates and maintains the private forum workspace after confirmation. Start the single safe flow with {prefix}start bot."
        }
        (Locale::Russian, Text::OnboardingCompanion) => {
            "Companion-бот после подтверждения создаёт и поддерживает приватное форум-пространство. Единый безопасный запуск: {prefix}start bot."
        }
        (Locale::English, Text::OnboardingDone) => {
            "Tutorial complete. Restart it anytime with {prefix}start."
        }
        (Locale::Russian, Text::OnboardingDone) => {
            "Обучение завершено. Перезапустить его можно: {prefix}start."
        }
        (Locale::English, Text::OnboardingSkipped) => {
            "Tutorial skipped. Restart it anytime with {prefix}start."
        }
        (Locale::Russian, Text::OnboardingSkipped) => {
            "Обучение пропущено. Перезапустить его можно: {prefix}start."
        }
        (Locale::English, Text::StartUsage) => "Usage: {prefix}start [en|ru|skip|bot]",
        (Locale::Russian, Text::StartUsage) => "Использование: {prefix}start [en|ru|skip|bot]",
        (Locale::English, Text::PostAuthInvite) => {
            "Authorization complete. Send {prefix}start to begin your Lavis tutorial."
        }
        (Locale::Russian, Text::PostAuthInvite) => {
            "Авторизация завершена. Отправьте {prefix}start, чтобы начать обучение Lavis."
        }
        (Locale::English, Text::PostAuthFallback) => "Could not send the Telegram invitation.",
        (Locale::Russian, Text::PostAuthFallback) => "Не удалось отправить приглашение в Telegram.",
    }
}

pub fn bilingual(key: Text, prefix: &str) -> String {
    format!(
        "{}\n{}",
        text(Locale::English, key).replace("{prefix}", prefix),
        text(Locale::Russian, key).replace("{prefix}", prefix)
    )
}
