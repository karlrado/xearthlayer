//! Pipelined installation of several packages (#292).
//!
//! Downloading is network bound and installing is disk bound, so the two
//! barely compete. [`run`] installs package `n` on a worker thread while the
//! calling thread downloads package `n + 1`, cutting a batch's wall clock time
//! towards the longer of the two stages instead of their sum.
//!
//! Overlap costs disk space: package `n`'s archive and extracted tree coexist
//! with package `n + 1`'s incoming parts. [`schedule_for`] sizes that peak up
//! front and falls back to [`Schedule::Sequential`] when only one package at a
//! time fits, so a batch is never refused where installing the packages one by
//! one would have succeeded.

use std::sync::mpsc;
use std::thread;

use super::disk_check::SPACE_BUFFER_MULTIPLIER;

/// How a batch's downloads and installs are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// Install package `n` while package `n + 1` downloads.
    Overlapped,
    /// Install each package before the next one starts downloading.
    Sequential,
}

/// Peak temporary disk usage of a batch whose packages download `sizes`
/// bytes each, run under `schedule`.
///
/// Installing a package peaks at [`SPACE_BUFFER_MULTIPLIER`] times its
/// download (see that constant for the measurements). Overlapped, the
/// next package's parts arrive during that peak.
pub fn peak_bytes(sizes: &[u64], schedule: Schedule) -> u64 {
    let install_peak = |size: u64| size.saturating_mul(SPACE_BUFFER_MULTIPLIER);
    sizes
        .iter()
        .enumerate()
        .map(|(i, &size)| match (schedule, sizes.get(i + 1)) {
            (Schedule::Overlapped, Some(&next)) => install_peak(size).saturating_add(next),
            _ => install_peak(size),
        })
        .max()
        .unwrap_or(0)
}

/// Choose the fastest schedule whose peak fits in `available` bytes.
///
/// Returns the bytes required when not even [`Schedule::Sequential`] fits.
pub fn schedule_for(sizes: &[u64], available: u64) -> Result<Schedule, u64> {
    [Schedule::Overlapped, Schedule::Sequential]
        .into_iter()
        .find(|&schedule| peak_bytes(sizes, schedule) <= available)
        .ok_or_else(|| peak_bytes(sizes, Schedule::Sequential))
}

/// Download then install every item, returning one result per item in item
/// order.
///
/// `download` runs on the calling thread. Overlapped, `install` runs on a
/// single worker thread, so packages never install in parallel: they would
/// only compete for the same disk. A failure in either stage is that item's
/// result alone; every other item still downloads and installs.
pub fn run<T, D, R, E>(
    items: Vec<T>,
    schedule: Schedule,
    download: impl FnMut(T) -> Result<D, E>,
    install: impl FnMut(D) -> Result<R, E> + Send,
) -> Vec<Result<R, E>>
where
    D: Send,
    R: Send,
    E: Send,
{
    match schedule {
        Schedule::Sequential => run_sequential(items, download, install),
        Schedule::Overlapped => run_overlapped(items, download, install),
    }
}

fn run_sequential<T, D, R, E>(
    items: Vec<T>,
    mut download: impl FnMut(T) -> Result<D, E>,
    mut install: impl FnMut(D) -> Result<R, E>,
) -> Vec<Result<R, E>> {
    items
        .into_iter()
        .map(|item| download(item).and_then(&mut install))
        .collect()
}

