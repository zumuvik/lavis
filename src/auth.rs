#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationOutcome {
    JustCompleted { self_user_id: PeerId },
    ExistingSession { self_user_id: PeerId },
}

impl AuthorizationOutcome {
    pub fn is_just_completed(self) -> bool {
        matches!(self, Self::JustCompleted { .. })
    }

    pub fn self_user_id(self) -> PeerId {
        match self {
            Self::JustCompleted { self_user_id } | Self::ExistingSession { self_user_id } => {
                self_user_id
            }
        }
    }
}

pub const CREDENTIAL_NOTIFICATION: &str = "Lavis использует авторизацию Telegram по MTProto.\n\n\
    Код входа временно обрабатывается в памяти процесса и\n\
    передаётся Telegram через библиотеку grammers.\n\
    Lavis намеренно не записывает код в постоянное хранилище\n\
    и не добавляет его в логи или диагностику.\n\n\
    Локальная сессия Telegram сохраняется, чтобы не выполнять\n\
    авторизацию при каждом запуске.";
pub const PASSWORD_NOTIFICATION: &str = "Пароль 2FA временно обрабатывается в памяти процесса и\n\
    передаётся Telegram через библиотеку grammers.\n\
    Lavis намеренно не записывает пароль в постоянное хранилище\n\
    и не добавляет его в логи или диагностику.";

use std::io::{self, IsTerminal, Write};

use grammers_client::{Client, SignInError};
use grammers_session::types::PeerId;

use crate::{config::Config, error::AuthError};

pub async fn authorize(
    client: &Client,
    config: &Config,
) -> Result<AuthorizationOutcome, AuthError> {
    let just_completed = if !client
        .is_authorized()
        .await
        .map_err(|_| AuthError::AuthorizationCheck)?
    {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(AuthError::NonInteractive);
        }
        println!("{CREDENTIAL_NOTIFICATION}");
        let phone = read_line("Telegram phone number: ").await?;
        let token = client
            .request_login_code(&phone, config.api_hash())
            .await
            .map_err(|_| AuthError::RequestLoginCode)?;
        let code = read_secret("Telegram login code: ").await?;

        match client.sign_in(&token, &code).await {
            Ok(_) => true,
            Err(SignInError::PasswordRequired(token)) => {
                println!("{PASSWORD_NOTIFICATION}");
                let password = read_password("Telegram two-factor password: ").await?;
                match client.check_password(token, password.as_bytes()).await {
                    Ok(_) => true,
                    Err(SignInError::SignUpRequired) => return Err(AuthError::SignUpRequired),
                    Err(SignInError::PasswordRequired(_)) => return Err(AuthError::SignIn),
                    Err(SignInError::InvalidCode) => return Err(AuthError::SignIn),
                    Err(SignInError::InvalidPassword(_)) => return Err(AuthError::InvalidPassword),
                    Err(SignInError::Other(_)) => return Err(AuthError::SignIn),
                }
            }
            Err(SignInError::SignUpRequired) => return Err(AuthError::SignUpRequired),
            Err(SignInError::InvalidCode) => return Err(AuthError::InvalidCode),
            Err(SignInError::InvalidPassword(_)) => return Err(AuthError::InvalidPassword),
            Err(SignInError::Other(_)) => return Err(AuthError::SignIn),
        }
    } else {
        false
    };

    let user = client
        .get_me()
        .await
        .map_err(|_| AuthError::GetAuthorizedUser)?;
    log_authorized_user(&user);
    let outcome = if just_completed {
        AuthorizationOutcome::JustCompleted {
            self_user_id: user.id(),
        }
    } else {
        AuthorizationOutcome::ExistingSession {
            self_user_id: user.id(),
        }
    };
    Ok(outcome)
}

async fn read_line(prompt: &'static str) -> Result<String, AuthError> {
    let value = tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut value = String::new();
        io::stdin().read_line(&mut value)?;
        Ok::<_, io::Error>(value)
    })
    .await
    .map_err(|_| AuthError::ReadInput)?
    .map_err(|_| AuthError::ReadInput)?;

    normalize_input(value).ok_or(AuthError::EmptyInput)
}

async fn read_secret(prompt: &'static str) -> Result<String, AuthError> {
    let value = tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt))
        .await
        .map_err(|_| AuthError::ReadInput)?
        .map_err(|_| AuthError::ReadInput)?;

    normalize_input(value).ok_or(AuthError::EmptyInput)
}

async fn read_password(prompt: &'static str) -> Result<String, AuthError> {
    let value = tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt))
        .await
        .map_err(|_| AuthError::ReadInput)?
        .map_err(|_| AuthError::ReadInput)?;

    preserve_password_input(value).ok_or(AuthError::EmptyInput)
}

