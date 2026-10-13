//! Console progress for a batch install.
//!
//! While a batch runs, one package installs while the next downloads (#292),
//! so the display shows both at once: a spinner line for the package
//! installing, and per-part bars with a total for the package downloading.

use std::sync::Mutex;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use xearthlayer::manager::{
    DownloadProgress, DownloadProgressCallback, InstallObserver, InstallStage, PartState,
};
use xearthlayer_package::PackageMetadata;

/// Renders a batch install's progress to the terminal.
#[derive(Default)]
pub struct ConsoleInstallObserver {
    multi: MultiProgress,
    /// Spinner for the package installing, created on first use.
    install_line: Mutex<Option<ProgressBar>>,
    /// Bars of the package downloading now.
    download_bars: Mutex<Vec<ProgressBar>>,
}

impl ConsoleInstallObserver {
    /// Clear every bar. Call once the batch has finished.
    pub fn finish(&self) {
        clear(&mut self.download_bars.lock().unwrap());
        if let Some(line) = self.install_line.lock().unwrap().take() {
            line.finish_and_clear();
        }
    }
}

impl InstallObserver for ConsoleInstallObserver {
    fn download_started(&self, package: &PackageMetadata) -> Option<DownloadProgressCallback> {
        let mut bars = self.download_bars.lock().unwrap();
        clear(&mut bars);

        let header = self.multi.add(ProgressBar::new(0));
        header.set_style(ProgressStyle::with_template("{msg:.bold}").unwrap());
        header.set_message(format!(
            "Downloading {} v{}",
            package.title, package.package_version
        ));

        let queued_style = ProgressStyle::with_template("  {prefix:.dim} {msg:.dim}").unwrap();
        let parts: Vec<ProgressBar> = package
            .parts
            .iter()
            .map(|part| {
                let bar = self.multi.add(ProgressBar::new(0));
                bar.set_style(queued_style.clone());
                bar.set_prefix(part.filename.clone());
                bar.set_message("(queued)");
                bar
            })
            .collect();

        let footer = self.multi.add(ProgressBar::new(0));
        footer.set_style(
            ProgressStyle::with_template(
                "  Total [{bar:25.green/dim}] {percent:>3}% {bytes:>10}/{total_bytes:<10} {binary_bytes_per_sec:>12}",
            )
            .unwrap()
            .progress_chars("##-"),
        );

        bars.push(header);
        bars.extend(parts.iter().cloned());
        bars.push(footer.clone());

        Some(part_progress(parts, footer))
    }

    fn install_stage(&self, package: &PackageMetadata, stage: InstallStage) {
        let mut line = self.install_line.lock().unwrap();
        let line = line.get_or_insert_with(|| {
            // Above the download block, which is rebuilt per package.
            let bar = self.multi.insert(0, ProgressBar::new_spinner());
            bar.set_style(ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap());
            bar.enable_steady_tick(Duration::from_millis(100));
            bar
        });
        // One update, so a redraw never pairs a package with another's stage.
        line.set_message(format!("{}: {}", package.title, stage.name()));
    }
}

/// Finish and remove every bar in `bars`.
fn clear(bars: &mut Vec<ProgressBar>) {
    for bar in bars.drain(..) {
        bar.finish_and_clear();
    }
}

/// Callback that drives one package's part bars and its total.
fn part_progress(parts: Vec<ProgressBar>, footer: ProgressBar) -> DownloadProgressCallback {
    let part_style = ProgressStyle::with_template(
        "  {prefix:.dim} [{bar:25.cyan/dim}] {percent:>3}% {bytes:>10}/{total_bytes:<10} {binary_bytes_per_sec:>12}",
    )
    .unwrap()
    .progress_chars("##-");
    let fail_style = ProgressStyle::with_template("  {prefix:.red} {msg:.red}").unwrap();
    let retry_style = ProgressStyle::with_template("  {prefix:.yellow} {msg:.yellow}").unwrap();

    Box::new(move |progress: &DownloadProgress| {
        for part in &progress.parts {
            let Some(bar) = parts.get(part.index) else {
                continue;
            };
            // Skip already-finished bars to avoid duplicate rendering
            if bar.is_finished() {
                continue;
            }
            match &part.state {
                PartState::Queued => {}
                PartState::Downloading => {
                    if let Some(total) = part.total_bytes {
                        bar.set_length(total);
                    }
                    bar.set_style(part_style.clone());
                    bar.set_position(part.bytes_downloaded);
                }
                PartState::Done => {
                    // Clear completed bars so the list scrolls naturally
                    bar.finish_and_clear();
                }
                PartState::Failed { reason, .. } => {
                    bar.set_style(fail_style.clone());
                    bar.abandon_with_message(format!("(failed: {})", reason));
                }
                PartState::Retrying { attempt } => {
                    bar.set_style(retry_style.clone());
                    bar.set_message(format!("(retry {}/3)", attempt));
                }
            }
        }
        if let Some(total) = progress.total_bytes {
            footer.set_length(total);
        }
        footer.set_position(progress.total_bytes_downloaded);
    })
}