fn run_overlapped<T, D, R, E>(
    items: Vec<T>,
    mut download: impl FnMut(T) -> Result<D, E>,
    mut install: impl FnMut(D) -> Result<R, E> + Send,
) -> Vec<Result<R, E>>
where
    D: Send,
    R: Send,
    E: Send,
{
    let mut results: Vec<Option<Result<R, E>>> = items.iter().map(|_| None).collect();

    thread::scope(|scope| {
        // Capacity 0 makes every hand-off a rendezvous: package n + 1 is
        // only handed over once package n's install has finished, so the
        // next download cannot start while two packages are on disk.
        let (handoff, downloaded) = mpsc::sync_channel::<(usize, D)>(0);
        let installer = scope.spawn(move || {
            downloaded
                .into_iter()
                .map(|(index, package)| (index, install(package)))
                .collect::<Vec<_>>()
        });

        for (index, item) in items.into_iter().enumerate() {
            match download(item) {
                Ok(package) => {
                    if handoff.send((index, package)).is_err() {
                        break; // The installer panicked; join reports it.
                    }
                }
                Err(e) => results[index] = Some(Err(e)),
            }
        }
        drop(handoff);

        match installer.join() {
            Ok(installed) => {
                for (index, result) in installed {
                    results[index] = Some(result);
                }
            }
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });

    results
        .into_iter()
        .map(|r| r.expect("every item was downloaded or failed"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    const GB: u64 = 1_000_000_000;

    // ------------------------------------------------------------------------
    // peak_bytes and schedule_for
    // ------------------------------------------------------------------------

    #[test]
    fn an_empty_batch_needs_no_space() {
        assert_eq!(peak_bytes(&[], Schedule::Overlapped), 0);
        assert_eq!(peak_bytes(&[], Schedule::Sequential), 0);
    }

    #[test]
    fn a_single_package_needs_what_one_install_always_needed() {
        assert_eq!(peak_bytes(&[70 * GB], Schedule::Overlapped), 280 * GB);
        assert_eq!(peak_bytes(&[70 * GB], Schedule::Sequential), 280 * GB);
    }

    #[test]
    fn overlapping_adds_the_next_download_to_each_install_peak() {
        assert_eq!(
            peak_bytes(&[70 * GB, 70 * GB], Schedule::Overlapped),
            350 * GB
        );
        assert_eq!(
            peak_bytes(&[100 * GB, 10 * GB, 50 * GB], Schedule::Overlapped),
            410 * GB
        );
    }

    #[test]
    fn the_last_install_can_be_the_peak() {
        // 4 * 10 + 100 = 140, but installing the 100 GB package alone is 400.
        assert_eq!(
            peak_bytes(&[10 * GB, 100 * GB], Schedule::Overlapped),
            400 * GB
        );
    }

    #[test]
    fn sequentially_the_peak_is_the_largest_single_install() {
        assert_eq!(
            peak_bytes(&[100 * GB, 10 * GB, 50 * GB], Schedule::Sequential),
            400 * GB
        );
    }

    #[test]
    fn a_batch_overlaps_when_the_overlapped_peak_fits() {
        assert_eq!(
            schedule_for(&[70 * GB, 70 * GB], 360 * GB),
            Ok(Schedule::Overlapped)
        );
        assert_eq!(
            schedule_for(&[70 * GB, 70 * GB], 350 * GB),
            Ok(Schedule::Overlapped)
        );
    }

    #[test]
    fn a_batch_falls_back_to_sequential_when_only_one_install_fits() {
        assert_eq!(
            schedule_for(&[70 * GB, 70 * GB], 300 * GB),
            Ok(Schedule::Sequential)
        );
    }

    #[test]
    fn a_batch_is_refused_only_when_one_install_does_not_fit() {
        assert_eq!(schedule_for(&[70 * GB, 70 * GB], 250 * GB), Err(280 * GB));
    }

    // ------------------------------------------------------------------------
    // run
    // ------------------------------------------------------------------------

    /// Ordered record of stage boundaries: "d1+" is download 1 starting,
    /// "i0-" is install 0 finishing.
    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    impl Log {
        fn push(&self, event: String) {
            self.0.lock().unwrap().push(event);
        }

        fn position(&self, event: &str) -> usize {
            let events = self.0.lock().unwrap();
            events
                .iter()
                .position(|e| e == event)
                .unwrap_or_else(|| panic!("{event} never happened: {events:?}"))
        }
    }

    /// Run `n` items whose installs take a moment, logging every boundary.
    fn logged_run(n: usize, schedule: Schedule) -> Log {
        let log = Log::default();
        let results = run(
            (0..n).collect(),
            schedule,
            |i: usize| {
                log.push(format!("d{i}+"));
                log.push(format!("d{i}-"));
                Ok::<_, String>(i)
            },
            |i| {
                log.push(format!("i{i}+"));
                thread::sleep(Duration::from_millis(20));
                log.push(format!("i{i}-"));
                Ok(i)
            },
        );
        assert_eq!(results, (0..n).map(Ok).collect::<Vec<_>>());
        log
    }

    #[test]
    fn results_come_back_in_item_order() {
        for schedule in [Schedule::Overlapped, Schedule::Sequential] {
            let results = run(
                vec!["a", "b", "c"],
                schedule,
                |s| Ok::<_, ()>(s.to_uppercase()),
                Ok,
            );
            assert_eq!(
                results,
                [
                    Ok("A".to_string()),
                    Ok("B".to_string()),
                    Ok("C".to_string())
                ],
                "{schedule:?}"
            );
        }
    }

    #[test]
    fn overlapped_installs_while_the_next_package_downloads() {
        // Install 0 waits for download 1 to start. Without overlap, download
        // 1 would wait for install 0 instead, and this times out.
        let (started, download_1_started) = mpsc::channel();
        let results = run(
            vec![0, 1],
            Schedule::Overlapped,
            |i: i32| {
                if i == 1 {
                    started.send(()).unwrap();
                }
                Ok::<_, &str>(i)
            },
            move |i| {
                if i == 0 {
                    download_1_started
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|_| "download 1 never overlapped install 0")?;
                }
                Ok(i)
            },
        );
        assert_eq!(results, [Ok(0), Ok(1)]);
    }

    #[test]
    fn overlapped_never_downloads_more_than_one_package_ahead() {
        // Package k's parts may only arrive once package k - 2 has left the
        // disk, or three packages would be on disk at once.
        let log = logged_run(5, Schedule::Overlapped);
        for k in 2..5 {
            assert!(
                log.position(&format!("d{k}+")) > log.position(&format!("i{}-", k - 2)),
                "download {k} started before install {} finished",
                k - 2
            );
        }
    }

    #[test]
    fn sequential_installs_each_package_before_the_next_download() {
        let log = logged_run(4, Schedule::Sequential);
        for k in 1..4 {
            assert!(
                log.position(&format!("d{k}+")) > log.position(&format!("i{}-", k - 1)),
                "download {k} started before install {} finished",
                k - 1
            );
        }
    }

    #[test]
    fn a_failed_download_skips_only_that_install() {
        for schedule in [Schedule::Overlapped, Schedule::Sequential] {
            let installed = Mutex::new(Vec::new());
            let results = run(
                vec![0, 1, 2],
                schedule,
                |i: i32| if i == 1 { Err("lost") } else { Ok(i) },
                |i| {
                    installed.lock().unwrap().push(i);
                    Ok(i)
                },
            );
            assert_eq!(results, [Ok(0), Err("lost"), Ok(2)], "{schedule:?}");
            assert_eq!(*installed.lock().unwrap(), [0, 2], "{schedule:?}");
        }
    }

    #[test]
    fn a_failed_install_does_not_stop_later_packages() {
        for schedule in [Schedule::Overlapped, Schedule::Sequential] {
            let results = run(
                vec![0, 1, 2],
                schedule,
                |i: i32| Ok(i),
                |i| if i == 0 { Err("disk full") } else { Ok(i) },
            );
            assert_eq!(results, [Err("disk full"), Ok(1), Ok(2)], "{schedule:?}");
        }
    }

    #[test]
    #[should_panic(expected = "install exploded")]
    fn a_panicking_install_reaches_the_caller_instead_of_hanging() {
        run(
            vec![0, 1, 2],
            Schedule::Overlapped,
            |i: i32| Ok::<_, ()>(i),
            |i| -> Result<i32, ()> {
                if i == 0 {
                    panic!("install exploded");
                }
                Ok(i)
            },
        );
    }
}
