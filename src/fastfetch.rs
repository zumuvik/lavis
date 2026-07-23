use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
    time::Duration,
};

use serde::Deserialize;

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    task::JoinHandle,
    time::timeout,
};

use crate::response::{Response, TRUNCATION_SUFFIX};

const STDOUT_CAP: usize = 64 * 1024;
const STDERR_CAP: usize = 16 * 1024;
const STDERR_EXCERPT_UNITS: usize = 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
const DRAIN_GRACE: Duration = Duration::from_secs(1);
const PROFILE_MAX_BYTES: usize = 16 * 1024;
const MAX_STRUCTURE_COMPONENTS: usize = 26;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastfetchInputError {
    Tokenization,
    UnsupportedOption,
    MissingValue,
    DuplicateOption,
    InvalidLogo,
    InvalidStructure,
    InvalidSeparator,
    InvalidLogoPadding,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FastfetchResult {
    Success(Response),
    Empty,
    TimedOut,
    Unavailable,
    NonZero { code: i32, stderr: String },
    UnexpectedStatus,
    InvalidArguments(FastfetchInputError),
    ProfileError(FastfetchProfileError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastfetchProfileError {
    NotReadable,
    Malformed,
    UnsupportedVersion,
    TooLarge,
    UnsafePath,
    InvalidLogo,
    InvalidStructure,
    InvalidSeparator,
    InvalidLogoPadding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LogoPadding {
    left: Option<u8>,
    right: Option<u8>,
    top: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Logo {
    None,
    Builtin(BuiltinLogo),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinLogo {
    Alpine,
    Arch,
    Debian,
    Fedora,
    FreeBSD,
    Linux,
    MacOS,
    NixOS,
    OpenBSD,
    Ubuntu,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Module {
    Title,
    Separator,
    Os,
    Kernel,
    Uptime,
    Cpu,
    Memory,
    Gpu,
    Packages,
    Shell,
    Terminal,
    TerminalSize,
    Host,
    Display,
    Wm,
    De,
    Theme,
    Icons,
    Font,
    Cursor,
    Disk,
    Swap,
    LocalIp,
    Battery,
    PowerAdapter,
    Locale,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PartialOptions {
    no_profile: bool,
    logo: Option<Logo>,
    structure: Option<Vec<Module>>,
    separator: Option<String>,
    logo_padding: LogoPadding,
}

#[derive(Debug)]
struct EffectiveOptions {
    logo: Option<Logo>,
    structure: Option<Vec<Module>>,
    separator: Option<String>,
    logo_padding: LogoPadding,
}

#[derive(Debug)]
struct Invocation {
    arguments: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    version: u32,
    logo: Option<String>,
    structure: Option<Vec<String>>,
    separator: Option<String>,
    #[serde(default)]
    logo_padding_left: Option<u8>,
    #[serde(default)]
    logo_padding_right: Option<u8>,
    #[serde(default)]
    logo_padding_top: Option<u8>,
}

struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

pub async fn run(arguments: &str, profile_path: &Path) -> FastfetchResult {
    match prepare(arguments, profile_path).await {
        Ok(invocation) => execute(invocation).await,
        Err(result) => result,
    }
}

async fn prepare(arguments: &str, profile_path: &Path) -> Result<Invocation, FastfetchResult> {
    let tokens = match tokenize(arguments) {
        Ok(tokens) => tokens,
        Err(error) => return Err(FastfetchResult::InvalidArguments(error)),
    };
    let command = match parse_options(&tokens) {
        Ok(options) => options,
        Err(error) => return Err(FastfetchResult::InvalidArguments(error)),
    };
    let profile = if command.no_profile {
        PartialOptions::default()
    } else {
        match load_profile(profile_path).await {
            Ok(profile) => profile,
            Err(error) => return Err(FastfetchResult::ProfileError(error)),
        }
    };
    Ok(compile(profile, command))
}

pub fn tokenize(arguments: &str) -> Result<Vec<String>, FastfetchInputError> {
    shell_words::split(arguments).map_err(|_| FastfetchInputError::Tokenization)
}

fn parse_options(tokens: &[String]) -> Result<PartialOptions, FastfetchInputError> {
    let mut options = PartialOptions::default();
    let mut index = 0;
    while let Some(option) = tokens.get(index) {
        match option.as_str() {
            "--no-profile" => {
                if options.no_profile {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                options.no_profile = true;
                index += 1;
            }
            "--logo" => {
                if options.logo.is_some() {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                let value = tokens
                    .get(index + 1)
                    .ok_or(FastfetchInputError::MissingValue)?;
                options.logo = Some(parse_logo(value)?);
                index += 2;
            }
            "--structure" => {
                if options.structure.is_some() {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                let value = tokens
                    .get(index + 1)
                    .ok_or(FastfetchInputError::MissingValue)?;
                options.structure = Some(parse_structure(value)?);
                index += 2;
            }
            "--separator" => {
                if options.separator.is_some() {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                let value = tokens
                    .get(index + 1)
                    .ok_or(FastfetchInputError::MissingValue)?;
                validate_separator(value)?;
                options.separator = Some(value.clone());
                index += 2;
            }
            "--logo-padding-left" => {
                if options.logo_padding.left.is_some() {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                let value = tokens
                    .get(index + 1)
                    .ok_or(FastfetchInputError::MissingValue)?;
                options.logo_padding.left = Some(parse_logo_padding(value)?);
                index += 2;
            }
            "--logo-padding-right" => {
                if options.logo_padding.right.is_some() {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                let value = tokens
                    .get(index + 1)
                    .ok_or(FastfetchInputError::MissingValue)?;
                options.logo_padding.right = Some(parse_logo_padding(value)?);
                index += 2;
            }
            "--logo-padding-top" => {
                if options.logo_padding.top.is_some() {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                let value = tokens
                    .get(index + 1)
                    .ok_or(FastfetchInputError::MissingValue)?;
                options.logo_padding.top = Some(parse_logo_padding(value)?);
                index += 2;
            }
            _ => return Err(FastfetchInputError::UnsupportedOption),
        }
    }
    Ok(options)
}

fn parse_logo(value: &str) -> Result<Logo, FastfetchInputError> {
    let logo = if value.eq_ignore_ascii_case("none") {
        Logo::None
    } else if value.eq_ignore_ascii_case("Alpine") {
        Logo::Builtin(BuiltinLogo::Alpine)
    } else if value.eq_ignore_ascii_case("Arch") {
        Logo::Builtin(BuiltinLogo::Arch)
    } else if value.eq_ignore_ascii_case("Debian") {
        Logo::Builtin(BuiltinLogo::Debian)
    } else if value.eq_ignore_ascii_case("Fedora") {
        Logo::Builtin(BuiltinLogo::Fedora)
    } else if value.eq_ignore_ascii_case("FreeBSD") {
        Logo::Builtin(BuiltinLogo::FreeBSD)
    } else if value.eq_ignore_ascii_case("Linux") {
        Logo::Builtin(BuiltinLogo::Linux)
    } else if value.eq_ignore_ascii_case("MacOS") {
        Logo::Builtin(BuiltinLogo::MacOS)
    } else if value.eq_ignore_ascii_case("NixOS") {
        Logo::Builtin(BuiltinLogo::NixOS)
    } else if value.eq_ignore_ascii_case("OpenBSD") {
        Logo::Builtin(BuiltinLogo::OpenBSD)
    } else if value.eq_ignore_ascii_case("Ubuntu") {
        Logo::Builtin(BuiltinLogo::Ubuntu)
    } else if value.eq_ignore_ascii_case("Windows") {
        Logo::Builtin(BuiltinLogo::Windows)
    } else {
        return Err(FastfetchInputError::InvalidLogo);
    };
    Ok(logo)
}

fn parse_structure(value: &str) -> Result<Vec<Module>, FastfetchInputError> {
    let mut components = Vec::new();
    for component in value.split(':') {
        let module = match component.to_ascii_lowercase().as_str() {
            "title" => Module::Title,
            "separator" => Module::Separator,
            "os" => Module::Os,
            "kernel" => Module::Kernel,
            "uptime" => Module::Uptime,
            "cpu" => Module::Cpu,
            "memory" => Module::Memory,
            "gpu" => Module::Gpu,
            "packages" => Module::Packages,
            "shell" => Module::Shell,
            "terminal" => Module::Terminal,
            "terminalsize" => Module::TerminalSize,
            "host" => Module::Host,
            "display" => Module::Display,
            "wm" => Module::Wm,
            "de" => Module::De,
            "theme" => Module::Theme,
            "icons" => Module::Icons,
            "font" => Module::Font,
            "cursor" => Module::Cursor,
            "disk" => Module::Disk,
            "swap" => Module::Swap,
            "localip" => Module::LocalIp,
            "battery" => Module::Battery,
            "poweradapter" => Module::PowerAdapter,
            "locale" => Module::Locale,
            _ => return Err(FastfetchInputError::InvalidStructure),
        };
        if components.contains(&module) {
            return Err(FastfetchInputError::InvalidStructure);
        }
        components.push(module);
    }
    if components.is_empty() || components.len() > MAX_STRUCTURE_COMPONENTS {
        return Err(FastfetchInputError::InvalidStructure);
    }
    Ok(components)
}

fn validate_separator(value: &str) -> Result<(), FastfetchInputError> {
    if !(1..=64).contains(&value.chars().count())
        || value.starts_with("--")
        || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        return Err(FastfetchInputError::InvalidSeparator);
    }
    Ok(())
}

fn parse_logo_padding(value: &str) -> Result<u8, FastfetchInputError> {
    let n = value
        .parse::<u8>()
        .map_err(|_| FastfetchInputError::InvalidLogoPadding)?;
    if n > 32 {
        return Err(FastfetchInputError::InvalidLogoPadding);
    }
    Ok(n)
}

async fn load_profile(path: &Path) -> Result<PartialOptions, FastfetchProfileError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_profile(&path))
        .await
        .map_err(|_| FastfetchProfileError::NotReadable)?
}

fn read_profile(path: &Path) -> Result<PartialOptions, FastfetchProfileError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PartialOptions::default());
        }
        Err(_) => return Err(FastfetchProfileError::NotReadable),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(FastfetchProfileError::UnsafePath);
    }
    if metadata.len() > PROFILE_MAX_BYTES as u64 {
        return Err(FastfetchProfileError::TooLarge);
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| FastfetchProfileError::NotReadable)?
        .take((PROFILE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| FastfetchProfileError::NotReadable)?;
    if bytes.len() > PROFILE_MAX_BYTES {
        return Err(FastfetchProfileError::TooLarge);
    }
    let profile: Profile =
        serde_json::from_slice(&bytes).map_err(|_| FastfetchProfileError::Malformed)?;
    if profile.version != 1 {
        return Err(FastfetchProfileError::UnsupportedVersion);
    }
    let logo = profile
        .logo
        .as_deref()
        .map(parse_logo)
        .transpose()
        .map_err(|_| FastfetchProfileError::InvalidLogo)?;
    let structure = match profile.structure {
        Some(structure) => {
            if structure.iter().any(|component| component.contains(':')) {
                return Err(FastfetchProfileError::InvalidStructure);
            }
            Some(
                parse_structure(&structure.join(":"))
                    .map_err(|_| FastfetchProfileError::InvalidStructure)?,
            )
        }
        None => None,
    };
    if let Some(separator) = &profile.separator {
        validate_separator(separator).map_err(|_| FastfetchProfileError::InvalidSeparator)?;
    }
    let validate_padding = |v: Option<u8>| -> Result<(), FastfetchProfileError> {
        match v {
            Some(n) if n > 32 => Err(FastfetchProfileError::InvalidLogoPadding),
            _ => Ok(()),
        }
    };
    validate_padding(profile.logo_padding_left)?;
    validate_padding(profile.logo_padding_right)?;
    validate_padding(profile.logo_padding_top)?;
    Ok(PartialOptions {
        no_profile: false,
        logo,
        structure,
        separator: profile.separator,
        logo_padding: LogoPadding {
            left: profile.logo_padding_left,
            right: profile.logo_padding_right,
            top: profile.logo_padding_top,
        },
    })
}

fn compile(profile: PartialOptions, command: PartialOptions) -> Invocation {
    let effective = EffectiveOptions {
        logo: command.logo.or(profile.logo),
        structure: command.structure.or(profile.structure),
        separator: command.separator.or(profile.separator),
        logo_padding: LogoPadding {
            left: command.logo_padding.left.or(profile.logo_padding.left),
            right: command.logo_padding.right.or(profile.logo_padding.right),
            top: command.logo_padding.top.or(profile.logo_padding.top),
        },
    };
    let mut arguments = vec![
        "--config".to_owned(),
        "none".to_owned(),
        "--pipe".to_owned(),
    ];
    if let Some(logo) = effective.logo {
        match logo {
            Logo::None => arguments.extend(["--logo".to_owned(), "none".to_owned()]),
            Logo::Builtin(logo) => arguments.extend([
                "--logo-type".to_owned(),
                "builtin".to_owned(),
                "--logo".to_owned(),
                logo.as_str().to_owned(),
            ]),
        }
    }
    if let Some(structure) = effective.structure {
        arguments.extend([
            "--structure".to_owned(),
            structure
                .iter()
                .map(|module| module.as_str())
                .collect::<Vec<_>>()
                .join(":"),
        ]);
    }
    if let Some(separator) = effective.separator {
        arguments.extend(["--separator".to_owned(), separator]);
    }
    if let Some(left) = effective.logo_padding.left {
        arguments.extend(["--logo-padding-left".to_owned(), left.to_string()]);
    }
    if let Some(right) = effective.logo_padding.right {
        arguments.extend(["--logo-padding-right".to_owned(), right.to_string()]);
    }
    if let Some(top) = effective.logo_padding.top {
        arguments.extend(["--logo-padding-top".to_owned(), top.to_string()]);
    }
    Invocation { arguments }
}

impl BuiltinLogo {
    fn as_str(self) -> &'static str {
        match self {
            Self::Alpine => "Alpine",
            Self::Arch => "Arch",
            Self::Debian => "Debian",
            Self::Fedora => "Fedora",
            Self::FreeBSD => "FreeBSD",
            Self::Linux => "Linux",
            Self::MacOS => "MacOS",
            Self::NixOS => "NixOS",
            Self::OpenBSD => "OpenBSD",
            Self::Ubuntu => "Ubuntu",
            Self::Windows => "Windows",
        }
    }
}

impl Module {
    fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Separator => "separator",
            Self::Os => "os",
            Self::Kernel => "kernel",
            Self::Uptime => "uptime",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Gpu => "gpu",
            Self::Packages => "packages",
            Self::Shell => "shell",
            Self::Terminal => "terminal",
            Self::TerminalSize => "terminalsize",
            Self::Host => "host",
            Self::Display => "display",
            Self::Wm => "wm",
            Self::De => "de",
            Self::Theme => "theme",
            Self::Icons => "icons",
            Self::Font => "font",
            Self::Cursor => "cursor",
            Self::Disk => "disk",
            Self::Swap => "swap",
            Self::LocalIp => "localip",
            Self::Battery => "battery",
            Self::PowerAdapter => "poweradapter",
            Self::Locale => "locale",
        }
    }
}

async fn execute(invocation: Invocation) -> FastfetchResult {
    let mut command = Command::new("fastfetch");
    command
        .args(invocation.arguments)
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .env_remove("LAVIS_API_ID")
        .env_remove("LAVIS_API_HASH")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("CLICOLOR_FORCE", "0")
        .env("TERM", "dumb");

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return FastfetchResult::Unavailable,
    };
    let Some(stdout) = child.stdout.take() else {
        if !terminate_and_wait(&mut child).await {
            tracing::debug!(
                event = "fastfetch_cleanup_failed",
                "Fastfetch cleanup failed"
            );
        }
        return FastfetchResult::UnexpectedStatus;
    };
    let Some(stderr) = child.stderr.take() else {
        if !terminate_and_wait(&mut child).await {
            tracing::debug!(
                event = "fastfetch_cleanup_failed",
                "Fastfetch cleanup failed"
            );
        }
        return FastfetchResult::UnexpectedStatus;
    };
    let mut stdout_task = tokio::spawn(drain(stdout, STDOUT_CAP));
    let mut stderr_task = tokio::spawn(drain(stderr, STDERR_CAP));

    let status = match timeout(TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            let terminated = terminate_and_wait(&mut child).await;
            let drains = await_drains(&mut stdout_task, &mut stderr_task).await;
            if !terminated || drains.is_none() {
                tracing::debug!(
                    event = "fastfetch_cleanup_failed",
                    "Fastfetch cleanup failed"
                );
            }
            return FastfetchResult::UnexpectedStatus;
        }
        Err(_) => {
            let terminated = terminate_and_wait(&mut child).await;
            let drains = await_drains_with_grace(&mut stdout_task, &mut stderr_task).await;
            if !terminated || drains.is_none() {
                tracing::debug!(
                    event = "fastfetch_cleanup_failed",
                    "Fastfetch cleanup failed"
                );
            }
            return FastfetchResult::TimedOut;
        }
    };
    let (stdout, stderr) = match await_drains(&mut stdout_task, &mut stderr_task).await {
        Some(captures) => captures,
        None => return FastfetchResult::UnexpectedStatus,
    };

    if status.success() {
        let output = sanitize_capture(&stdout);
        if output.is_empty() {
            FastfetchResult::Empty
        } else {
            FastfetchResult::Success(Response::preformatted(output))
        }
    } else if let Some(code) = status.code() {
        FastfetchResult::NonZero {
            code,
            stderr: truncate_excerpt(&sanitize_capture(&stderr)),
        }
    } else {
        FastfetchResult::UnexpectedStatus
    }
}

async fn terminate_and_wait(child: &mut Child) -> bool {
    let kill_started = child.start_kill().is_ok();
    let waited = child.wait().await.is_ok();
    kill_started && waited
}

async fn await_drains(
    stdout_task: &mut JoinHandle<io::Result<Capture>>,
    stderr_task: &mut JoinHandle<io::Result<Capture>>,
) -> Option<(Capture, Capture)> {
    let stdout = stdout_task.await;
    let stderr = stderr_task.await;
    match (stdout, stderr) {
        (Ok(Ok(stdout)), Ok(Ok(stderr))) => Some((stdout, stderr)),
        _ => None,
    }
}

async fn await_drains_with_grace(
    stdout_task: &mut JoinHandle<io::Result<Capture>>,
    stderr_task: &mut JoinHandle<io::Result<Capture>>,
) -> Option<(Capture, Capture)> {
    match timeout(DRAIN_GRACE, await_drains(stdout_task, stderr_task)).await {
        Ok(captures) => captures,
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            let stdout = stdout_task.await;
            let stderr = stderr_task.await;
            if stdout.is_err() || stderr.is_err() {
                tracing::debug!(
                    event = "fastfetch_drain_abort_failed",
                    "Fastfetch drain abort failed"
                );
            }
            None
        }
    }
}

async fn drain<R>(mut reader: R, cap: usize) -> io::Result<Capture>
where
    R: AsyncRead + Unpin,
{
    let mut capture = Capture {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(capture);
        }
        append_capture(&mut capture, &buffer[..count], cap);
    }
}

fn append_capture(capture: &mut Capture, chunk: &[u8], cap: usize) {
    let available = cap.saturating_sub(capture.bytes.len());
    let captured = available.min(chunk.len());
    capture.bytes.extend_from_slice(&chunk[..captured]);
    capture.truncated |= captured < chunk.len();
}

fn sanitize_capture(capture: &Capture) -> String {
    let stripped = strip_ansi_escapes::strip(normalize_input_bytes(&capture.bytes));
    let normalized = String::from_utf8_lossy(&stripped);
    let mut output = String::new();
    for character in normalized.chars() {
        match character {
            '\n' => output.push('\n'),
            '\t' => output.push_str("    "),
            character
                if character == '\0' || character.is_control() || is_bidi_control(character) => {}
            character => output.push(character),
        }
    }
    let output = output
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches('\n')
        .to_owned();
    if capture.truncated {
        if output.is_empty() {
            TRUNCATION_SUFFIX.to_owned()
        } else {
            format!("{output}\n{TRUNCATION_SUFFIX}")
        }
    } else {
        output
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn normalize_input_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                normalized.push(b'\n');
                index += usize::from(bytes.get(index + 1) == Some(&b'\n'));
            }
            b'\t' => normalized.extend_from_slice(b"    "),
            byte => normalized.push(byte),
        }
        index += 1;
    }
    normalized
}

