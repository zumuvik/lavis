use crate::i18n::{Locale, Text, text};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingProgress {
    #[default]
    NotStarted,
    Intro,
    Prefix,
    Ping,
    Help,
    Modules,
    ExternalModules,
    Companion,
    Completion,
    Complete,
    Skipped,
}

/// Result of choosing a tutorial page. The runtime persists `page` before
/// rendering and only advances it after Telegram accepts the edit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TutorialPage {
    pub page: OnboardingProgress,
    pub pending: OnboardingProgress,
}

impl OnboardingProgress {
    pub fn restart() -> Self {
        Self::Intro
    }
    pub fn skip() -> Self {
        Self::Skipped
    }
    pub fn next(self) -> Self {
        match self {
            Self::Intro => Self::Prefix,
            Self::Prefix => Self::Ping,
            Self::Ping => Self::Help,
            Self::Help => Self::Modules,
            Self::Modules => Self::ExternalModules,
            Self::ExternalModules => Self::Companion,
            Self::Companion => Self::Completion,
            Self::Completion => Self::Complete,
            state => state,
        }
    }
    pub fn page_for_start(self) -> TutorialPage {
        let page = match self {
            Self::NotStarted | Self::Complete | Self::Skipped => Self::Intro,
            page => page,
        };
        TutorialPage {
            page,
            pending: page,
        }
    }

    pub fn mark_delivered(self) -> Self {
        self.next()
    }
    pub fn message(self, locale: Locale, prefix: &str) -> String {
        let key = match self {
            Self::Intro => Text::OnboardingIntro,
            Self::Prefix => Text::OnboardingPrefix,
            Self::Ping => Text::OnboardingPing,
            Self::Help => Text::OnboardingHelp,
            Self::Modules => Text::OnboardingModules,
            Self::ExternalModules => Text::OnboardingExternal,
            Self::Companion => Text::OnboardingCompanion,
            Self::Completion => Text::OnboardingDone,
            Self::Complete => Text::OnboardingDone,
            Self::Skipped => Text::OnboardingSkipped,
            Self::NotStarted => Text::OnboardingSelect,
        };
        text(locale, key).replace("{prefix}", prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progresses_in_short_order_with_the_active_prefix() {
        assert_eq!(OnboardingProgress::Intro.next(), OnboardingProgress::Prefix);
        assert!(
            OnboardingProgress::Prefix
                .message(Locale::English, "!")
                .contains("!prefix")
        );
        assert_eq!(
            OnboardingProgress::Companion.next(),
            OnboardingProgress::Completion
        );
    }

    #[test]
    fn completion_is_rendered_before_it_is_persisted_as_complete() {
        let page = OnboardingProgress::Companion.page_for_start();
        assert_eq!(page.page, OnboardingProgress::Companion);
        assert_eq!(page.pending, OnboardingProgress::Companion);
        assert_eq!(page.page.mark_delivered(), OnboardingProgress::Completion);
        assert_eq!(
            OnboardingProgress::Completion.page_for_start().page,
            OnboardingProgress::Completion
        );
        assert_eq!(
            OnboardingProgress::Complete.page_for_start().page,
            OnboardingProgress::Intro
        );
    }
}
