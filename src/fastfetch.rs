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
const MAX_STRUCTURE_COMPONENTS: usize = 12;
const DEFAULT_STRUCTURE: &[Module] = &[
    Module::Os,
    Module::Kernel,
    Module::Cpu,
    Module::Gpu,
    Module::Memory,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastfetchInputError {
    Tokenization,
    UnsupportedOption,
    MissingValue,
    DuplicateOption,
    InvalidLogo,
    InvalidStructure,
    InvalidSeparator,
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
    ProfileError,
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
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PartialOptions {
    no_profile: bool,
    logo: Option<Logo>,
    structure: Option<Vec<Module>>,
    separator: Option<String>,
}

#[derive(Debug)]
struct EffectiveOptions {
    logo: Logo,
    structure: Vec<Module>,
    separator: Option<String>,
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
    structure: Vec<String>,
    separator: Option<String>,
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
            Err(()) => return Err(FastfetchResult::ProfileError),
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
                let value = tokens.get(index + 1).ok_or(FastfetchInputError::MissingValue)?;
                options.logo = Some(parse_logo(value)?);
                index += 2;
            }
            "--structure" => {
                if options.structure.is_some() {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                let value = tokens.get(index + 1).ok_or(FastfetchInputError::MissingValue)?;
                options.structure = Some(parse_structure(value)?);
                index += 2;
            }
            "--separator" => {
                if options.separator.is_some() {
                    return Err(FastfetchInputError::DuplicateOption);
                }
                let value = tokens.get(index + 1).ok_or(FastfetchInputError::MissingValue)?;
                validate_separator(value)?;
                options.separator = Some(value.clone());
                index += 2;
            }
            _ => return Err(FastfetchInputError::UnsupportedOption),
        }
    }
    Ok(options)
}

fn parse_logo(value: &str) -> Result<Logo, FastfetchInputError> {
    Ok(match value {
        "none" => Logo::None,
        "Alpine" => Logo::Builtin(BuiltinLogo::Alpine),
        "Arch" => Logo::Builtin(BuiltinLogo::Arch),
        "Debian" => Logo::Builtin(BuiltinLogo::Debian),
        "Fedora" => Logo::Builtin(BuiltinLogo::Fedora),
        "FreeBSD" => Logo::Builtin(BuiltinLogo::FreeBSD),
        "Linux" => Logo::Builtin(BuiltinLogo::Linux),
        "MacOS" => Logo::Builtin(BuiltinLogo::MacOS),
        "NixOS" => Logo::Builtin(BuiltinLogo::NixOS),
        "OpenBSD" => Logo::Builtin(BuiltinLogo::OpenBSD),
        "Ubuntu" => Logo::Builtin(BuiltinLogo::Ubuntu),
        "Windows" => Logo::Builtin(BuiltinLogo::Windows),
        _ => return Err(FastfetchInputError::InvalidLogo),
    })
}

