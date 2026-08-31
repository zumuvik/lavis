use super::*;

impl RuntimeState {
    /// Builds the `info` reply: a dynamic caption plus the static branding
    /// image URL.
    pub(super) fn execute_info(&mut self) -> RuntimeExecution {
        let locale = self.locale();
        let prefix = self.prefix().to_owned();
        let owner = self
            .self_identity()
            .map(crate::info::owner_label)
            .unwrap_or_else(|| info_text(locale, InfoText::Unknown).to_owned());
        let upstream_data = self.upstream_revision();
        let upstream = match &upstream_data {
            Some(data) => crate::info::short_commit(&data.revision).to_owned(),
            None => info_text(locale, InfoText::Unavailable).to_owned(),
        };
        let upstream_status = upstream_data
            .as_ref()
            .map(|data| render_revision_status(locale, data.relation))
            .unwrap_or_else(|| info_text(locale, InfoText::Unavailable).to_owned());
        let version_status = if !self.upstream_version_known {
            String::new()
        } else {
            match version_relation(
                env!("CARGO_PKG_VERSION"),
                upstream_data
                    .as_ref()
                    .and_then(|data| data.version.as_deref()),
            ) {
                VersionRelation::Current => match locale {
                    Locale::English => "current ✅".to_owned(),
                    Locale::Russian => "актуальная ✅".to_owned(),
                },
                VersionRelation::NewerAvailable => {
                    let upstream_version = upstream_data
                        .as_ref()
                        .and_then(|data| data.version.as_deref())
                        .unwrap_or_default();
                    match locale {
                        Locale::English => format!("newer available: {upstream_version} ⬆️"),
                        Locale::Russian => format!("доступна новая: {upstream_version} ⬆️"),
                    }
                }
                VersionRelation::Unavailable => String::new(),
            }
        };
        let built_in_modules = crate::modules::modules().len();
        let total_modules = built_in_modules + self.external_descriptors().len();
        let active_modules = built_in_modules
            + self
                .external_snapshot
                .module_statuses
                .iter()
                .filter(|status| status.status == ExternalModuleRuntimeStatus::Running)
                .count();
        let caption = render_info_text(
            locale,
            InfoCaptionData {
                owner: &owner,
                version: env!("CARGO_PKG_VERSION"),
                version_status: &version_status,
                commit: crate::info::short_commit(crate::info::build_rev()),
                upstream: &upstream,
                upstream_status: &upstream_status,
                prefix: &prefix,
                active_modules,
                total_modules,
                host: self.info_local_metadata.host,
                os: &self.info_local_metadata.os,
            },
        );
        RuntimeExecution {
            response: Response::four_blockquotes_with_locale(locale, caption),
            media: self.info_local_metadata.media.clone(),
            provision: None,
            shutdown: None,
            post_edit: None,
            onboarding_page: false,
        }
    }
}
pub(super) fn enabled_label(locale: Locale, enabled: bool) -> &'static str {
    lm_label(
        locale,
        if enabled {
            LmLabel::Enabled
        } else {
            LmLabel::Disabled
        },
    )
}

pub(super) fn management_label(
    locale: Locale,
    management: control::ModuleManagement,
) -> &'static str {
    match management {
        control::ModuleManagement::Manual => lm_label(locale, LmLabel::Manual),
        control::ModuleManagement::DeclarativeNixOs => lm_label(locale, LmLabel::Declarative),
    }
}

pub(super) fn diagnostic_label(
    locale: Locale,
    diagnostic: Option<&control::ModuleDiagnostic>,
) -> &'static str {
    match diagnostic {
        Some(control::ModuleDiagnostic::InvalidModuleId) => lm_label(locale, LmLabel::InvalidId),
        Some(control::ModuleDiagnostic::InvalidManifest) => {
            lm_label(locale, LmLabel::InvalidManifest)
        }
        None => lm_label(locale, LmLabel::None),
    }
}

pub(super) fn runtime_status_from_snapshot(
    locale: Locale,
    snapshot: &ExternalRuntimeSnapshot,
    id: &str,
) -> String {
    snapshot
        .module_statuses
        .iter()
        .find(|status| status.id == id)
        .map(|status| lm_runtime_status(locale, status.status).to_owned())
        .unwrap_or_else(|| lm_label(locale, LmLabel::NotRunning).to_owned())
}

