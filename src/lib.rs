//! Amphisbaena: a Nix build coordinator for GitHub repositories.

pub mod api;
mod drv;
mod repo_config;
pub mod server;
mod text;
pub mod worker;

/// Logs to stderr, filtered by `RUST_LOG`, at `info` by default. In the
/// journal, each line has its level as its priority.
pub fn init_tracing() {
    use std::io::IsTerminal;

    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let format = tracing_subscriber::fmt::format().without_time();
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if stderr_is_journal() {
        builder
            .with_ansi(false)
            .event_format(JournalPriority(format))
            .init();
    } else {
        builder
            .with_ansi(std::io::stderr().is_terminal())
            .event_format(format)
            .init();
    }
}

/// Whether stderr is connected to the journal, which systemd tells by the
/// device and inode in `$JOURNAL_STREAM`.
fn stderr_is_journal() -> bool {
    use std::os::unix::fs::MetadataExt;

    let Some(stream) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Some((device, inode)) = stream.to_str().and_then(|stream| stream.split_once(':')) else {
        return false;
    };
    std::fs::metadata("/proc/self/fd/2")
        .is_ok_and(|stderr| stderr.dev().to_string() == device && stderr.ino().to_string() == inode)
}

/// Prefixes each line with the syslog priority of its level, which journald
/// strips and records as the priority of the line.
struct JournalPriority<F>(F);

impl<S, N, F> tracing_subscriber::fmt::FormatEvent<S, N> for JournalPriority<F>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
    F: tracing_subscriber::fmt::FormatEvent<S, N>,
{
    fn format_event(
        &self,
        context: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let priority = match *event.metadata().level() {
            tracing::Level::ERROR => 3,
            tracing::Level::WARN => 4,
            tracing::Level::INFO => 6,
            tracing::Level::DEBUG | tracing::Level::TRACE => 7,
        };
        // Messages may span lines, e.g. a command's stderr.
        let mut formatted = String::new();
        let buffer = tracing_subscriber::fmt::format::Writer::new(&mut formatted);
        self.0.format_event(context, buffer, event)?;
        for line in formatted.lines() {
            writeln!(writer, "<{priority}>{line}")?;
        }
        Ok(())
    }
}
