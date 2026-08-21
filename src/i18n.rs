use serde::{Deserialize, Serialize};

use crate::external_modules::manager::ExternalModuleRuntimeStatus;
use crate::external_modules::source_inspection::InspectionWarning;

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
    LanguageSaveFailed,
    OnboardingSaveFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeText {
    PingFailed,
    Rebooting,
    LmUnavailable,
    LmStateUnavailable,
    LmListUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebootText {
    Pending,
    ReceiptLookupFailed,
    ReceiptPreparationFailed,
    StartFailed,
    ReceiptArmFailed,
    Completed,
}

pub fn reboot_text(locale: Locale, key: RebootText, elapsed_seconds: Option<u64>) -> String {
    let template = match (locale, key) {
        (Locale::English, RebootText::Pending) => {
            "⚠️ A previous restart confirmation is still pending."
        }
        (Locale::Russian, RebootText::Pending) => {
            "⚠️ Уже ожидается подтверждение предыдущего перезапуска."
        }
        (Locale::English, RebootText::ReceiptLookupFailed) => {
            "⚠️ Could not check the restart confirmation."
        }
        (Locale::Russian, RebootText::ReceiptLookupFailed) => {
            "⚠️ Не удалось проверить подтверждение перезапуска."
        }
        (Locale::English, RebootText::ReceiptPreparationFailed) => {
            "⚠️ Could not prepare the restart confirmation."
        }
        (Locale::Russian, RebootText::ReceiptPreparationFailed) => {
            "⚠️ Не удалось подготовить подтверждение перезапуска."
        }
        (Locale::English, RebootText::StartFailed) => {
            "⚠️ Could not start the restart; Lavis is still running."
        }
        (Locale::Russian, RebootText::StartFailed) => {
            "⚠️ Не удалось начать перезапуск; Lavis продолжает работу."
        }
        (Locale::English, RebootText::ReceiptArmFailed) => {
            "⚠️ Could not save the restart confirmation; Lavis is still running."
        }
        (Locale::Russian, RebootText::ReceiptArmFailed) => {
            "⚠️ Не удалось сохранить подтверждение перезапуска; Lavis продолжает работу."
        }
        (Locale::English, RebootText::Completed) => {
            "✅ Lavis restarted\n\nRestart time: {elapsed} s"
        }
        (Locale::Russian, RebootText::Completed) => {
            "✅ Lavis перезагрузился\n\nВремя перезагрузки: {elapsed} с"
        }
    };
    template.replace(
        "{elapsed}",
        &elapsed_seconds.unwrap_or_default().to_string(),
    )
}

/// Lavis-owned response presentation text. Content supplied by external
/// modules is intentionally not included here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseText {
    Truncated,
    DocumentationTruncated,
    ExternalModuleProvenance,
}

pub fn response_text(locale: Locale, key: ResponseText) -> &'static str {
    match (locale, key) {
        (Locale::English, ResponseText::Truncated) => "… output truncated",
        (Locale::Russian, ResponseText::Truncated) => "… вывод сокращён",
        (Locale::English, ResponseText::DocumentationTruncated) => "… documentation truncated",
        (Locale::Russian, ResponseText::DocumentationTruncated) => "… описание сокращено",
        (Locale::English, ResponseText::ExternalModuleProvenance) => {
            "⚠️ External module «{name}» ({id} v{version}) — code runs without a sandbox."
        }
        (Locale::Russian, ResponseText::ExternalModuleProvenance) => {
            "⚠️ Внешний модуль «{name}» ({id} v{version}) — код без песочницы."
        }
    }
}

pub fn external_module_provenance(
    locale: Locale,
    display_name: &str,
    module_id: &str,
    version: &str,
) -> String {
    interpolate_response_template(
        response_text(locale, ResponseText::ExternalModuleProvenance),
        &[
            ("name", display_name),
            ("id", module_id),
            ("version", version),
        ],
    )
}

fn interpolate_response_template(template: &str, values: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(start) = remaining.find('{') {
        output.push_str(&remaining[..start]);
        let after_start = &remaining[start + 1..];
        let Some(end) = after_start.find('}') else {
            output.push_str(&remaining[start..]);
            return output;
        };
        let key = &after_start[..end];
        if let Some((_, value)) = values.iter().find(|(candidate, _)| *candidate == key) {
            output.push_str(value);
        } else {
            output.push('{');
            output.push_str(key);
            output.push('}');
        }
        remaining = &after_start[end + 1..];
    }
    output.push_str(remaining);
    output
}

/// Lavis-owned companion setup UI. BotFather's own replies are intentionally
/// never included here: they are parsed, but never translated or echoed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupText {
    Unavailable,
    SavedMessagesOnly,
    TimedOut,
    AlreadyActive,
    ExistingBot,
    NoActiveSetup,
    Cancelled,
    UsernamePrompt,
    UsernameGenerationFailed,
    UsernameInvalid,
    Usage,
    Plan,
    ConfirmOrCancel,
    WaitingBotFather,
    BotFatherUnavailable,
    BotFatherStartFailed,
    Started,
    BotCheckUnavailable,
    BotUsernameRejected,
    BotLimitReached,
    BotRetryLater,
    BotUnexpected,
    BotStorageFailed,
    BotTimedOut,
    BotVerificationFailed,
    Status,
    StatusIdle,
    StatusIdleValue,
    StatusUnavailable,
    NotConfigured,
    RepairCheckUnavailable,
    RepairStarted,
    RepairNoData,
    RepairDataUnsafe,
    RepairIdentityUnsafe,
    RepairTokenUnsafe,
    RepairTokenMismatch,
    RepairIdentitySaveFailed,
    ProvisionCompleted,
    ProvisionWithoutCommunity,
    ProvisionWithoutFolderCapacity,
    ProvisionWithoutFolderConflict,
    ProvisionFailed,
    StatusBotValidated,
    StatusComplete,
    StatusCompletedWithoutFolderCapacity,
    StatusCompletedWithoutFolderNameConflict,
    StatusCompanionAndCommunityConfigured,
    StatusCompanionConfiguredCommunityPending,
    StatusUnknown,
}