pub(super) async fn fresh_runtime_status(
    locale: Locale,
    handle: Option<&ExternalManagerHandle>,
    cached: &ExternalRuntimeSnapshot,
    id: &str,
) -> String {
    match handle {
        Some(handle) => runtime_status_from_snapshot(locale, &handle.snapshot().await, id),
        None => runtime_status_from_snapshot(locale, cached, id),
    }
}

pub(super) fn capabilities_label(locale: Locale, capabilities: &[ExternalCapability]) -> String {
    if capabilities.is_empty() {
        lm_label(locale, LmLabel::None).to_owned()
    } else {
        capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(super) fn commands_label(
    locale: Locale,
    commands: &[crate::external_modules::manifest::ExternalCommandDescriptor],
) -> String {
    if commands.is_empty() {
        lm_label(locale, LmLabel::None).to_owned()
    } else {
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(super) fn bounded_list(locale: Locale, values: &[String]) -> String {
    const MAX_ITEMS: usize = 8;
    const MAX_VALUE_CHARS: usize = 96;
    if values.is_empty() {
        return lm_label(locale, LmLabel::None).to_owned();
    }
    let mut rendered = values
        .iter()
        .take(MAX_ITEMS)
        .map(|value| value.chars().take(MAX_VALUE_CHARS).collect::<String>())
        .collect::<Vec<_>>();
    if values.len() > MAX_ITEMS {
        let remaining = values.len() - MAX_ITEMS;
        rendered.push(match locale {
            Locale::English => format!("+{remaining}"),
            Locale::Russian => format!("ещё {remaining}"),
        });
    }
    rendered.join(", ")
}

pub(super) fn fastfetch_response(
    result: FastfetchResult,
    locale: Locale,
    prefix: &str,
    profile_path: &std::path::Path,
) -> Response {
    match result {
        FastfetchResult::Success(response) => response,
        FastfetchResult::Empty => fastfetch_failure(locale, FastfetchText::Empty, prefix),
        FastfetchResult::TimedOut => fastfetch_failure(locale, FastfetchText::TimedOut, prefix),
        FastfetchResult::Unavailable => {
            fastfetch_failure(locale, FastfetchText::Unavailable, prefix)
        }
        FastfetchResult::NonZero { code, .. } => Response::plain_with_locale(
            locale,
            fastfetch_text(locale, FastfetchText::NonZero)
                .replace("{code}", &code.to_string())
                .replace("{prefix}", prefix),
        ),
        FastfetchResult::UnexpectedStatus => {
            fastfetch_failure(locale, FastfetchText::UnexpectedStatus, prefix)
        }
        FastfetchResult::InvalidArguments(error) => {
            fastfetch_failure(locale, fastfetch_input_text(error), prefix)
        }
        FastfetchResult::ProfileError(error) => Response::plain_with_locale(
            locale,
            fastfetch_text(locale, fastfetch_profile_error_text(error))
                .replace("{path}", &format!("{profile_path:?}"))
                .replace("{prefix}", prefix),
        ),
    }
}

fn fastfetch_profile_error_text(error: FastfetchProfileError) -> FastfetchText {
    match error {
        FastfetchProfileError::NotReadable => FastfetchText::ProfileNotReadable,
        FastfetchProfileError::Malformed => FastfetchText::ProfileMalformed,
        FastfetchProfileError::UnsupportedVersion => FastfetchText::ProfileUnsupportedVersion,
        FastfetchProfileError::TooLarge => FastfetchText::ProfileTooLarge,
        FastfetchProfileError::UnsafePath => FastfetchText::ProfileUnsafePath,
        FastfetchProfileError::InvalidLogo => FastfetchText::ProfileInvalidLogo,
        FastfetchProfileError::InvalidStructure => FastfetchText::ProfileInvalidStructure,
        FastfetchProfileError::InvalidSeparator => FastfetchText::ProfileInvalidSeparator,
        FastfetchProfileError::InvalidLogoPadding => FastfetchText::ProfileInvalidLogoPadding,
    }
}

fn fastfetch_failure(locale: Locale, key: FastfetchText, prefix: &str) -> Response {
    Response::plain_with_locale(
        locale,
        fastfetch_text(locale, key).replace("{prefix}", prefix),
    )
}

fn fastfetch_input_text(error: FastfetchInputError) -> FastfetchText {
    match error {
        FastfetchInputError::Tokenization => FastfetchText::InputTokenization,
        FastfetchInputError::UnsupportedOption => FastfetchText::InputUnsupportedOption,
        FastfetchInputError::MissingValue => FastfetchText::InputMissingValue,
        FastfetchInputError::DuplicateOption => FastfetchText::InputDuplicateOption,
        FastfetchInputError::InvalidLogo => FastfetchText::InputInvalidLogo,
        FastfetchInputError::InvalidStructure => FastfetchText::InputInvalidStructure,
        FastfetchInputError::InvalidSeparator => FastfetchText::InputInvalidSeparator,
        FastfetchInputError::InvalidLogoPadding => FastfetchText::InputInvalidLogoPadding,
    }
}

pub(super) async fn telegram_ping(
    client: &Client,
    message_id: i32,
) -> Result<Duration, grammers_mtsender::InvocationError> {
    let started_at = Instant::now();
    client
        .invoke(&tl::functions::Ping {
            ping_id: i64::from(message_id),
        })
        .await?;
    Ok(started_at.elapsed())
}

pub(super) fn log_ping_failure(
    action: &Action,
    message_id: i32,
    error: &grammers_mtsender::InvocationError,
) {
    tracing::warn!(
        event = "telegram_ping_failed",
        command = action.name(),
        message_id,
        error_category = invocation_error_category(error),
        "Telegram ping failed"
    );
}

#[derive(Debug, Default)]
pub(super) struct ProcStats {
    pub(super) system_uptime: Option<Duration>,
    pub(super) memory_kib: Option<u64>,
}

pub(super) async fn read_proc_stats() -> ProcStats {
    tokio::task::spawn_blocking(|| ProcStats {
        system_uptime: std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|uptime| parse_system_uptime(&uptime)),
        memory_kib: std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| parse_memory_kib(&status)),
    })
    .await
    .unwrap_or_default()
}

pub(super) fn log_unavailable_proc_stats(proc_stats: &ProcStats) {
    if proc_stats.system_uptime.is_none() {
        tracing::debug!(
            event = "proc_stat_unavailable",
            stat = "system_uptime",
            "Proc stat unavailable"
        );
    }
    if proc_stats.memory_kib.is_none() {
        tracing::debug!(
            event = "proc_stat_unavailable",
            stat = "memory",
            "Proc stat unavailable"
        );
    }
}

pub(super) fn parse_system_uptime(input: &str) -> Option<Duration> {
    let seconds = input.split_whitespace().next()?.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds >= 0.0)
        .then_some(seconds)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
}

pub(super) fn parse_memory_kib(input: &str) -> Option<u64> {
    input.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("VmRSS:") {
            return None;
        }
        let value = fields.next()?;
        (fields.next() == Some("kB"))
            .then(|| value.parse().ok())
            .flatten()
    })
}