fn truncate_excerpt(text: &str) -> String {
    if text.encode_utf16().count() <= STDERR_EXCERPT_UNITS {
        return text.to_owned();
    }
    let suffix_units = TRUNCATION_SUFFIX.encode_utf16().count();
    let limit = STDERR_EXCERPT_UNITS.saturating_sub(suffix_units);
    let mut end = 0;
    let mut units = 0usize;
    for (index, character) in text.char_indices() {
        if units.saturating_add(character.len_utf16()) > limit {
            break;
        }
        units += character.len_utf16();
        end = index + character.len_utf8();
    }
    format!("{}{}", &text[..end], TRUNCATION_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::{
        Capture, FastfetchInputError, FastfetchProfileError, FastfetchResult, LogoPadding,
        PROFILE_MAX_BYTES, PartialOptions, append_capture, compile, parse_logo_padding,
        parse_options, prepare, read_profile, sanitize_capture, tokenize,
    };
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn tokenizes_quotes_escapes_and_shell_syntax_literally() {
        assert_eq!(
            tokenize("--logo none --structure os:kernel:cpu:gpu:memory --separator \" -> \"")
                .unwrap(),
            [
                "--logo",
                "none",
                "--structure",
                "os:kernel:cpu:gpu:memory",
                "--separator",
                " -> "
            ]
        );
        assert_eq!(
            tokenize("'a b' escaped\\ space ; $() `x`").unwrap(),
            ["a b", "escaped space", ";", "$()", "`x`"]
        );
        assert_eq!(
            tokenize("'unterminated"),
            Err(FastfetchInputError::Tokenization)
        );
    }

    #[test]
    fn parses_and_compiles_only_safe_fastfetch_options() {
        let invocation = compile(
            parse_options(&[]).unwrap(),
            parse_options(&[
                "--logo".to_owned(),
                "nIxOs".to_owned(),
                "--structure".to_owned(),
                "Title:SEPARATOR:os:TerminalSize".to_owned(),
                "--separator".to_owned(),
                " -> ".to_owned(),
            ])
            .unwrap(),
        );
        assert_eq!(
            invocation.arguments,
            [
                "--config",
                "none",
                "--pipe",
                "--logo-type",
                "builtin",
                "--logo",
                "NixOS",
                "--structure",
                "title:separator:os:terminalsize",
                "--separator",
                " -> ",
            ]
        );
        assert_eq!(
            compile(parse_options(&[]).unwrap(), parse_options(&[]).unwrap()).arguments,
            ["--config", "none", "--pipe"]
        );
    }

    #[test]
    fn canonicalizes_ascii_case_insensitive_logos() {
        for (input, expected) in [
            ("arch", "Arch"),
            ("Windows", "Windows"),
            ("nIxOs", "NixOS"),
            ("UBUNTU", "Ubuntu"),
        ] {
            let invocation = compile(
                parse_options(&[]).unwrap(),
                parse_options(&["--logo".to_owned(), input.to_owned()]).unwrap(),
            );
            assert_eq!(
                invocation.arguments,
                [
                    "--config",
                    "none",
                    "--pipe",
                    "--logo-type",
                    "builtin",
                    "--logo",
                    expected,
                ]
            );
        }
        assert_eq!(
            compile(
                parse_options(&[]).unwrap(),
                parse_options(&["--logo".to_owned(), "NoNe".to_owned()]).unwrap(),
            )
            .arguments,
            ["--config", "none", "--pipe", "--logo", "none"]
        );
    }

    #[test]
    fn rejects_unsafe_and_ambiguous_option_forms() {
        assert_eq!(
            parse_options(&["--logo".to_owned(), "small".to_owned()]),
            Err(FastfetchInputError::InvalidLogo)
        );
        assert_eq!(
            parse_options(&["--config".to_owned(), "file".to_owned()]),
            Err(FastfetchInputError::UnsupportedOption)
        );
        assert_eq!(
            parse_options(&["--separator".to_owned(), "bad\n".to_owned()]),
            Err(FastfetchInputError::InvalidSeparator)
        );
        assert_eq!(
            parse_options(&["--separator".to_owned(), "--unsafe".to_owned()]),
            Err(FastfetchInputError::InvalidSeparator)
        );
        assert_eq!(
            parse_options(&["--separator".to_owned(), "→".to_owned()]),
            Err(FastfetchInputError::InvalidSeparator)
        );
        for tokens in [
            vec!["--logo=NixOS".to_owned()],
            vec!["--".to_owned()],
            vec!["positional".to_owned()],
            vec!["--logo".to_owned()],
            vec!["--no-profile".to_owned(), "--no-profile".to_owned()],
            vec!["--structure".to_owned(), "сpu".to_owned()],
        ] {
            assert!(parse_options(&tokens).is_err());
        }
    }

    #[test]
    fn rejects_repeated_structure_modules() {
        assert_eq!(
            parse_options(&["--structure".to_owned(), "os:kernel:os".to_owned()]),
            Err(FastfetchInputError::InvalidStructure)
        );
    }

    #[test]
    fn preserves_mixed_case_structure_order_and_allows_exactly_26_modules() {
        let structure = "Locale:PowerAdapter:Battery:LocalIP:Swap:Disk:Cursor:Font:Icons:Theme:DE:WM:Display:Host:TerminalSize:Terminal:Shell:Packages:GPU:Memory:CPU:Uptime:Kernel:OS:Separator:Title";
        let invocation = compile(
            parse_options(&[]).unwrap(),
            parse_options(&["--structure".to_owned(), structure.to_owned()]).unwrap(),
        );
        assert_eq!(
            invocation.arguments.last(),
            Some(&"locale:poweradapter:battery:localip:swap:disk:cursor:font:icons:theme:de:wm:display:host:terminalsize:terminal:shell:packages:gpu:memory:cpu:uptime:kernel:os:separator:title".to_owned())
        );
        assert_eq!(
            parse_options(&["--structure".to_owned(), "command".to_owned()]),
            Err(FastfetchInputError::InvalidStructure)
        );
    }

    #[test]
    fn aliases_and_direct_commands_compile_through_the_same_parser() {
        let direct = compile(
            parse_options(&[]).unwrap(),
            parse_options(&[
                "--logo".to_owned(),
                "arch".to_owned(),
                "--structure".to_owned(),
                "OS:Kernel:CPU".to_owned(),
            ])
            .unwrap(),
        );
        let alias_arguments = shell_words::join(["--logo", "arch", "--structure", "OS:Kernel:CPU"]);
        let alias = compile(
            parse_options(&[]).unwrap(),
            parse_options(&shell_words::split(&alias_arguments).unwrap()).unwrap(),
        );
        assert_eq!(direct.arguments, alias.arguments);
    }

    #[test]
    fn profile_is_strict_and_merges_before_command_options() {
        let path = test_path("profile");
        assert_eq!(read_profile(&path).unwrap().structure, None);
        fs::write(&path, r#"{"version":1}"#).unwrap();
        assert_eq!(
            compile(read_profile(&path).unwrap(), parse_options(&[]).unwrap()).arguments,
            ["--config", "none", "--pipe"]
        );
        fs::write(&path, r#"{"version":1,"logo":"arch"}"#).unwrap();
        assert_eq!(
            compile(read_profile(&path).unwrap(), parse_options(&[]).unwrap()).arguments,
            [
                "--config",
                "none",
                "--pipe",
                "--logo-type",
                "builtin",
                "--logo",
                "Arch",
            ]
        );
        fs::write(
            &path,
            r#"{"version":1,"logo":"Arch","structure":["title","os"],"separator":" :: "}"#,
        )
        .unwrap();
        let profile = read_profile(&path).unwrap();
        let invocation = compile(
            profile,
            parse_options(&["--logo".to_owned(), "Ubuntu".to_owned()]).unwrap(),
        );
        assert_eq!(
            invocation.arguments,
            [
                "--config",
                "none",
                "--pipe",
                "--logo-type",
                "builtin",
                "--logo",
                "Ubuntu",
                "--structure",
                "title:os",
                "--separator",
                " :: ",
            ]
        );
        assert_profile_error(&path, b"not json", FastfetchProfileError::Malformed);
        assert_profile_error(
            &path,
            br#"{"version":2}"#,
            FastfetchProfileError::UnsupportedVersion,
        );
        assert_profile_error(
            &path,
            br#"{"version":1,"logo":"bad"}"#,
            FastfetchProfileError::InvalidLogo,
        );
        assert_profile_error(
            &path,
            br#"{"version":1,"structure":[]}"#,
            FastfetchProfileError::InvalidStructure,
        );
        assert_profile_error(
            &path,
            br#"{"version":1,"structure":["os:kernel"]}"#,
            FastfetchProfileError::InvalidStructure,
        );
        assert_profile_error(
            &path,
            br#"{"version":1,"separator":"\n"}"#,
            FastfetchProfileError::InvalidSeparator,
        );
        fs::write(&path, vec![b'x'; PROFILE_MAX_BYTES + 1]).unwrap();
        assert_eq!(read_profile(&path), Err(FastfetchProfileError::TooLarge));
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(read_profile(&path), Err(FastfetchProfileError::UnsafePath));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn leaves_unset_logo_and_structure_out_of_the_fixed_prefix() {
        let path = test_path("unset-options");
        let fixed = ["--config", "none", "--pipe"];

        assert_eq!(prepare("", &path).await.unwrap().arguments, fixed);
        assert_eq!(
            prepare("--no-profile", &path).await.unwrap().arguments,
            fixed
        );
        fs::write(&path, r#"{"version":1}"#).unwrap();
        assert_eq!(prepare("", &path).await.unwrap().arguments, fixed);
        fs::write(&path, r#"{"version":1,"logo":"arch"}"#).unwrap();
        assert_eq!(
            prepare("", &path).await.unwrap().arguments,
            [
                "--config",
                "none",
                "--pipe",
                "--logo-type",
                "builtin",
                "--logo",
                "Arch",
            ]
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn reports_unreadable_profile_paths_without_exposing_os_errors() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let path = std::path::PathBuf::from(OsString::from_vec(b"invalid\0profile".to_vec()));
        assert_eq!(read_profile(&path), Err(FastfetchProfileError::NotReadable));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_profile_and_no_profile_can_bypass_it() {
        use std::os::unix::fs::symlink;
        let path = test_path("symlink");
        let target = path.with_file_name("target.json");
        fs::write(&target, r#"{"version":1,"structure":["os"]}"#).unwrap();
        symlink(&target, &path).unwrap();
        assert_eq!(read_profile(&path), Err(FastfetchProfileError::UnsafePath));
        assert!(matches!(
            prepare("", &path).await,
            Err(FastfetchResult::ProfileError(
                FastfetchProfileError::UnsafePath
            ))
        ));
        assert_eq!(
            prepare("--no-profile", &path).await.unwrap().arguments,
            ["--config", "none", "--pipe"]
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn no_profile_bypasses_a_malformed_profile() {
        let path = test_path("no-profile-malformed");
        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            prepare("", &path).await,
            Err(FastfetchResult::ProfileError(
                FastfetchProfileError::Malformed
            ))
        ));
        assert!(prepare("--no-profile", &path).await.is_ok());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn assert_profile_error(path: &std::path::Path, bytes: &[u8], expected: FastfetchProfileError) {
        fs::write(path, bytes).unwrap();
        assert_eq!(read_profile(path), Err(expected));
    }

    fn test_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "lavis-fastfetch-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory.join("fastfetch.json")
    }

    #[test]
    fn sanitizes_ansi_controls_carriage_returns_and_invalid_utf8() {
        let capture = Capture {
            bytes: b"\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b[2J\r\nline\rnext\0\x01\t  \xff\xe2\x80\xaehidden"
                .to_vec(),
            truncated: false,
        };

        assert_eq!(sanitize_capture(&capture), "red\nline\nnext      �hidden");
    }

    #[test]
    fn bounded_capture_keeps_draining_after_its_cap() {
        let mut capture = Capture {
            bytes: Vec::new(),
            truncated: false,
        };
        append_capture(&mut capture, b"abcd", 3);
        append_capture(&mut capture, b"efgh", 3);

        assert_eq!(capture.bytes, b"abc");
        assert!(capture.truncated);
    }

    #[test]
    fn separator_compiles_as_two_argv_elements() {
        let cmd = compile(
            parse_options(&[]).unwrap(),
            parse_options(&["--separator".to_owned(), " -> ".to_owned()]).unwrap(),
        );
        let sep_idx = cmd
            .arguments
            .iter()
            .position(|a| a == "--separator")
            .unwrap();
        assert_eq!(cmd.arguments.get(sep_idx + 1), Some(&" -> ".to_owned()));
        assert!(!cmd.arguments.iter().any(|a| a.starts_with("--separator=")));
    }

    #[test]
    fn logo_padding_left_from_cli() {
        let cmd = compile(
            parse_options(&[]).unwrap(),
            parse_options(&["--logo-padding-left".to_owned(), "4".to_owned()]).unwrap(),
        );
        let idx = cmd
            .arguments
            .iter()
            .position(|a| a == "--logo-padding-left")
            .unwrap();
        assert_eq!(cmd.arguments.get(idx + 1), Some(&"4".to_owned()));
    }

    #[test]
    fn logo_padding_right_from_cli() {
        let cmd = compile(
            parse_options(&[]).unwrap(),
            parse_options(&["--logo-padding-right".to_owned(), "5".to_owned()]).unwrap(),
        );
        let idx = cmd
            .arguments
            .iter()
            .position(|a| a == "--logo-padding-right")
            .unwrap();
        assert_eq!(cmd.arguments.get(idx + 1), Some(&"5".to_owned()));
    }

    #[test]
    fn logo_padding_top_from_cli() {
        let cmd = compile(
            parse_options(&[]).unwrap(),
            parse_options(&["--logo-padding-top".to_owned(), "1".to_owned()]).unwrap(),
        );
        let idx = cmd
            .arguments
            .iter()
            .position(|a| a == "--logo-padding-top")
            .unwrap();
        assert_eq!(cmd.arguments.get(idx + 1), Some(&"1".to_owned()));
    }

    #[test]
    fn logo_padding_all_three_from_cli() {
        let cmd = compile(
            parse_options(&[]).unwrap(),
            parse_options(&[
                "--logo-padding-left".to_owned(),
                "2".to_owned(),
                "--logo-padding-right".to_owned(),
                "3".to_owned(),
                "--logo-padding-top".to_owned(),
                "1".to_owned(),
            ])
            .unwrap(),
        );
        assert_eq!(
            cmd.arguments,
            [
                "--config",
                "none",
                "--pipe",
                "--logo-padding-left",
                "2",
                "--logo-padding-right",
                "3",
                "--logo-padding-top",
                "1",
            ]
        );
    }

    #[test]
    fn command_padding_overrides_profile() {
        let profile = PartialOptions {
            logo_padding: LogoPadding {
                left: Some(2),
                right: Some(3),
                top: None,
            },
            ..PartialOptions::default()
        };
        let cmd = compile(
            profile,
            parse_options(&["--logo-padding-right".to_owned(), "5".to_owned()]).unwrap(),
        );
        assert_eq!(
            cmd.arguments,
            [
                "--config",
                "none",
                "--pipe",
                "--logo-padding-left",
                "2",
                "--logo-padding-right",
                "5",
            ]
        );
    }

    #[test]
    fn unspecified_dimensions_remain_unset() {
        let profile = PartialOptions {
            logo_padding: LogoPadding {
                left: Some(2),
                right: None,
                top: None,
            },
            ..PartialOptions::default()
        };
        let cmd = compile(profile, parse_options(&[]).unwrap());
        assert_eq!(
            cmd.arguments,
            ["--config", "none", "--pipe", "--logo-padding-left", "2",]
        );
    }

    #[test]
    fn duplicate_logo_padding_option_is_rejected() {
        assert_eq!(
            parse_options(&[
                "--logo-padding-left".to_owned(),
                "1".to_owned(),
                "--logo-padding-left".to_owned(),
                "2".to_owned(),
            ]),
            Err(FastfetchInputError::DuplicateOption)
        );
    }

    #[test]
    fn missing_logo_padding_value_is_rejected() {
        assert_eq!(
            parse_options(&["--logo-padding-left".to_owned()]),
            Err(FastfetchInputError::MissingValue)
        );
    }

    #[test]
    fn non_numeric_logo_padding_is_rejected() {
        assert_eq!(
            parse_logo_padding("abc"),
            Err(FastfetchInputError::InvalidLogoPadding)
        );
    }

    #[test]
    fn negative_logo_padding_is_rejected() {
        assert_eq!(
            parse_logo_padding("-1"),
            Err(FastfetchInputError::InvalidLogoPadding)
        );
    }

    #[test]
    fn padding_value_33_is_rejected() {
        assert_eq!(
            parse_logo_padding("33"),
            Err(FastfetchInputError::InvalidLogoPadding)
        );
    }

    #[test]
    fn padding_boundary_values_0_and_32_are_accepted() {
        assert_eq!(parse_logo_padding("0").unwrap(), 0);
        assert_eq!(parse_logo_padding("32").unwrap(), 32);
    }

    #[test]
    fn malformed_profile_padding_is_rejected() {
        let path = test_path("malformed-padding");
        assert_profile_error(
            &path,
            br#"{"version":1,"logo_padding_left":"abc"}"#,
            FastfetchProfileError::Malformed,
        );
        fs::remove_file(&path).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn out_of_range_profile_padding_is_rejected() {
        let path = test_path("oor-padding");
        assert_profile_error(
            &path,
            br#"{"version":1,"logo_padding_left":33}"#,
            FastfetchProfileError::InvalidLogoPadding,
        );
        fs::remove_file(&path).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn unknown_profile_fields_remain_rejected() {
        let path = test_path("unknown-field");
        assert_profile_error(
            &path,
            br#"{"version":1,"nonexistent":true}"#,
            FastfetchProfileError::Malformed,
        );
        fs::remove_file(&path).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn no_profile_ignores_all_padding() {
        let path = test_path("no-profile-padding");
        fs::write(&path, r#"{"version":1,"logo_padding_left":2}"#).unwrap();
        let cmd = prepare("--no-profile", &path).await.unwrap();
        assert_eq!(cmd.arguments, ["--config", "none", "--pipe"]);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