fn normalize_input(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn preserve_password_input(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn log_authorized_user(user: &grammers_client::peer::User) {
    let id = user.id();
    if let Some(username) = user.username().filter(|username| !username.is_empty()) {
        tracing::info!(event = "telegram_authorized", %id, username, "Telegram authorization ready");
    } else if let Some(display_name) = display_name(user) {
        tracing::info!(event = "telegram_authorized", %id, display_name, "Telegram authorization ready");
    } else {
        tracing::info!(event = "telegram_authorized", %id, "Telegram authorization ready");
    }
}

fn display_name(user: &grammers_client::peer::User) -> Option<String> {
    let first_name = user.first_name().filter(|name| !name.is_empty());
    let last_name = user.last_name().filter(|name| !name.is_empty());
    match (first_name, last_name) {
        (Some(first), Some(last)) => Some(format!("{first} {last}")),
        (Some(first), None) => Some(first.to_owned()),
        (None, Some(last)) => Some(last.to_owned()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CREDENTIAL_NOTIFICATION, PASSWORD_NOTIFICATION, normalize_input, preserve_password_input,
    };

    #[test]
    fn normalizes_non_empty_input() {
        assert_eq!(
            normalize_input("  value\n".to_owned()),
            Some("value".to_owned())
        );
        assert_eq!(normalize_input(" \t ".to_owned()), None);
    }

    #[test]
    fn preserves_password_whitespace_but_rejects_empty_input() {
        assert_eq!(
            preserve_password_input(" leading and trailing ".to_owned()),
            Some(" leading and trailing ".to_owned())
        );
        assert_eq!(preserve_password_input(String::new()), None);
    }

    #[test]
    fn credential_notification_is_technically_precise() {
        assert!(CREDENTIAL_NOTIFICATION.contains("MTProto"));
        assert!(CREDENTIAL_NOTIFICATION.contains("grammers"));
        assert!(CREDENTIAL_NOTIFICATION.contains("временно обрабатывается"));
        assert!(CREDENTIAL_NOTIFICATION.contains("постоянное хранилище"));
        assert!(CREDENTIAL_NOTIFICATION.contains("логи"));
        assert!(CREDENTIAL_NOTIFICATION.contains("Локальная сессия"));
        assert!(!CREDENTIAL_NOTIFICATION.contains("только в Telegram"));
        assert!(!CREDENTIAL_NOTIFICATION.contains("напрямую"));
        assert!(!CREDENTIAL_NOTIFICATION.contains("никогда"));
        assert!(!CREDENTIAL_NOTIFICATION.contains("api_id"));
        assert!(!CREDENTIAL_NOTIFICATION.contains("api_hash"));
        assert!(!CREDENTIAL_NOTIFICATION.contains("/home/"));
    }

    #[test]
    fn password_notification_is_technically_precise() {
        assert!(PASSWORD_NOTIFICATION.contains("grammers"));
        assert!(PASSWORD_NOTIFICATION.contains("временно обрабатывается"));
        assert!(PASSWORD_NOTIFICATION.contains("постоянное хранилище"));
        assert!(PASSWORD_NOTIFICATION.contains("логи"));
        assert!(!PASSWORD_NOTIFICATION.contains("только в Telegram"));
        assert!(!PASSWORD_NOTIFICATION.contains("напрямую"));
        assert!(!PASSWORD_NOTIFICATION.contains("не накапливает"));
        assert!(!PASSWORD_NOTIFICATION.contains("никогда"));
        assert!(!PASSWORD_NOTIFICATION.contains("api_id"));
        assert!(!PASSWORD_NOTIFICATION.contains("api_hash"));
        assert!(!PASSWORD_NOTIFICATION.contains("/home/"));
    }

    #[test]
    fn just_completed_causes_is_just_completed_true() {
        use super::AuthorizationOutcome;
        use grammers_session::types::PeerId;
        let outcome = AuthorizationOutcome::JustCompleted {
            self_user_id: PeerId::self_user(),
        };
        assert!(outcome.is_just_completed());
    }

    #[test]
    fn existing_session_causes_is_just_completed_false() {
        use super::AuthorizationOutcome;
        use grammers_session::types::PeerId;
        let outcome = AuthorizationOutcome::ExistingSession {
            self_user_id: PeerId::self_user(),
        };
        assert!(!outcome.is_just_completed());
    }

    #[test]
    fn both_outcomes_provide_self_user_id() {
        use super::AuthorizationOutcome;
        use grammers_session::types::PeerId;
        let just = AuthorizationOutcome::JustCompleted {
            self_user_id: PeerId::self_user(),
        };
        let existing = AuthorizationOutcome::ExistingSession {
            self_user_id: PeerId::self_user(),
        };
        assert_eq!(just.self_user_id(), PeerId::self_user());
        assert_eq!(existing.self_user_id(), PeerId::self_user());
    }
}