pub(super) fn format_latency(latency: Duration) -> String {
    if latency < Duration::from_millis(1) {
        "<1 ms".to_owned()
    } else {
        format!("{} ms", latency.as_millis())
    }
}

pub(super) fn format_duration(duration: Duration) -> String {
    let mut seconds = duration.as_secs();
    let days = seconds / 86_400;
    seconds %= 86_400;
    let hours = seconds / 3_600;
    seconds %= 3_600;
    let minutes = seconds / 60;
    seconds %= 60;

    if days > 0 {
        format!("{days}d {hours:02}h {minutes:02}m {seconds:02}s")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

pub(super) fn format_stats(
    locale: Locale,
    telegram: &str,
    lavis_uptime: Duration,
    proc_stats: &ProcStats,
    recognized_commands: u64,
) -> String {
    let system_uptime = proc_stats
        .system_uptime
        .map(format_duration)
        .unwrap_or_else(|| stats_text(locale, StatsText::Unavailable).to_owned());
    let memory = proc_stats
        .memory_kib
        .map(|memory_kib| format!("{:.1} MiB RSS", memory_kib as f64 / 1024.0))
        .unwrap_or_else(|| stats_text(locale, StatsText::Unavailable).to_owned());

    render_stats_text(
        locale,
        telegram,
        &format_duration(lavis_uptime),
        &system_uptime,
        &memory,
        recognized_commands,
        env!("CARGO_PKG_VERSION"),
    )
}