pub fn setup_text(locale: Locale, key: SetupText) -> &'static str {
    match (locale, key) {
        (Locale::English, SetupText::Unavailable) => "⚠️ Setup storage is unavailable.",
        (Locale::Russian, SetupText::Unavailable) => "⚠️ Хранилище настройки недоступно.",
        (Locale::English, SetupText::SavedMessagesOnly) => {
            "⚠️ Setup is available only in Saved Messages. Start it there with the active prefix."
        }
        (Locale::Russian, SetupText::SavedMessagesOnly) => {
            "⚠️ Настройка доступна только в Saved Messages. Запустите её там с активным префиксом."
        }
        (Locale::English, SetupText::TimedOut) => {
            "⌛ Setup timed out. Persistent data is retained; retry setup or setup repair."
        }
        (Locale::Russian, SetupText::TimedOut) => {
            "⌛ Настройка остановлена по таймауту. Постоянные данные сохранены; повторите setup или setup repair."
        }
        (Locale::English, SetupText::AlreadyActive) => {
            "⚠️ Setup is already active. Send cancel to stop it."
        }
        (Locale::Russian, SetupText::AlreadyActive) => {
            "⚠️ Настройка уже выполняется. Напишите cancel для отмены."
        }
        (Locale::English, SetupText::ExistingBot) => {
            "ℹ️ The bot already exists. Use setup repair to restore the workspace."
        }
        (Locale::Russian, SetupText::ExistingBot) => {
            "ℹ️ Бот уже создан. Используйте setup repair для восстановления workspace."
        }
        (Locale::English, SetupText::NoActiveSetup) => "ℹ️ No active setup to cancel.",
        (Locale::Russian, SetupText::NoActiveSetup) => "ℹ️ Нет активной настройки для отмены.",
        (Locale::English, SetupText::Cancelled) => "✅ Setup cancelled.",
        (Locale::Russian, SetupText::Cancelled) => "✅ Настройка отменена.",
        (Locale::English, SetupText::UsernamePrompt) => "🤖 Enter the bot username ending in _bot.",
        (Locale::Russian, SetupText::UsernamePrompt) => {
            "🤖 Введите желаемое имя бота, оканчивающееся на _bot."
        }
        (Locale::English, SetupText::UsernameGenerationFailed) => {
            "⚠️ Could not generate a bot username."
        }
        (Locale::Russian, SetupText::UsernameGenerationFailed) => {
            "⚠️ Не удалось сгенерировать имя бота."
        }
        (Locale::English, SetupText::UsernameInvalid) => {
            "⚠️ The username must contain 5–32 ASCII letters, digits, or _ and end in _bot."
        }
        (Locale::Russian, SetupText::UsernameInvalid) => {
            "⚠️ Имя должно содержать 5–32 ASCII-букв, цифр или _ и оканчиваться на _bot."
        }
        (Locale::English, SetupText::Usage) => {
            "⚠️ Usage: setup [auto|<username_bot>|status|repair|cancel]"
        }
        (Locale::Russian, SetupText::Usage) => {
            "⚠️ Использование: setup [auto|<username_bot>|status|repair|cancel]"
        }
        (Locale::English, SetupText::Plan) => {
            "📋 Setup plan\n\n• Create companion bot @{username} named «{display_name}».\n• Create or repair the private Lavis workspace.\n• Join your account to the official public community @lavis_userbot.\n• Add the workspace, bot, and community to the Lavis folder.\n\nSend confirm to proceed or cancel to stop."
        }
        (Locale::Russian, SetupText::Plan) => {
            "📋 План настройки\n\n• Создать companion-бота @{username} с именем «{display_name}».\n• Создать или восстановить приватный Lavis workspace.\n• Присоединить ваш Telegram-аккаунт к официальному публичному сообществу @lavis_userbot.\n• Добавить workspace, бота и сообщество в папку Lavis.\n\nНапишите confirm для подтверждения или cancel для отмены."
        }
        (Locale::English, SetupText::ConfirmOrCancel) => "⚠️ Send confirm or cancel.",
        (Locale::Russian, SetupText::ConfirmOrCancel) => "⚠️ Напишите confirm или cancel.",
        (Locale::English, SetupText::WaitingBotFather) => "ℹ️ Setup is waiting for BotFather.",
        (Locale::Russian, SetupText::WaitingBotFather) => "ℹ️ Настройка ожидает ответ BotFather.",
        (Locale::English, SetupText::BotFatherUnavailable) => "⚠️ Could not contact BotFather.",
        (Locale::Russian, SetupText::BotFatherUnavailable) => {
            "⚠️ Не удалось связаться с BotFather."
        }
        (Locale::English, SetupText::BotFatherStartFailed) => {
            "⚠️ Could not start the BotFather conversation."
        }
        (Locale::Russian, SetupText::BotFatherStartFailed) => {
            "⚠️ Не удалось начать диалог с BotFather."
        }
        (Locale::English, SetupText::Started) => "⏳ Setup started. Waiting for BotFather.",
        (Locale::Russian, SetupText::Started) => "⏳ Настройка начата. Ожидается ответ BotFather.",
        (Locale::English, SetupText::BotCheckUnavailable) => "⚠️ Bot verification is unavailable.",
        (Locale::Russian, SetupText::BotCheckUnavailable) => "⚠️ Проверка бота недоступна.",
        (Locale::English, SetupText::BotUsernameRejected) => {
            "⚠️ BotFather rejected the username. Enter another username ending in _bot."
        }
        (Locale::Russian, SetupText::BotUsernameRejected) => {
            "⚠️ BotFather отклонил имя. Введите другое имя, оканчивающееся на _bot."
        }
        (Locale::English, SetupText::BotLimitReached) => {
            "⚠️ BotFather reported that the bot limit was reached."
        }
        (Locale::Russian, SetupText::BotLimitReached) => {
            "⚠️ BotFather сообщил о достигнутом лимите ботов."
        }
        (Locale::English, SetupText::BotRetryLater) => "⚠️ BotFather asked you to retry later.",
        (Locale::Russian, SetupText::BotRetryLater) => {
            "⚠️ BotFather просит повторить попытку позже."
        }
        (Locale::English, SetupText::BotUnexpected) => {
            "⚠️ The BotFather conversation ended due to an unexpected response."
        }
        (Locale::Russian, SetupText::BotUnexpected) => {
            "⚠️ Диалог с BotFather завершился из-за неожиданного ответа."
        }
        (Locale::English, SetupText::BotStorageFailed) => {
            "⚠️ Bot data could not be saved safely. Setup stopped."
        }
        (Locale::Russian, SetupText::BotStorageFailed) => {
            "⚠️ Данные бота не удалось безопасно сохранить. Настройка остановлена."
        }
        (Locale::English, SetupText::BotTimedOut) => {
            "⚠️ BotFather did not respond in time. Setup stopped."
        }
        (Locale::Russian, SetupText::BotTimedOut) => {
            "⚠️ BotFather не ответил вовремя. Настройка остановлена."
        }
        (Locale::English, SetupText::BotVerificationFailed) => {
            "⚠️ Bot verification or storage failed. Setup stopped."
        }
        (Locale::Russian, SetupText::BotVerificationFailed) => {
            "⚠️ Проверка или сохранение бота завершились ошибкой. Настройка остановлена."
        }
        (Locale::English, SetupText::Status) => "⚙️ Setup status: {status}\nBot: {bot}",
        (Locale::Russian, SetupText::Status) => "⚙️ Состояние настройки: {status}\nБот: {bot}",
        (Locale::English, SetupText::StatusIdle) => "⚙️ Setup status: idle",
        (Locale::Russian, SetupText::StatusIdle) => "⚙️ Состояние настройки: бездействует",
        (Locale::English, SetupText::StatusIdleValue) => "idle",
        (Locale::Russian, SetupText::StatusIdleValue) => "бездействует",
        (Locale::English, SetupText::StatusUnavailable) => {
            "⚠️ Setup status is unavailable because local state could not be read safely."
        }
        (Locale::Russian, SetupText::StatusUnavailable) => {
            "⚠️ Состояние настройки недоступно: локальные данные нельзя безопасно прочитать."
        }
        (Locale::English, SetupText::NotConfigured) => "not configured",
        (Locale::Russian, SetupText::NotConfigured) => "не настроен",
        (Locale::English, SetupText::RepairCheckUnavailable) => {
            "⚠️ Saved bot verification is unavailable."
        }
        (Locale::Russian, SetupText::RepairCheckUnavailable) => {
            "⚠️ Проверка сохранённого бота недоступна."
        }
        (Locale::English, SetupText::RepairStarted) => "⏳ Companion workspace repair started.",
        (Locale::Russian, SetupText::RepairStarted) => {
            "⏳ Восстановление companion workspace начато."
        }
        (Locale::English, SetupText::RepairNoData) => "⚠️ No safe data is available for repair.",
        (Locale::Russian, SetupText::RepairNoData) => {
            "⚠️ Нет безопасных данных для восстановления."
        }
        (Locale::English, SetupText::RepairDataUnsafe) => {
            "⚠️ Saved data cannot be verified safely."
        }
        (Locale::Russian, SetupText::RepairDataUnsafe) => {
            "⚠️ Сохранённые данные нельзя безопасно проверить."
        }
        (Locale::English, SetupText::RepairIdentityUnsafe) => {
            "⚠️ Saved bot identity is incomplete or unsafe."
        }
        (Locale::Russian, SetupText::RepairIdentityUnsafe) => {
            "⚠️ Сохранённая идентификация бота неполна или небезопасна."
        }
        (Locale::English, SetupText::RepairTokenUnsafe) => {
            "⚠️ Saved token could not be verified safely. Repair was not started."
        }
        (Locale::Russian, SetupText::RepairTokenUnsafe) => {
            "⚠️ Сохранённый токен не удалось безопасно проверить. Восстановление не запущено."
        }
        (Locale::English, SetupText::RepairTokenMismatch) => {
            "⚠️ Saved token does not match the saved bot. Repair was not started."
        }
        (Locale::Russian, SetupText::RepairTokenMismatch) => {
            "⚠️ Сохранённый токен не соответствует сохранённому боту. Восстановление не запущено."
        }
        (Locale::English, SetupText::RepairIdentitySaveFailed) => {
            "⚠️ Verified bot identity could not be saved safely. Repair was not started."
        }
        (Locale::Russian, SetupText::RepairIdentitySaveFailed) => {
            "⚠️ Проверенный идентификатор бота не удалось безопасно сохранить. Восстановление не запущено."
        }
        (Locale::English, SetupText::ProvisionCompleted) => {
            "✅ Companion workspace and official community @lavis_userbot are configured."
        }
        (Locale::Russian, SetupText::ProvisionCompleted) => {
            "✅ Companion workspace и официальное сообщество @lavis_userbot настроены."
        }
        (Locale::English, SetupText::ProvisionWithoutCommunity) => {
            "⚠️ Companion workspace is ready, but joining @lavis_userbot failed. Retry {prefix}setup repair."
        }
        (Locale::Russian, SetupText::ProvisionWithoutCommunity) => {
            "⚠️ Companion workspace готов, но присоединиться к @lavis_userbot не удалось. Повторите {prefix}setup repair."
        }
        (Locale::English, SetupText::ProvisionWithoutFolderCapacity) => {
            "⚠️ Companion workspace is configured without a folder: the folder limit was reached. Retry {prefix}setup repair later."
        }
        (Locale::Russian, SetupText::ProvisionWithoutFolderCapacity) => {
            "⚠️ Companion workspace настроен без папки: достигнут лимит папок. Повторите {prefix}setup repair позже."
        }
        (Locale::English, SetupText::ProvisionWithoutFolderConflict) => {
            "⚠️ Companion workspace is configured without a folder: its name or ownership conflicts. Retry {prefix}setup repair after resolving it."
        }
        (Locale::Russian, SetupText::ProvisionWithoutFolderConflict) => {
            "⚠️ Companion workspace настроен без папки: папка занята или принадлежит другой настройке. Повторите {prefix}setup repair после устранения конфликта."
        }
        (Locale::English, SetupText::ProvisionFailed) => {
            "⚠️ Companion workspace repair did not finish. Retry {prefix}setup repair later."
        }
        (Locale::English, SetupText::StatusBotValidated) => "bot validated",
        (Locale::Russian, SetupText::StatusBotValidated) => "бот подтверждён",
        (Locale::English, SetupText::StatusComplete) => "complete",
        (Locale::Russian, SetupText::StatusComplete) => "завершено",
        (Locale::English, SetupText::StatusCompletedWithoutFolderCapacity) => {
            "complete without folder (folder limit reached)"
        }
        (Locale::Russian, SetupText::StatusCompletedWithoutFolderCapacity) => {
            "завершено без папки (достигнут лимит папок)"
        }
        (Locale::English, SetupText::StatusCompletedWithoutFolderNameConflict) => {
            "complete without folder (name or ownership conflict)"
        }
        (Locale::Russian, SetupText::StatusCompletedWithoutFolderNameConflict) => {
            "завершено без папки (конфликт имени или владельца)"
        }
        (Locale::English, SetupText::StatusCompanionAndCommunityConfigured) => {
            "companion workspace and community configured"
        }
        (Locale::Russian, SetupText::StatusCompanionAndCommunityConfigured) => {
            "companion workspace и сообщество настроены"
        }
        (Locale::English, SetupText::StatusCompanionConfiguredCommunityPending) => {
            "companion workspace configured; community pending"
        }
        (Locale::Russian, SetupText::StatusCompanionConfiguredCommunityPending) => {
            "companion workspace настроен; сообщество ожидает подключения"
        }
        (Locale::English, SetupText::StatusUnknown) => "unknown",
        (Locale::Russian, SetupText::StatusUnknown) => "неизвестно",
        (Locale::Russian, SetupText::ProvisionFailed) => {
            "⚠️ Восстановление companion workspace не завершено. Повторите {prefix}setup repair позже."
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefixText {
    Current,
    Changed,
    ChangeFailed,
    Reset,
    ResetFailed,
    Usage,
}

pub fn prefix_text(locale: Locale, key: PrefixText) -> &'static str {
    match (locale, key) {
        (Locale::English, PrefixText::Current) => "⚙️ Active prefix: {prefix}",
        (Locale::Russian, PrefixText::Current) => "⚙️ Текущий префикс: {prefix}",
        (Locale::English, PrefixText::Changed) => "⚙️ Command prefix set: {prefix}",
        (Locale::Russian, PrefixText::Changed) => "⚙️ Префикс команд изменён: {prefix}",
        (Locale::English, PrefixText::ChangeFailed) => "⚠️ Could not change prefix.",
        (Locale::Russian, PrefixText::ChangeFailed) => "⚠️ Не удалось изменить префикс.",
        (Locale::English, PrefixText::Reset) => "⚙️ Command prefix reset: {prefix}",
        (Locale::Russian, PrefixText::Reset) => "⚙️ Префикс сброшен: {prefix}",
        (Locale::English, PrefixText::ResetFailed) => "⚠️ Could not reset prefix.",
        (Locale::Russian, PrefixText::ResetFailed) => "⚠️ Не удалось сбросить префикс.",
        (Locale::English, PrefixText::Usage) => "⚠️ Usage: {prefix}prefix [new-prefix|reset]",
        (Locale::Russian, PrefixText::Usage) => {
            "⚠️ Использование: {prefix}prefix [new-prefix|reset]"
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AliasText {
    Empty,
    List,
    ListItem,
    Added,
    AddFailed,
    Deleted,
    NotFound,
    DeleteFailed,
    DoesNotExist,
    ShowHeading,
    Target,
    Usage,
}

pub fn alias_text(locale: Locale, key: AliasText) -> &'static str {
    match (locale, key) {
        (Locale::English, AliasText::Empty) => "🔗 No aliases configured",
        (Locale::Russian, AliasText::Empty) => "🔗 Псевдонимы не настроены",
        (Locale::English, AliasText::List) => "🔗 Aliases\n\n{items}",
        (Locale::Russian, AliasText::List) => "🔗 Псевдонимы\n\n{items}",
        (Locale::English, AliasText::ListItem) => "{prefix}{name} → {prefix}{target}{args}",
        (Locale::Russian, AliasText::ListItem) => "{prefix}{name} → {prefix}{target}{args}",
        (Locale::English, AliasText::Added) => "🔗 Added alias: {prefix}{name}",
        (Locale::Russian, AliasText::Added) => "🔗 Добавлен псевдоним: {prefix}{name}",
        (Locale::English, AliasText::AddFailed) => "⚠️ Could not add alias.",
        (Locale::Russian, AliasText::AddFailed) => "⚠️ Не удалось добавить псевдоним.",
        (Locale::English, AliasText::Deleted) => "🔗 Deleted alias: {prefix}{name}",
        (Locale::Russian, AliasText::Deleted) => "🔗 Псевдоним удалён: {prefix}{name}",
        (Locale::English, AliasText::NotFound) => "❓ Alias not found: {name}",
        (Locale::Russian, AliasText::NotFound) => "❓ Псевдоним не найден: {name}",
        (Locale::English, AliasText::DeleteFailed) => "⚠️ Could not delete alias.",
        (Locale::Russian, AliasText::DeleteFailed) => "⚠️ Не удалось удалить псевдоним.",
        (Locale::English, AliasText::DoesNotExist) => "⚠️ Alias does not exist: {prefix}{name}",
        (Locale::Russian, AliasText::DoesNotExist) => "⚠️ Псевдоним не существует: {prefix}{name}",
        (Locale::English, AliasText::ShowHeading) => "🔗 {prefix}{name}",
        (Locale::Russian, AliasText::ShowHeading) => "🔗 {prefix}{name}",
        (Locale::English, AliasText::Target) => "Alias for:\n{prefix}{target}{args}",
        (Locale::Russian, AliasText::Target) => "Псевдоним для:\n{prefix}{target}{args}",
        (Locale::English, AliasText::Usage) => {
            "⚠️ Usage: {prefix}alias [list|add <name> <command> [arguments...]|show <name>|del <name>]"
        }
        (Locale::Russian, AliasText::Usage) => {
            "⚠️ Использование: {prefix}alias [list|add <name> <command> [arguments...]|show <name>|del <name>]"
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FastfetchText {
    Empty,
    TimedOut,
    Unavailable,
    NonZero,
    UnexpectedStatus,
    InputTokenization,
    InputUnsupportedOption,
    InputMissingValue,
    InputDuplicateOption,
    InputInvalidLogo,
    InputInvalidStructure,
    InputInvalidSeparator,
    InputInvalidLogoPadding,
    ProfileNotReadable,
    ProfileMalformed,
    ProfileUnsupportedVersion,
    ProfileTooLarge,
    ProfileUnsafePath,
    ProfileInvalidLogo,
    ProfileInvalidStructure,
    ProfileInvalidSeparator,
    ProfileInvalidLogoPadding,
}

pub fn fastfetch_text(locale: Locale, key: FastfetchText) -> &'static str {
    match (locale, key) {
        (Locale::English, FastfetchText::Empty) => {
            "⚠️ Fastfetch produced no output. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::Empty) => {
            "⚠️ Fastfetch не вернул вывод. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::TimedOut) => {
            "⚠️ Fastfetch timed out. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::TimedOut) => {
            "⚠️ Fastfetch превысил время ожидания. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::Unavailable) => {
            "⚠️ Fastfetch is unavailable. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::Unavailable) => {
            "⚠️ Fastfetch недоступен. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::NonZero) => {
            "⚠️ Fastfetch failed (exit code {code}). See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::NonZero) => {
            "⚠️ Fastfetch завершился с кодом {code}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::UnexpectedStatus) => {
            "⚠️ Fastfetch ended unexpectedly. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::UnexpectedStatus) => {
            "⚠️ Fastfetch завершился неожиданно. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::InputTokenization) => {
            "⚠️ Fastfetch input error: invalid quoting. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::InputTokenization) => {
            "⚠️ Ошибка ввода Fastfetch: неверные кавычки. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::InputUnsupportedOption) => {
            "⚠️ Fastfetch input error: unsupported option. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::InputUnsupportedOption) => {
            "⚠️ Ошибка ввода Fastfetch: неподдерживаемый параметр. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::InputMissingValue) => {
            "⚠️ Fastfetch input error: option value is missing. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::InputMissingValue) => {
            "⚠️ Ошибка ввода Fastfetch: отсутствует значение параметра. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::InputDuplicateOption) => {
            "⚠️ Fastfetch input error: option is repeated. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::InputDuplicateOption) => {
            "⚠️ Ошибка ввода Fastfetch: параметр повторяется. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::InputInvalidLogo) => {
            "⚠️ Fastfetch input error: invalid --logo value. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::InputInvalidLogo) => {
            "⚠️ Ошибка ввода Fastfetch: неверное значение --logo. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::InputInvalidStructure) => {
            "⚠️ Fastfetch input error: invalid --structure value. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::InputInvalidStructure) => {
            "⚠️ Ошибка ввода Fastfetch: неверное значение --structure. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::InputInvalidSeparator) => {
            "⚠️ Fastfetch input error: invalid --separator value. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::InputInvalidSeparator) => {
            "⚠️ Ошибка ввода Fastfetch: неверное значение --separator. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::InputInvalidLogoPadding) => {
            "⚠️ Fastfetch input error: invalid --logo-padding value. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::InputInvalidLogoPadding) => {
            "⚠️ Ошибка ввода Fastfetch: неверное значение --logo-padding. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileNotReadable) => {
            "⚠️ Fastfetch profile cannot be read at {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileNotReadable) => {
            "⚠️ Не удалось прочитать профиль Fastfetch в {path}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileMalformed) => {
            "⚠️ Fastfetch profile is malformed at {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileMalformed) => {
            "⚠️ Профиль Fastfetch содержит ошибку в {path}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileUnsupportedVersion) => {
            "⚠️ Fastfetch profile version is unsupported at {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileUnsupportedVersion) => {
            "⚠️ Версия профиля Fastfetch не поддерживается в {path}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileTooLarge) => {
            "⚠️ Fastfetch profile is too large at {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileTooLarge) => {
            "⚠️ Профиль Fastfetch слишком большой в {path}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileUnsafePath) => {
            "⚠️ Fastfetch profile path is unsafe: {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileUnsafePath) => {
            "⚠️ Небезопасный путь профиля Fastfetch: {path}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileInvalidLogo) => {
            "⚠️ Fastfetch profile has an invalid logo at {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileInvalidLogo) => {
            "⚠️ Профиль Fastfetch содержит неверный логотип в {path}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileInvalidStructure) => {
            "⚠️ Fastfetch profile has an invalid structure at {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileInvalidStructure) => {
            "⚠️ Профиль Fastfetch содержит неверную структуру в {path}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileInvalidSeparator) => {
            "⚠️ Fastfetch profile has an invalid separator at {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileInvalidSeparator) => {
            "⚠️ Профиль Fastfetch содержит неверный разделитель в {path}. См. {prefix}help fastfetch"
        }
        (Locale::English, FastfetchText::ProfileInvalidLogoPadding) => {
            "⚠️ Fastfetch profile has invalid logo padding at {path}. See {prefix}help fastfetch"
        }
        (Locale::Russian, FastfetchText::ProfileInvalidLogoPadding) => {
            "⚠️ Профиль Fastfetch содержит неверный отступ логотипа в {path}. См. {prefix}help fastfetch"
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SensitiveText {
    RebootDenied,
    ModuleMutationDenied,
}

pub fn sensitive_text(locale: Locale, key: SensitiveText) -> &'static str {
    match (locale, key) {
        (Locale::English, SensitiveText::RebootDenied) => {
            "⚠️ Restart is available only from a new self-authored message."
        }
        (Locale::Russian, SensitiveText::RebootDenied) => {
            "⚠️ Перезапуск доступен только из нового собственного сообщения."
        }
        (Locale::English, SensitiveText::ModuleMutationDenied) => {
            "⚠️ This module operation is available only from a new self-authored message in Saved Messages."
        }
        (Locale::Russian, SensitiveText::ModuleMutationDenied) => {
            "⚠️ Эта операция с модулями доступна только из нового собственного сообщения в Saved Messages."
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PingText {
    Success,
}

pub fn ping_text(locale: Locale, key: PingText, latency: &str) -> String {
    let template = match (locale, key) {
        (Locale::English, PingText::Success) => "🏓 Pong: {latency}",
        (Locale::Russian, PingText::Success) => "🏓 Понг: {latency}",
    };
    template.replace("{latency}", latency)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatsText {
    Report,
    Unavailable,
}

pub fn stats_text(locale: Locale, key: StatsText) -> &'static str {
    match (locale, key) {
        (Locale::English, StatsText::Report) => {
            "📊 Lavis stats\n\nTelegram: {telegram}\nLavis uptime: {lavis_uptime}\nSystem uptime: {system_uptime}\nMemory: {memory}\nCommands: {commands}\nVersion: {version}"
        }
        (Locale::Russian, StatsText::Report) => {
            "📊 Статистика Lavis\n\nTelegram: {telegram}\nВремя работы Lavis: {lavis_uptime}\nВремя работы системы: {system_uptime}\nПамять: {memory}\nКоманды: {commands}\nВерсия: {version}"
        }
        (Locale::English, StatsText::Unavailable) => "unavailable",
        (Locale::Russian, StatsText::Unavailable) => "недоступно",
    }
}

pub fn render_stats_text(
    locale: Locale,
    telegram: &str,
    lavis_uptime: &str,
    system_uptime: &str,
    memory: &str,
    commands: u64,
    version: &str,
) -> String {
    stats_text(locale, StatsText::Report)
        .replace("{telegram}", telegram)
        .replace("{lavis_uptime}", lavis_uptime)
        .replace("{system_uptime}", system_uptime)
        .replace("{memory}", memory)
        .replace("{commands}", &commands.to_string())
        .replace("{version}", version)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InfoText {
    Caption,
    Unknown,
    Unavailable,
}

pub fn info_text(locale: Locale, key: InfoText) -> &'static str {
    match (locale, key) {
        (Locale::English, InfoText::Caption) => {
            "ℹ️ Lavis — really your userbot\n\nOwner: {owner}\nVersion: {version}\nCurrent commit: {commit}\nUpstream main: {upstream}\nPrefix: {prefix}\nModules: {total} ({active} active)\nHost: {host}\nOS: {os}"
        }
        (Locale::Russian, InfoText::Caption) => {
            "ℹ️ Lavis — really your userbot\n\nВладелец: {owner}\nВерсия: {version}\nТекущий коммит: {commit}\nОсновная ветка: {upstream}\nПрефикс: {prefix}\nМодули: {total} ({active} активных)\nХост: {host}\nОС: {os}"
        }
        (Locale::English, InfoText::Unknown) => "unknown",
        (Locale::Russian, InfoText::Unknown) => "неизвестно",
        (Locale::English, InfoText::Unavailable) => "unavailable",
        (Locale::Russian, InfoText::Unavailable) => "недоступно",
    }
}

pub struct InfoCaptionData<'a> {
    pub owner: &'a str,
    pub version: &'a str,
    pub commit: &'a str,
    pub upstream: &'a str,
    pub prefix: &'a str,
    pub active_modules: usize,
    pub total_modules: usize,
    pub host: &'a str,
    pub os: &'a str,
}

pub fn render_info_text(locale: Locale, info: InfoCaptionData<'_>) -> String {
    info_text(locale, InfoText::Caption)
        .replace("{owner}", info.owner)
        .replace("{version}", info.version)
        .replace("{commit}", info.commit)
        .replace("{upstream}", info.upstream)
        .replace("{prefix}", info.prefix)
        .replace("{active}", &info.active_modules.to_string())
        .replace("{total}", &info.total_modules.to_string())
        .replace("{host}", info.host)
        .replace("{os}", info.os)
}

/// Lavis-owned framing for an external command result. Module-provided text is
/// deliberately not catalogued or translated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalCommandText {
    RuntimeUnavailable,
    MissingDescriptor,
    Unavailable,
    Timeout,
    ProtocolDecode,
    WrongRequestId,
    ModuleError,
    ResultTooLarge,
    GenericError,
}

pub fn external_command_text(
    locale: Locale,
    key: ExternalCommandText,
    module_id: &str,
    detail: Option<&str>,
) -> String {
    let template = match (locale, key) {
        (Locale::English, ExternalCommandText::RuntimeUnavailable) => {
            "⚠️ External modules are unavailable."
        }
        (Locale::Russian, ExternalCommandText::RuntimeUnavailable) => {
            "⚠️ Внешние модули недоступны."
        }
        (Locale::English, ExternalCommandText::MissingDescriptor) => {
            "⚠️ Module «{id}» is missing from the module catalog."
        }
        (Locale::Russian, ExternalCommandText::MissingDescriptor) => {
            "⚠️ Модуль «{id}» отсутствует в каталоге модулей."
        }
        (Locale::English, ExternalCommandText::Unavailable) => {
            "⚠️ Module «{id}» is unavailable or stopped with an error."
        }
        (Locale::Russian, ExternalCommandText::Unavailable) => {
            "⚠️ Модуль «{id}» недоступен или завершился с ошибкой."
        }
        (Locale::English, ExternalCommandText::Timeout) => {
            "⚠️ Module «{id}» did not respond in time."
        }
        (Locale::Russian, ExternalCommandText::Timeout) => "⚠️ Модуль «{id}» не ответил вовремя.",
        (Locale::English, ExternalCommandText::ProtocolDecode) => {
            "⚠️ Module «{id}» sent an invalid response."
        }
        (Locale::Russian, ExternalCommandText::ProtocolDecode) => {
            "⚠️ Модуль «{id}» прислал некорректный ответ."
        }
        (Locale::English, ExternalCommandText::WrongRequestId) => {
            "⚠️ Module «{id}» responded with the wrong request ID."
        }
        (Locale::Russian, ExternalCommandText::WrongRequestId) => {
            "⚠️ Модуль «{id}» прислал ответ с неверным идентификатором запроса."
        }
        (Locale::English, ExternalCommandText::ModuleError) => {
            "⚠️ Module «{id}» reported an execution error."
        }
        (Locale::Russian, ExternalCommandText::ModuleError) => {
            "⚠️ Модуль «{id}» сообщил об ошибке выполнения."
        }
        (Locale::English, ExternalCommandText::ResultTooLarge) => {
            "⚠️ Module «{id}» returned a result that is too large."
        }
        (Locale::Russian, ExternalCommandText::ResultTooLarge) => {
            "⚠️ Результат модуля «{id}» слишком большой."
        }
        (Locale::English, ExternalCommandText::GenericError) => "⚠️ Module error «{id}»: {detail}",
        (Locale::Russian, ExternalCommandText::GenericError) => "⚠️ Ошибка модуля «{id}»: {detail}",
    };
    template
        .replace("{id}", module_id)
        .replace("{detail}", detail.unwrap_or_default())
}

/// Stable labels and status messages owned by Lavis's module-management UI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LmText {
    Usage,
    Unavailable,
    StateUnavailable,
    ListUnavailable,
    Empty,
    ListHeading,
    RuntimeUnavailable,
    NoRuntimeError,
    LastModuleError,
    DoctorHeading,
    DoctorModuleHeading,
    DoctorEmpty,
    NotFound,
    InvalidManifest,
    NotInstalled,
    NotInstalledOrUnavailable,
    StateChanged,
    StateUnchanged,
    Declarative,
    StateChangeFailed,
    InstallUnavailable,
    AttachPackage,
    UnsafePackage,
    PlanUnavailable,
    ApprovalInvalid,
    AlreadyRegistered,
    VerifiedPackageUnavailable,
    InstallRollbackFailed,
    InstallValidationFailed,
    InstallFailed,
    RegistrationConflict,
    InstalledDisabled,
    Cancelled,
    SavedMessagesOnly,
    InstallPlan,
}

pub fn lm_text(locale: Locale, key: LmText) -> &'static str {
    match (locale, key) {
        (Locale::English, LmText::Usage) => {
            "⚠️ Usage:\n{prefix}lm\n{prefix}lm list\n{prefix}lm info <id>\n{prefix}lm logs <id>\n{prefix}lm doctor [<id>]\n{prefix}lm install\n{prefix}lm confirm <ApprovalId>\n{prefix}lm cancel <ApprovalId>\n{prefix}lm enable <id>\n{prefix}lm disable <id>"
        }
        (Locale::Russian, LmText::Usage) => {
            "⚠️ Использование:\n{prefix}lm\n{prefix}lm list\n{prefix}lm info <id>\n{prefix}lm logs <id>\n{prefix}lm doctor [<id>]\n{prefix}lm install\n{prefix}lm confirm <ApprovalId>\n{prefix}lm cancel <ApprovalId>\n{prefix}lm enable <id>\n{prefix}lm disable <id>"
        }
        (Locale::English, LmText::Unavailable) => "⚠️ External module management is unavailable.",
        (Locale::Russian, LmText::Unavailable) => "⚠️ Управление внешними модулями недоступно.",
        (Locale::English, LmText::StateUnavailable) => "⚠️ External module state is unavailable.",
        (Locale::Russian, LmText::StateUnavailable) => "⚠️ Состояние внешних модулей недоступно.",
        (Locale::English, LmText::ListUnavailable) => "⚠️ Could not read the external module list.",
        (Locale::Russian, LmText::ListUnavailable) => {
            "⚠️ Не удалось прочитать список внешних модулей."
        }
        (Locale::English, LmText::Empty) => {
            "📦 No external modules are installed.\n\nAttach a .lmod document to a message, then use:\n{prefix}lm install"
        }
        (Locale::Russian, LmText::Empty) => {
            "📦 Внешние модули не установлены.\n\nПрикрепите .lmod к сообщению, затем используйте:\n{prefix}lm install"
        }
        (Locale::English, LmText::ListHeading) => "📦 External modules",
        (Locale::Russian, LmText::ListHeading) => "📦 Внешние модули",
        (Locale::English, LmText::RuntimeUnavailable) => {
            "⚠️ External module runtime is unavailable."
        }
        (Locale::Russian, LmText::RuntimeUnavailable) => "⚠️ Runtime внешних модулей недоступен.",
        (Locale::English, LmText::NoRuntimeError) => {
            "ℹ️ Module {id} has no retained runtime error."
        }
        (Locale::Russian, LmText::NoRuntimeError) => {
            "ℹ️ Для модуля {id} нет сохранённой runtime-ошибки."
        }
        (Locale::English, LmText::LastModuleError) => "📋 Last module error: {id}\n\n{detail}",
        (Locale::Russian, LmText::LastModuleError) => "📋 Последняя ошибка модуля {id}\n\n{detail}",
        (Locale::English, LmText::DoctorHeading) => "🩺 External module diagnostics",
        (Locale::Russian, LmText::DoctorHeading) => "🩺 Диагностика внешних модулей",
        (Locale::English, LmText::DoctorModuleHeading) => "🩺 Module diagnostics: {id}",
        (Locale::Russian, LmText::DoctorModuleHeading) => "🩺 Диагностика модуля {id}",
        (Locale::English, LmText::DoctorEmpty) => "No external modules are installed.",
        (Locale::Russian, LmText::DoctorEmpty) => "Внешние модули не установлены.",
        (Locale::English, LmText::NotFound) => "ℹ️ Module {id} was not found.",
        (Locale::Russian, LmText::NotFound) => "ℹ️ Модуль {id} не найден.",
        (Locale::English, LmText::InvalidManifest) => {
            "⚠️ The installed module manifest is invalid."
        }
        (Locale::Russian, LmText::InvalidManifest) => {
            "⚠️ Манифест установленного модуля некорректен."
        }
        (Locale::English, LmText::NotInstalled) => "⚠️ Module is not installed.",
        (Locale::Russian, LmText::NotInstalled) => "⚠️ Модуль не установлен.",
        (Locale::English, LmText::NotInstalledOrUnavailable) => {
            "⚠️ Module is not installed or unavailable."
        }
        (Locale::Russian, LmText::NotInstalledOrUnavailable) => {
            "⚠️ Модуль не установлен или недоступен."
        }
        (Locale::English, LmText::Declarative) => {
            "⚠️ This module is managed declaratively by NixOS. Change services.lavis.extensions and run nh os switch."
        }
        (Locale::Russian, LmText::Declarative) => {
            "⚠️ Модуль управляется декларативно через NixOS. Измените services.lavis.extensions и выполните nh os switch."
        }
        (Locale::English, LmText::StateChangeFailed) => "⚠️ Could not change module state.",
        (Locale::Russian, LmText::StateChangeFailed) => "⚠️ Не удалось изменить состояние модуля.",
        (Locale::English, LmText::InstallUnavailable) => {
            "⚠️ External module installation is unavailable."
        }
        (Locale::Russian, LmText::InstallUnavailable) => "⚠️ Установка внешних модулей недоступна.",
        (Locale::English, LmText::AttachPackage) => {
            "⚠️ Attach a .lmod document to a new self-authored message in Saved Messages."
        }
        (Locale::Russian, LmText::AttachPackage) => {
            "⚠️ Прикрепите документ .lmod к новому собственному сообщению в Saved Messages."
        }
        (Locale::English, LmText::UnsafePackage) => {
            "⚠️ The .lmod package did not pass safe inspection."
        }
        (Locale::Russian, LmText::UnsafePackage) => "⚠️ Пакет .lmod не прошёл безопасную проверку.",
        (Locale::English, LmText::PlanUnavailable) => "⚠️ The installation plan is unavailable.",
        (Locale::Russian, LmText::PlanUnavailable) => "⚠️ План установки недоступен.",
        (Locale::English, LmText::ApprovalInvalid) => "⚠️ ApprovalId is invalid or expired.",
        (Locale::Russian, LmText::ApprovalInvalid) => "⚠️ ApprovalId недействителен или истёк.",
        (Locale::English, LmText::AlreadyRegistered) => {
            "⚠️ Module «{id}» is already registered; installation was not started."
        }
        (Locale::Russian, LmText::AlreadyRegistered) => {
            "⚠️ Модуль «{id}» уже зарегистрирован; установка не начата."
        }
        (Locale::English, LmText::VerifiedPackageUnavailable) => {
            "⚠️ The verified package is unavailable."
        }
        (Locale::Russian, LmText::VerifiedPackageUnavailable) => "⚠️ Проверенный пакет недоступен.",
        (Locale::English, LmText::InstallRollbackFailed) => {
            "⚠️ Installation did not finish: target rollback failed; inspect the module directory manually."
        }
        (Locale::Russian, LmText::InstallRollbackFailed) => {
            "⚠️ Установка не завершена: откат цели не удался; проверьте каталог модулей вручную."
        }
        (Locale::English, LmText::InstallValidationFailed) => {
            "⚠️ Installation was not completed: final validation failed and the target was removed."
        }
        (Locale::Russian, LmText::InstallValidationFailed) => {
            "⚠️ Установка не выполнена: финальная проверка не пройдена, цель удалена."
        }
        (Locale::English, LmText::InstallFailed) => "⚠️ Installation was not completed.",
        (Locale::Russian, LmText::InstallFailed) => "⚠️ Установка не выполнена.",
        (Locale::English, LmText::RegistrationConflict) => {
            "⚠️ Module «{id}» was installed but not registered because its descriptor conflicts."
        }
        (Locale::Russian, LmText::RegistrationConflict) => {
            "⚠️ Модуль «{id}» установлен, но не зарегистрирован из-за конфликтующего описания."
        }
        (Locale::English, LmText::InstalledDisabled) => {
            "✅ Module «{id}» was installed and is disabled."
        }
        (Locale::Russian, LmText::InstalledDisabled) => "✅ Модуль «{id}» установлен и выключен.",
        (Locale::English, LmText::Cancelled) => "✅ Installation plan cancelled.",
        (Locale::Russian, LmText::Cancelled) => "✅ План установки отменён.",
        (Locale::English, LmText::SavedMessagesOnly) => {
            "⚠️ This module operation is available only from a new self-authored message in Saved Messages."
        }
        (Locale::Russian, LmText::SavedMessagesOnly) => {
            "⚠️ Эта операция с модулями доступна только из нового собственного сообщения в Saved Messages."
        }
        (Locale::English, LmText::InstallPlan) => {
            "📋 Installation plan\n\nSource: {source}\nModule: {module} v{version}\nProtocol: {protocol}\nEntrypoint: {entrypoint}\nDefault command: {default_command}\nSHA-256: {sha256}\nFingerprint: {fingerprint}\nArchive: {archive_bytes} bytes, files: {file_count}, compressed: {compressed_bytes} bytes, expanded: {expanded_bytes} bytes\nCapabilities: {capabilities}\nSubscriptions: {subscriptions}\nTelegram V6 methods: {methods}\nActions: {actions}\nWarnings: {warnings}\n\nApprovalId: {approval_id}\nConfirm: {prefix}lm confirm {approval_id}\nCancel: {prefix}lm cancel {approval_id}\nExpires in: 10 minutes."
        }
        (Locale::Russian, LmText::InstallPlan) => {
            "📋 План установки\n\nИсточник: {source}\nМодуль: {module} v{version}\nПротокол: {protocol}\nТочка входа: {entrypoint}\nКоманда по умолчанию: {default_command}\nSHA-256: {sha256}\nОтпечаток: {fingerprint}\nАрхив: {archive_bytes} Байт, файлов: {file_count}, сжато: {compressed_bytes} Байт, распаковано: {expanded_bytes} Байт\nВозможности: {capabilities}\nПодписки: {subscriptions}\nМетоды Telegram V6: {methods}\nДействия: {actions}\nПредупреждения: {warnings}\n\nApprovalId: {approval_id}\nПодтвердите: {prefix}lm confirm {approval_id}\nОтменить: {prefix}lm cancel {approval_id}\nСрок действия: 10 минут."
        }
        (Locale::English, LmText::StateChanged) => {
            "✅ Module «{id}» is now {detail}.\n\nRun {prefix}reboot to apply the change."
        }
        (Locale::Russian, LmText::StateChanged) => {
            "✅ Модуль «{id}» {detail}.\n\nДля применения изменений выполните:\n{prefix}reboot"
        }
        (Locale::English, LmText::StateUnchanged) => "ℹ️ Module «{id}» is already {detail}.",
        (Locale::Russian, LmText::StateUnchanged) => "ℹ️ Модуль «{id}» уже {detail}.",
    }
}

pub fn lm_format(locale: Locale, key: LmText, id: &str, detail: &str) -> String {
    interpolate_lm_template(
        lm_text(locale, key),
        &[("id", id), ("detail", detail), ("prefix", detail)],
    )
}

pub fn lm_state_text(locale: Locale, key: LmText, id: &str, state: &str, prefix: &str) -> String {
    interpolate_lm_template(
        lm_text(locale, key),
        &[("id", id), ("detail", state), ("prefix", prefix)],
    )
}

pub struct LmInstallPlanText<'a> {
    pub source: &'a str,
    pub module: &'a str,
    pub version: &'a str,
    pub protocol: u32,
    pub entrypoint: &'a str,
    pub default_command: &'a str,
    pub sha256: &'a str,
    pub fingerprint: &'a str,
    pub archive_bytes: u64,
    pub file_count: usize,
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
    pub capabilities: &'a str,
    pub subscriptions: &'a str,
    pub methods: &'a str,
    pub actions: &'a str,
    pub warnings: &'a str,
    pub approval_id: &'a str,
    pub prefix: &'a str,
}

pub fn render_lm_install_plan(locale: Locale, plan: LmInstallPlanText<'_>) -> String {
    let protocol = plan.protocol.to_string();
    let archive_bytes = plan.archive_bytes.to_string();
    let file_count = plan.file_count.to_string();
    let compressed_bytes = plan.compressed_bytes.to_string();
    let expanded_bytes = plan.expanded_bytes.to_string();
    interpolate_lm_template(
        lm_text(locale, LmText::InstallPlan),
        &[
            ("source", plan.source),
            ("module", plan.module),
            ("version", plan.version),
            ("entrypoint", plan.entrypoint),
            ("default_command", plan.default_command),
            ("sha256", plan.sha256),
            ("fingerprint", plan.fingerprint),
            ("capabilities", plan.capabilities),
            ("subscriptions", plan.subscriptions),
            ("methods", plan.methods),
            ("actions", plan.actions),
            ("warnings", plan.warnings),
            ("approval_id", plan.approval_id),
            ("prefix", plan.prefix),
            ("protocol", protocol.as_str()),
            ("archive_bytes", archive_bytes.as_str()),
            ("file_count", file_count.as_str()),
            ("compressed_bytes", compressed_bytes.as_str()),
            ("expanded_bytes", expanded_bytes.as_str()),
        ],
    )
}

/// Substitutes only placeholders in the trusted catalog template. Values are
/// appended verbatim and are never parsed as templates themselves.
fn interpolate_lm_template(template: &str, values: &[(&str, &str)]) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(start) = remaining.find('{') {
        rendered.push_str(&remaining[..start]);
        let after_start = &remaining[start + 1..];
        let Some(end) = after_start.find('}') else {
            rendered.push_str(&remaining[start..]);
            return rendered;
        };
        let key = &after_start[..end];
        if let Some((_, value)) = values.iter().find(|(candidate, _)| *candidate == key) {
            rendered.push_str(value);
        } else {
            rendered.push('{');
            rendered.push_str(key);
            rendered.push('}');
        }
        remaining = &after_start[end + 1..];
    }
    rendered.push_str(remaining);
    rendered
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LmLabel {
    Enabled,
    Disabled,
    Manual,
    Declarative,
    None,
    InvalidId,
    InvalidManifest,
    NotRunning,
    MissingCatalog,
    Status,
    Source,
    Runtime,
    Author,
    Commands,
    Id,
    Version,
    Management,
    Diagnostic,
    LastFailure,
    LastError,
    Capabilities,
    Entrypoint,
    Protocol,
    ProvidedCommands,
    InstallationPlan,
    Archive,
    Repository,
    Warnings,
}

pub fn lm_label(locale: Locale, key: LmLabel) -> &'static str {
    match (locale, key) {
        (Locale::English, LmLabel::Enabled) => "enabled",
        (Locale::Russian, LmLabel::Enabled) => "включён",
        (Locale::English, LmLabel::Disabled) => "disabled",
        (Locale::Russian, LmLabel::Disabled) => "отключён",
        (Locale::English, LmLabel::Manual) => "manual",
        (Locale::Russian, LmLabel::Manual) => "вручную",
        (Locale::English, LmLabel::Declarative) => "NixOS (declarative)",
        (Locale::Russian, LmLabel::Declarative) => "NixOS (декларативно)",
        (Locale::English, LmLabel::None) => "none",
        (Locale::Russian, LmLabel::None) => "нет",
        (Locale::English, LmLabel::InvalidId) => "invalid ID",
        (Locale::Russian, LmLabel::InvalidId) => "некорректный ID",
        (Locale::English, LmLabel::InvalidManifest) => "invalid manifest",
        (Locale::Russian, LmLabel::InvalidManifest) => "некорректный манифест",
        (Locale::English, LmLabel::NotRunning) => "not running",
        (Locale::Russian, LmLabel::NotRunning) => "не запущен",
        (Locale::English, LmLabel::MissingCatalog) => "enabled, but missing from the catalog",
        (Locale::Russian, LmLabel::MissingCatalog) => "включён, но каталог отсутствует",
        (Locale::English, LmLabel::Status) => "Status",
        (Locale::Russian, LmLabel::Status) => "Состояние",
        (Locale::English, LmLabel::Source) => "Source",
        (Locale::Russian, LmLabel::Source) => "Источник",
        (Locale::English, LmLabel::Runtime) => "Runtime",
        (Locale::Russian, LmLabel::Runtime) => "Runtime",
        (Locale::English, LmLabel::Author) => "Author",
        (Locale::Russian, LmLabel::Author) => "Автор",
        (Locale::English, LmLabel::Commands) => "Commands",
        (Locale::Russian, LmLabel::Commands) => "Команд",
        (Locale::English, LmLabel::Id) => "ID",
        (Locale::Russian, LmLabel::Id) => "ID",
        (Locale::English, LmLabel::Version) => "Version",
        (Locale::Russian, LmLabel::Version) => "Версия",
        (Locale::English, LmLabel::Management) => "Management",
        (Locale::Russian, LmLabel::Management) => "Управление",
        (Locale::English, LmLabel::Diagnostic) => "Diagnostic",
        (Locale::Russian, LmLabel::Diagnostic) => "Диагностика",
        (Locale::English, LmLabel::LastFailure) => "Last failure",
        (Locale::Russian, LmLabel::LastFailure) => "Последний сбой",
        (Locale::English, LmLabel::LastError) => "Last error",
        (Locale::Russian, LmLabel::LastError) => "Последняя ошибка",
        (Locale::English, LmLabel::Capabilities) => "Capabilities",
        (Locale::Russian, LmLabel::Capabilities) => "Возможности",
        (Locale::English, LmLabel::Entrypoint) => "Entrypoint",
        (Locale::Russian, LmLabel::Entrypoint) => "Точка входа",
        (Locale::English, LmLabel::Protocol) => "Schema/API protocol",
        (Locale::Russian, LmLabel::Protocol) => "Schema/API protocol",
        (Locale::English, LmLabel::ProvidedCommands) => "Provided commands",
        (Locale::Russian, LmLabel::ProvidedCommands) => "Предоставляемые команды",
        (Locale::English, LmLabel::InstallationPlan) => "📋 Installation plan",
        (Locale::Russian, LmLabel::InstallationPlan) => "📋 План установки",
        (Locale::English, LmLabel::Archive) => "archive .lmod",
        (Locale::Russian, LmLabel::Archive) => "архив .lmod",
        (Locale::English, LmLabel::Repository) => "repository",
        (Locale::Russian, LmLabel::Repository) => "репозиторий",
        (Locale::English, LmLabel::Warnings) => "Warnings",
        (Locale::Russian, LmLabel::Warnings) => "Предупреждения",
    }
}

pub fn lm_runtime_status(locale: Locale, status: ExternalModuleRuntimeStatus) -> &'static str {
    match (locale, status) {
        (Locale::English, ExternalModuleRuntimeStatus::Running) => "running",
        (Locale::Russian, ExternalModuleRuntimeStatus::Running) => "активен",
        (Locale::English, ExternalModuleRuntimeStatus::Failed) => "failed",
        (Locale::Russian, ExternalModuleRuntimeStatus::Failed) => "ошибка",
        (Locale::English, ExternalModuleRuntimeStatus::Terminated) => "terminated",
        (Locale::Russian, ExternalModuleRuntimeStatus::Terminated) => "остановлен",
        (Locale::English, ExternalModuleRuntimeStatus::InstalledDisabled) => "installed, disabled",
        (Locale::Russian, ExternalModuleRuntimeStatus::InstalledDisabled) => "установлен, выключен",
    }
}

pub fn inspection_warning_text(locale: Locale, warning: &InspectionWarning) -> &'static str {
    match (locale, warning) {
        (Locale::English, InspectionWarning::StoredOnlyArchive) => {
            "archive uses stored (uncompressed) entries"
        }
        (Locale::Russian, InspectionWarning::StoredOnlyArchive) => {
            "архив содержит записи без сжатия"
        }
        (Locale::English, InspectionWarning::TelegramRawNotSandboxed) => {
            "raw Telegram RPC access is not sandboxed"
        }
        (Locale::Russian, InspectionWarning::TelegramRawNotSandboxed) => {
            "доступ к raw Telegram RPC не изолирован песочницей"
        }
    }
}

pub struct LmInfoResponse<'a> {
    pub display_name: &'a str,
    pub id: &'a str,
    pub version: &'a str,
    pub author: &'a str,
    pub enabled: &'a str,
    pub management: &'a str,
    pub entrypoint: &'a str,
    pub protocol_version: u32,
    pub capabilities: &'a str,
    pub commands: &'a str,
    pub runtime: &'a str,
    pub diagnostic: Option<&'a str>,
}

pub fn render_lm_info(locale: Locale, info: LmInfoResponse<'_>) -> String {
    format!(
        "📦 {display_name}\n{id_label}: {id}\n{version_label}: {version}\n{author_label}: {author}\n{status_label}: {enabled}\n{source_label}: {management}\n{entrypoint_label}: {entrypoint}\n{protocol_label}: v{protocol_version}\n{capabilities_label}: {capabilities}\n{commands_label}: {commands}\n{runtime_label}: {runtime}\n{last_failure_label}: {diagnostic}",
        display_name = info.display_name,
        id_label = lm_label(locale, LmLabel::Id),
        id = info.id,
        version_label = lm_label(locale, LmLabel::Version),
        version = info.version,
        author_label = lm_label(locale, LmLabel::Author),
        author = info.author,
        status_label = lm_label(locale, LmLabel::Status),
        enabled = info.enabled,
        source_label = lm_label(locale, LmLabel::Source),
        management = info.management,
        entrypoint_label = lm_label(locale, LmLabel::Entrypoint),
        entrypoint = info.entrypoint,
        protocol_label = lm_label(locale, LmLabel::Protocol),
        protocol_version = info.protocol_version,
        capabilities_label = lm_label(locale, LmLabel::Capabilities),
        capabilities = info.capabilities,
        commands_label = lm_label(locale, LmLabel::ProvidedCommands),
        commands = info.commands,
        runtime_label = lm_label(locale, LmLabel::Runtime),
        runtime = info.runtime,
        last_failure_label = lm_label(locale, LmLabel::LastError),
        diagnostic = info.diagnostic.unwrap_or(lm_label(locale, LmLabel::None)),
    )
}

pub fn render_lm_doctor_module(
    locale: Locale,
    display_name: &str,
    id: &str,
    enabled: &str,
    runtime: &str,
    management: &str,
    diagnostic: Option<&str>,
) -> String {
    let diagnostic = diagnostic.map(|diagnostic| {
        format!(
            "\n  {}: {diagnostic}",
            lm_label(locale, LmLabel::LastFailure)
        )
    });
    format!(
        "• {display_name}\n  {}: {id}\n  {}: {enabled}\n  {}: {runtime}\n  {}: {management}{}",
        lm_label(locale, LmLabel::Id),
        lm_label(locale, LmLabel::Status),
        lm_label(locale, LmLabel::Runtime),
        lm_label(locale, LmLabel::Management),
        diagnostic.unwrap_or_default(),
    )
}

pub fn render_lm_doctor_missing_catalog(locale: Locale, id: &str) -> String {
    format!(
        "• {id}\n  {}: {}",
        lm_label(locale, LmLabel::Status),
        lm_label(locale, LmLabel::MissingCatalog)
    )
}

pub fn render_lm_doctor_report(locale: Locale, target: Option<&str>, entries: &[String]) -> String {
    let heading = match target {
        Some(id) => lm_format(locale, LmText::DoctorModuleHeading, id, ""),
        None => lm_text(locale, LmText::DoctorHeading).to_owned(),
    };
    let body = if entries.is_empty() {
        lm_text(locale, LmText::DoctorEmpty).to_owned()
    } else {
        entries.join("\n\n")
    };
    format!("{heading}\n\n{body}")
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
        (Locale::English, Text::LanguageSaveFailed) => "⚠️ Could not save language.",
        (Locale::Russian, Text::LanguageSaveFailed) => "⚠️ Не удалось сохранить язык.",
        (Locale::English, Text::OnboardingSaveFailed) => "⚠️ Could not save tutorial progress.",
        (Locale::Russian, Text::OnboardingSaveFailed) => {
            "⚠️ Не удалось сохранить прогресс обучения."
        }
    }
}

