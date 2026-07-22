use std::io::{self, IsTerminal, Write};

use grammers_client::{Client, SignInError};
use grammers_session::types::PeerId;

use crate::{config::Config, error::AuthError};

pub async fn authorize(client: &Client, config: &Config) -> Result<PeerId, AuthError> {
    if !client
        .is_authorized()
        .await
        .map_err(|_| AuthError::AuthorizationCheck)?
    {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(AuthError::NonInteractive);
        }
        let phone = read_line("Telegram phone number: ").await?;
        let token = client
            .request_login_code(&phone, config.api_hash())
            .await
            .map_err(|_| AuthError::RequestLoginCode)?;
        let code = read_secret("Telegram login code: ").await?;

        match client.sign_in(&token, &code).await {
            Ok(_) => {}
            Err(SignInError::PasswordRequired(token)) => {
                let password = read_password("Telegram two-factor password: ").await?;
                match client.check_password(token, password.as_bytes()).await {
                    Ok(_) => {}
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
    }

    let user = client
        .get_me()
        .await
        .map_err(|_| AuthError::GetAuthorizedUser)?;
    log_authorized_user(&user);
    Ok(user.id())
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
    use super::{normalize_input, preserve_password_input};

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
}
