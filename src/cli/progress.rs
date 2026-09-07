// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::indexing::IndexProgress;
use indicatif::{
    MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub struct MultiStageProgressView {
    pub(crate) scan_bar: ProgressBar,
    pub(crate) diff_bar: ProgressBar,
    pub(crate) extract_bar: ProgressBar,
    pub(crate) merge_bar: ProgressBar,
    pub(crate) bar_style: ProgressStyle,
    pub(crate) max_extracted: AtomicUsize,
    pub(crate) is_finished: AtomicBool,
    pub(crate) enabled: bool,
}

impl MultiStageProgressView {
    pub fn for_stderr(enabled: bool) -> Self {
        let target = if enabled {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        };
        Self::with_draw_target(target)
    }

    pub fn for_stdout(enabled: bool) -> Self {
        let target = if enabled {
            ProgressDrawTarget::stdout()
        } else {
            ProgressDrawTarget::hidden()
        };
        Self::with_draw_target(target)
    }

    pub fn hidden() -> Self {
        Self::with_draw_target(ProgressDrawTarget::hidden())
    }

    pub fn with_draw_target(draw_target: ProgressDrawTarget) -> Self {
        let enabled = !draw_target.is_hidden();
        let mp = MultiProgress::with_draw_target(draw_target);
        let spinner_style = ProgressStyle::default_spinner()
            .tick_chars(r"/|\-")
            .template("{prefix:.bold} {spinner:.green} {msg}")
            .unwrap();
        let bar_template = "{prefix:.bold} [{bar:24}] \
                            {pos}/{len} ({percent}%) {msg}";
        let bar_style = ProgressStyle::default_bar()
            .template(bar_template)
            .unwrap()
            .progress_chars("=> ");

        let scan_bar = mp
            .add(ProgressBar::new_spinner().with_style(spinner_style.clone()));
        scan_bar.set_prefix("[1/4] Scan files      ");
        scan_bar.set_message("Scanning...");
        if enabled {
            scan_bar.enable_steady_tick(std::time::Duration::from_millis(100));
        }

        let diff_bar = mp
            .add(ProgressBar::new_spinner().with_style(spinner_style.clone()));
        diff_bar.set_prefix("[2/4] Detect changes  ");
        diff_bar.set_message("- Waiting");

        let extract_bar = mp
            .add(ProgressBar::new_spinner().with_style(spinner_style.clone()));
        extract_bar.set_prefix("[3/4] Extract metadata");
        extract_bar.set_message("- Waiting");

        let merge_bar =
            mp.add(ProgressBar::new_spinner().with_style(spinner_style));
        merge_bar.set_prefix("[4/4] Merge database  ");
        merge_bar.set_message("- Waiting");

        Self {
            scan_bar,
            diff_bar,
            extract_bar,
            merge_bar,
            bar_style,
            max_extracted: AtomicUsize::new(0),
            is_finished: AtomicBool::new(false),
            enabled,
        }
    }

    pub fn handle_progress(&self, p: IndexProgress) {
        match p {
            IndexProgress::Scanning { count } => {
                self.scan_bar.set_message(format!("{count} files found..."));
            }
            IndexProgress::Diffing => {
                self.scan_bar.finish_with_message("Done");
                self.diff_bar.set_message("Detecting...");
                if self.enabled {
                    self.diff_bar.enable_steady_tick(
                        std::time::Duration::from_millis(100),
                    );
                }
            }
            IndexProgress::Extracting { current, total } => {
                if !self.diff_bar.is_finished() {
                    self.scan_bar.finish_with_message("Done");
                    self.diff_bar.finish_with_message("Done");
                }
                if total == 0 {
                    self.extract_bar.finish_with_message("Done (0 changes)");
                } else {
                    if self.extract_bar.length() != Some(total as u64) {
                        self.extract_bar.set_style(self.bar_style.clone());
                        self.extract_bar.set_length(total as u64);
                    }
                    let prev = self
                        .max_extracted
                        .fetch_max(current, Ordering::Relaxed);
                    if current > prev {
                        self.extract_bar.set_position(current as u64);
                    }
                    self.extract_bar.set_message("");
                }
            }
            IndexProgress::Merging => {
                self.scan_bar.finish_with_message("Done");
                self.diff_bar.finish_with_message("Done");
                if !self.extract_bar.is_finished() {
                    self.extract_bar.finish_with_message("Done");
                }
                self.merge_bar.set_message("Merging...");
                if self.enabled {
                    self.merge_bar.enable_steady_tick(
                        std::time::Duration::from_millis(100),
                    );
                }
            }
        }
    }

    pub fn finish(&self) {
        self.scan_bar.finish_with_message("Done");
        self.diff_bar.finish_with_message("Done");
        self.extract_bar.finish_with_message("Done");
        self.merge_bar.finish_with_message("Done");
        self.is_finished.store(true, Ordering::SeqCst);
    }

    pub fn finish_dry_run(&self) {
        self.scan_bar.finish_with_message("Done (dry-run)");
        self.diff_bar.finish_and_clear();
        self.extract_bar.finish_and_clear();
        self.merge_bar.finish_and_clear();
        self.is_finished.store(true, Ordering::SeqCst);
    }
}

impl Drop for MultiStageProgressView {
    fn drop(&mut self) {
        if self.enabled && !self.is_finished.load(Ordering::SeqCst) {
            self.scan_bar.abandon();
            self.diff_bar.abandon();
            self.extract_bar.abandon();
            self.merge_bar.abandon();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_stage_progress_view_transitions_and_monotonicity() {
        let view = MultiStageProgressView::hidden();
        view.handle_progress(IndexProgress::Scanning { count: 100 });
        view.handle_progress(IndexProgress::Diffing);
        view.handle_progress(IndexProgress::Extracting {
            current: 0,
            total: 200,
        });
        view.handle_progress(IndexProgress::Extracting {
            current: 50,
            total: 200,
        });
        view.handle_progress(IndexProgress::Extracting {
            current: 20,
            total: 200,
        });
        assert_eq!(
            view.max_extracted
                .load(std::sync::atomic::Ordering::Relaxed),
            50
        );
        view.handle_progress(IndexProgress::Merging);
        view.finish();
        assert!(view.is_finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_multi_stage_progress_view_finish_dry_run() {
        let view = MultiStageProgressView::hidden();
        view.handle_progress(IndexProgress::Scanning { count: 10 });
        view.finish_dry_run();
        assert!(view.is_finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_multi_stage_progress_view_draw_target_enables_derived() {
        let hidden_view = MultiStageProgressView::with_draw_target(
            ProgressDrawTarget::hidden(),
        );
        assert!(!hidden_view.enabled);

        let stderr_view = MultiStageProgressView::for_stderr(false);
        assert!(!stderr_view.enabled);

        let active_view = MultiStageProgressView::for_stderr(true);
        assert!(active_view.enabled);
    }

    #[test]
    fn test_multi_stage_progress_view_extract_bar_starts_waiting_and_switches_on_extracting(
    ) {
        let view = MultiStageProgressView::hidden();
        assert_eq!(view.extract_bar.length(), None);

        view.handle_progress(IndexProgress::Extracting {
            current: 10,
            total: 100,
        });
        assert_eq!(view.extract_bar.length(), Some(100));
        assert_eq!(view.extract_bar.position(), 10);
    }
}