pub fn bilingual(key: Text, prefix: &str) -> String {
    format!(
        "{}\n{}",
        text(Locale::English, key).replace("{prefix}", prefix),
        text(Locale::Russian, key).replace("{prefix}", prefix)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ExternalCommandText, InfoCaptionData, InfoText, LmInstallPlanText, LmText, Locale,
        RebootText, Text, external_command_text, info_text, lm_format, lm_runtime_status, lm_text,
        reboot_text, render_info_text, render_lm_install_plan, text,
    };
    use crate::external_modules::manager::ExternalModuleRuntimeStatus;

    fn contains_cyrillic(value: &str) -> bool {
        value
            .chars()
            .any(|character| ('\u{0400}'..='\u{04ff}').contains(&character))
    }

    #[test]
    fn external_command_categories_are_localized_without_translating_detail() {
        for (key, expected_english, expected_russian) in [
            (
                ExternalCommandText::RuntimeUnavailable,
                "⚠️ External modules are unavailable.",
                "⚠️ Внешние модули недоступны.",
            ),
            (
                ExternalCommandText::MissingDescriptor,
                "⚠️ Module «fixture» is missing from the module catalog.",
                "⚠️ Модуль «fixture» отсутствует в каталоге модулей.",
            ),
            (
                ExternalCommandText::Unavailable,
                "⚠️ Module «fixture» is unavailable or stopped with an error.",
                "⚠️ Модуль «fixture» недоступен или завершился с ошибкой.",
            ),
            (
                ExternalCommandText::Timeout,
                "⚠️ Module «fixture» did not respond in time.",
                "⚠️ Модуль «fixture» не ответил вовремя.",
            ),
            (
                ExternalCommandText::ProtocolDecode,
                "⚠️ Module «fixture» sent an invalid response.",
                "⚠️ Модуль «fixture» прислал некорректный ответ.",
            ),
            (
                ExternalCommandText::WrongRequestId,
                "⚠️ Module «fixture» responded with the wrong request ID.",
                "⚠️ Модуль «fixture» прислал ответ с неверным идентификатором запроса.",
            ),
            (
                ExternalCommandText::ModuleError,
                "⚠️ Module «fixture» reported an execution error.",
                "⚠️ Модуль «fixture» сообщил об ошибке выполнения.",
            ),
            (
                ExternalCommandText::ResultTooLarge,
                "⚠️ Module «fixture» returned a result that is too large.",
                "⚠️ Результат модуля «fixture» слишком большой.",
            ),
            (
                ExternalCommandText::GenericError,
                "⚠️ Module error «fixture»: third-party output",
                "⚠️ Ошибка модуля «fixture»: third-party output",
            ),
        ] {
            let english =
                external_command_text(Locale::English, key, "fixture", Some("third-party output"));
            let russian =
                external_command_text(Locale::Russian, key, "fixture", Some("third-party output"));
            assert_eq!(english, expected_english);
            assert_eq!(russian, expected_russian);
        }
    }

    #[test]
    fn lm_interpolation_preserves_dynamic_placeholder_text() {
        for locale in [Locale::English, Locale::Russian] {
            let rendered = render_lm_install_plan(
                locale,
                LmInstallPlanText {
                    source: "source {prefix}",
                    module: "module {prefix}",
                    version: "{prefix}",
                    protocol: 6,
                    entrypoint: "entry {prefix}",
                    default_command: "{prefix}",
                    sha256: "hash {prefix}",
                    fingerprint: "fingerprint {prefix}",
                    archive_bytes: 1,
                    file_count: 1,
                    compressed_bytes: 1,
                    expanded_bytes: 1,
                    capabilities: "cap {prefix}",
                    subscriptions: "sub {prefix}",
                    methods: "method {prefix}",
                    actions: "action {prefix}",
                    warnings: "warning {prefix}",
                    approval_id: "id {prefix}",
                    prefix: "!",
                },
            );
            assert!(rendered.contains("source {prefix}"));
            assert!(rendered.contains("warning {prefix}"));
            assert!(
                rendered.contains("Confirm: !lm confirm id {prefix}")
                    || rendered.contains("Подтвердите: !lm confirm id {prefix}")
            );
        }
    }

    #[test]
    fn persistence_failures_have_catalog_entries_in_both_locales() {
        assert_eq!(
            text(Locale::English, Text::LanguageSaveFailed),
            "⚠️ Could not save language."
        );
        assert_eq!(
            text(Locale::Russian, Text::LanguageSaveFailed),
            "⚠️ Не удалось сохранить язык."
        );
        assert_eq!(
            text(Locale::English, Text::OnboardingSaveFailed),
            "⚠️ Could not save tutorial progress."
        );
        assert_eq!(
            text(Locale::Russian, Text::OnboardingSaveFailed),
            "⚠️ Не удалось сохранить прогресс обучения."
        );
    }

    #[test]
    fn reboot_ui_text_is_localized_for_pending_failure_and_completion() {
        for (locale, pending, failure, completion) in [
            (
                Locale::English,
                "⚠️ A previous restart confirmation is still pending.",
                "⚠️ Could not start the restart; Lavis is still running.",
                "✅ Lavis restarted\n\nRestart time: 35 s",
            ),
            (
                Locale::Russian,
                "⚠️ Уже ожидается подтверждение предыдущего перезапуска.",
                "⚠️ Не удалось начать перезапуск; Lavis продолжает работу.",
                "✅ Lavis перезагрузился\n\nВремя перезагрузки: 35 с",
            ),
        ] {
            assert_eq!(reboot_text(locale, RebootText::Pending, None), pending);
            assert_eq!(reboot_text(locale, RebootText::StartFailed, None), failure);
            assert_eq!(
                reboot_text(locale, RebootText::Completed, Some(35)),
                completion
            );
        }
    }

    #[test]
    fn lm_surfaces_have_english_and_russian_catalog_entries() {
        for key in [
            LmText::Usage,
            LmText::Empty,
            LmText::Unavailable,
            LmText::StateUnavailable,
            LmText::ListUnavailable,
            LmText::RuntimeUnavailable,
            LmText::NoRuntimeError,
            LmText::LastModuleError,
            LmText::DoctorHeading,
            LmText::DoctorModuleHeading,
            LmText::DoctorEmpty,
            LmText::NotFound,
            LmText::InvalidManifest,
            LmText::NotInstalled,
            LmText::Declarative,
            LmText::InstallUnavailable,
            LmText::AttachPackage,
            LmText::UnsafePackage,
            LmText::PlanUnavailable,
            LmText::ApprovalInvalid,
            LmText::AlreadyRegistered,
            LmText::Cancelled,
        ] {
            let english = lm_format(Locale::English, key, "fixture", ".");
            assert!(!contains_cyrillic(&english));
            assert!(!lm_text(Locale::Russian, key).is_empty());
        }
    }

    #[test]
    fn runtime_module_statuses_are_localized() {
        assert_eq!(
            lm_runtime_status(Locale::English, ExternalModuleRuntimeStatus::Running),
            "running"
        );
        assert_eq!(
            lm_runtime_status(
                Locale::Russian,
                ExternalModuleRuntimeStatus::InstalledDisabled
            ),
            "установлен, выключен"
        );
    }

    #[test]
    fn info_caption_is_localized_without_hardcoded_counts() {
        let english = render_info_text(
            Locale::English,
            InfoCaptionData {
                owner: "@owner",
                version: "0.1.0",
                commit: "b1d18f8",
                upstream: "unavailable",
                prefix: ",",
                active_modules: 3,
                total_modules: 5,
                host: "standalone",
                os: "NixOS 25.05",
            },
        );
        assert!(english.contains("Owner: @owner"));
        assert!(english.contains("Version: 0.1.0"));
        assert!(english.contains("Current commit: b1d18f8"));
        assert!(english.contains("Upstream main: unavailable"));
        assert!(english.contains("Prefix: ,"));
        assert!(english.contains("Modules: 5 (3 active)"));
        assert!(english.contains("Host: standalone"));
        assert!(english.contains("OS: NixOS 25.05"));
        assert!(!contains_cyrillic(&english));

        let russian = render_info_text(
            Locale::Russian,
            InfoCaptionData {
                owner: "@owner",
                version: "0.1.0",
                commit: "b1d18f8",
                upstream: "недоступно",
                prefix: ",",
                active_modules: 3,
                total_modules: 5,
                host: "standalone",
                os: "NixOS 25.05",
            },
        );
        assert!(russian.contains("Владелец: @owner"));
        assert!(russian.contains("Версия: 0.1.0"));
        assert!(russian.contains("Основная ветка: недоступно"));
        assert!(russian.contains("Модули: 5 (3 активных)"));
    }

    #[test]
    fn info_unavailable_string_is_localized() {
        assert_eq!(
            info_text(Locale::English, InfoText::Unavailable),
            "unavailable"
        );
        assert_eq!(
            info_text(Locale::Russian, InfoText::Unavailable),
            "недоступно"
        );
    }
}