fn parse_structure(value: &str) -> Result<Vec<Module>, FastfetchInputError> {
    let mut components = Vec::new();
    for component in value.split(':') {
        let module = match component {
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

async fn load_profile(path: &Path) -> Result<PartialOptions, ()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_profile(&path))
        .await
        .map_err(|_| ())?
}

fn read_profile(path: &Path) -> Result<PartialOptions, ()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PartialOptions::default());
        }
        Err(_) => return Err(()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| ())?
        .take((PROFILE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > PROFILE_MAX_BYTES {
        return Err(());
    }
    let profile: Profile = serde_json::from_slice(&bytes).map_err(|_| ())?;
    if profile.version != 1 {
        return Err(());
    }
    let logo = profile
        .logo
        .as_deref()
        .map(parse_logo)
        .transpose()
        .map_err(|_| ())?;
    if profile
        .structure
        .iter()
        .any(|component| component.contains(':'))
    {
        return Err(());
    }
    let structure = parse_structure(&profile.structure.join(":")).map_err(|_| ())?;
    if let Some(separator) = &profile.separator {
        validate_separator(separator).map_err(|_| ())?;
    }
    Ok(PartialOptions {
        no_profile: false,
        logo,
        structure: Some(structure),
        separator: profile.separator,
    })
}

fn compile(profile: PartialOptions, command: PartialOptions) -> Invocation {
    let effective = EffectiveOptions {
        logo: command.logo.or(profile.logo).unwrap_or(Logo::None),
        structure: command
            .structure
            .or(profile.structure)
            .unwrap_or_else(|| DEFAULT_STRUCTURE.to_vec()),
        separator: command.separator.or(profile.separator),
    };
    let mut arguments = vec![
        "--config".to_owned(),
        "none".to_owned(),
        "--pipe".to_owned(),
    ];
    match effective.logo {
        Logo::None => arguments.extend(["--logo".to_owned(), "none".to_owned()]),
        Logo::Builtin(logo) => arguments.extend([
            "--logo-type".to_owned(),
            "builtin".to_owned(),
            "--logo".to_owned(),
            logo.as_str().to_owned(),
        ]),
    }
    arguments.extend([
        "--structure".to_owned(),
        effective
            .structure
            .iter()
            .map(|module| module.as_str())
            .collect::<Vec<_>>()
            .join(":"),
    ]);
    if let Some(separator) = effective.separator {
        arguments.push(format!("--separator={separator}"));
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
            Self::Title => "Title",
            Self::Separator => "Separator",
            Self::Os => "OS",
            Self::Kernel => "Kernel",
            Self::Uptime => "Uptime",
            Self::Cpu => "CPU",
            Self::Memory => "Memory",
            Self::Gpu => "GPU",
            Self::Packages => "Packages",
            Self::Shell => "Shell",
            Self::Terminal => "Terminal",
            Self::TerminalSize => "TerminalSize",
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
        Capture, FastfetchInputError, FastfetchResult, PROFILE_MAX_BYTES, append_capture, compile,
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
                "NixOS".to_owned(),
                "--structure".to_owned(),
                "title:separator:os:terminalsize".to_owned(),
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
                "Title:Separator:OS:TerminalSize",
                "--separator= -> ",
            ]
        );
        assert_eq!(
            compile(parse_options(&[]).unwrap(), parse_options(&[]).unwrap()).arguments,
            [
                "--config",
                "none",
                "--pipe",
                "--logo",
                "none",
                "--structure",
                "OS:Kernel:CPU:GPU:Memory",
            ]
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
            vec!["--structure".to_owned(), "OS".to_owned()],
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
    fn profile_is_strict_and_merges_before_command_options() {
        let path = test_path("profile");
        assert_eq!(read_profile(&path).unwrap().structure, None);
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
                "Title:OS",
                "--separator= :: ",
            ]
        );
        fs::write(
            &path,
            r#"{"version":1,"structure":["os"],"unexpected":true}"#,
        )
        .unwrap();
        assert!(read_profile(&path).is_err());
        fs::write(
            &path,
            r#"{"version":1,"structure":["os","os"]}"#,
        )
        .unwrap();
        assert!(read_profile(&path).is_err());
        fs::write(&path, vec![b'x'; PROFILE_MAX_BYTES + 1]).unwrap();
        assert!(read_profile(&path).is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(read_profile(&path).is_err());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_profile_and_no_profile_can_bypass_it() {
        use std::os::unix::fs::symlink;
        let path = test_path("symlink");
        let target = path.with_file_name("target.json");
        fs::write(&target, r#"{"version":1,"structure":["os"]}"#).unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_profile(&path).is_err());
        assert!(matches!(
            prepare("", &path).await,
            Err(FastfetchResult::ProfileError)
        ));
        assert_eq!(
            prepare("--no-profile", &path).await.unwrap().arguments,
            [
                "--config",
                "none",
                "--pipe",
                "--logo",
                "none",
                "--structure",
                "OS:Kernel:CPU:GPU:Memory",
            ]
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
}
