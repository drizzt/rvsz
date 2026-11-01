// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! pv-style progress on stderr, drawn only when stderr is a terminal: one
//! row per running job and, for more than one job, a total line with rate
//! and ETA. Other output goes through `line` so it scrolls above the rows.

use crate::sys;
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The total rate is measured over this window, so a run of zips skipped
/// at once does not keep the ETA far too low.
const RATE_WINDOW: Duration = Duration::from_secs(30);

struct Row {
    name: String,
    /// Weight in the total, in the same unit as `State::total`.
    size: u64,
    /// Bytes to process; 0 until the job starts its main pass.
    total: AtomicU64,
    done: Arc<AtomicU64>,
    created: Instant,
    /// Milliseconds after `created` when `total` was set.
    begun_ms: AtomicU64,
}

impl Row {
    fn fraction(&self) -> f64 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 {
            return 0.0;
        }
        (self.done.load(Ordering::Relaxed) as f64 / t as f64).min(1.0)
    }
}

struct State {
    rows: Vec<Arc<Row>>,
    jobs_done: usize,
    jobs_total: usize,
    finished: u64,
    total: u64,
    samples: VecDeque<(Instant, f64)>,
    /// Lines of the display now on screen, below the cursor's line start.
    drawn: usize,
}

pub struct Progress {
    on: bool,
    st: Mutex<State>,
}

/// One job's row; removed from the display when dropped. A no-op when the
/// display is off.
pub struct Bar<'a> {
    p: &'a Progress,
    row: Option<Arc<Row>>,
}

impl Progress {
    /// Runs `f` with a display for `jobs` jobs weighing `total` in all,
    /// redrawn every second, and erases it at the end.
    pub fn run<T>(jobs: usize, total: u64, f: impl FnOnce(&Progress) -> T) -> T {
        let on = io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
            && sys::enable_vt()
            && sys::term_size().is_some();
        let p = Progress {
            on,
            st: Mutex::new(State {
                rows: Vec::new(),
                jobs_done: 0,
                jobs_total: jobs,
                finished: 0,
                total,
                samples: VecDeque::new(),
                drawn: 0,
            }),
        };
        if !on {
            return f(&p);
        }
        let stop = (Mutex::new(false), Condvar::new());
        // stops the ticker even if `f` panics, else the scope never ends
        struct Stop<'a>(&'a (Mutex<bool>, Condvar));
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                self.0.1.notify_all();
            }
        }
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                let mut stopped = stop.0.lock().unwrap();
                loop {
                    stopped = stop
                        .1
                        .wait_timeout(stopped, Duration::from_secs(1))
                        .unwrap()
                        .0;
                    if *stopped {
                        return;
                    }
                    p.redraw(|_| {});
                }
            });
            let _stop = Stop(&stop);
            f(&p)
        });
        let mut st = p.lock();
        let mut err = io::stderr().lock();
        erase(&mut err, &mut st);
        let _ = err.flush();
        r
    }

    pub fn on(&self) -> bool {
        self.on
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds a row for a job weighing `size`.
    pub fn start(&self, name: &str, size: u64) -> Bar<'_> {
        if !self.on {
            return Bar { p: self, row: None };
        }
        let row = Arc::new(Row {
            name: name.to_string(),
            size,
            total: AtomicU64::new(0),
            done: Arc::new(AtomicU64::new(0)),
            created: Instant::now(),
            begun_ms: AtomicU64::new(0),
        });
        self.lock().rows.push(row.clone());
        Bar {
            p: self,
            row: Some(row),
        }
    }

    /// Prints `text` as a line on stderr or stdout, above the display.
    pub fn line(&self, stderr: bool, text: &str) {
        if !self.on {
            if stderr {
                eprintln!("{text}");
            } else {
                println!("{text}");
            }
            return;
        }
        self.redraw(|err| {
            if stderr {
                let _ = writeln!(err, "{text}");
            } else {
                let mut out = io::stdout().lock();
                let _ = writeln!(out, "{text}");
                let _ = out.flush();
            }
        });
    }

    /// Erases the display, runs `between` (which may print), draws it again.
    fn redraw(&self, between: impl FnOnce(&mut io::StderrLock)) {
        let mut st = self.lock();
        let now = Instant::now();
        let done = st.finished as f64
            + st.rows
                .iter()
                .map(|r| r.size as f64 * r.fraction())
                .sum::<f64>();
        st.samples.push_back((now, done));
        while st.samples.len() > 2 && now - st.samples[0].0 > RATE_WINDOW {
            st.samples.pop_front();
        }
        let (cols, rows) = sys::term_size().unwrap_or((80, 24));
        let lines = render(&st, now, cols as usize, rows as usize);
        let mut err = io::stderr().lock();
        erase(&mut err, &mut st);
        between(&mut err);
        for l in &lines {
            let _ = writeln!(err, "{l}");
        }
        st.drawn = lines.len();
        let _ = err.flush();
    }
}

fn erase(err: &mut io::StderrLock, st: &mut State) {
    if st.drawn > 0 {
        let _ = write!(err, "\x1b[{}A\r\x1b[J", st.drawn);
        st.drawn = 0;
    }
}

impl Bar<'_> {
    /// The job's main pass starts: `total` bytes to go.
    pub fn set_total(&self, total: u64) {
        if let Some(r) = &self.row {
            r.begun_ms
                .store(r.created.elapsed().as_millis() as u64, Ordering::Relaxed);
            r.done.store(0, Ordering::Relaxed);
            r.total.store(total, Ordering::Relaxed);
        }
    }

    /// The done counter, for code that tracks progress itself.
    pub fn counter(&self) -> Option<Arc<AtomicU64>> {
        self.row.as_ref().map(|r| Arc::clone(&r.done))
    }

    pub fn add(&self, n: usize) {
        if let Some(r) = &self.row {
            r.done.fetch_add(n as u64, Ordering::Relaxed);
        }
    }

    /// Same as `Progress::line`.
    pub fn line(&self, stderr: bool, text: &str) {
        self.p.line(stderr, text);
    }
}

impl Drop for Bar<'_> {
    fn drop(&mut self) {
        if let Some(row) = self.row.take() {
            let mut st = self.p.lock();
            st.rows.retain(|r| !Arc::ptr_eq(r, &row));
            st.finished += row.size;
            st.jobs_done += 1;
        }
    }
}

fn size(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// `s` cut to `w` chars, with `~` marking the cut.
fn fit(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('~');
    out
}

fn row_line(r: &Row, now: Instant, cols: usize, name_w: usize) -> String {
    let total = r.total.load(Ordering::Relaxed);
    let elapsed = now.saturating_duration_since(r.created);
    let tail = if total == 0 {
        format!("checking {}", clock(elapsed.as_secs()))
    } else {
        let done = r.done.load(Ordering::Relaxed).min(total);
        let t = elapsed.as_secs_f64() - r.begun_ms.load(Ordering::Relaxed) as f64 / 1000.0;
        let rate = if t > 0.5 { done as f64 / t } else { 0.0 };
        format!(
            "{:3.0}% {}/{} {}/s",
            r.fraction() * 100.0,
            size(done as f64),
            size(total as f64),
            size(rate)
        )
    };
    // a line as wide as the terminal would wrap and break the erase
    let room = cols.saturating_sub(1);
    let name_w = name_w.min(room.saturating_sub(tail.len() + 2)).max(8);
    let bar_w = room.saturating_sub(name_w + tail.len() + 6).min(30);
    let bar = if total == 0 || bar_w < 5 {
        String::new()
    } else {
        let full = (r.fraction() * bar_w as f64) as usize;
        let mut b = "=".repeat(full);
        if full < bar_w {
            b.push('>');
            b.push_str(&" ".repeat(bar_w - full - 1));
        }
        format!("[{b}] ")
    };
    let l = format!(" {:<name_w$} {bar}{tail}", fit(&r.name, name_w));
    fit(&l, room)
}

fn render(st: &State, now: Instant, cols: usize, rows: usize) -> Vec<String> {
    let multi = st.jobs_total > 1;
    // keep a line for the cursor and one for the total
    let max = rows.saturating_sub(1 + multi as usize).max(1);
    let name_w = st
        .rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(cols / 2);
    let mut out = Vec::new();
    let shown = if st.rows.len() > max {
        max - 1
    } else {
        st.rows.len()
    };
    for r in &st.rows[..shown] {
        out.push(row_line(r, now, cols, name_w));
    }
    if shown < st.rows.len() {
        out.push(format!(" ... {} more", st.rows.len() - shown));
    }
    if multi {
        let done = st.finished as f64
            + st.rows
                .iter()
                .map(|r| r.size as f64 * r.fraction())
                .sum::<f64>();
        let pct = if st.total > 0 {
            done / st.total as f64 * 100.0
        } else {
            0.0
        };
        let rate = match (st.samples.front(), st.samples.back()) {
            (Some(a), Some(b)) if b.0 - a.0 >= Duration::from_secs(1) => {
                (b.1 - a.1) / (b.0 - a.0).as_secs_f64()
            }
            _ => 0.0,
        };
        let eta = if rate > 0.0 {
            clock(((st.total as f64 - done).max(0.0) / rate) as u64)
        } else {
            "--".to_string()
        };
        let l = format!(
            "total {}/{}  {pct:.1}%  {}/{}  {}/s  ETA {eta}",
            st.jobs_done,
            st.jobs_total,
            size(done),
            size(st.total as f64),
            size(rate)
        );
        out.push(fit(&l, cols.saturating_sub(1)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(names: &[&str], jobs_total: usize) -> State {
        let now = Instant::now();
        State {
            rows: names
                .iter()
                .map(|n| {
                    Arc::new(Row {
                        name: n.to_string(),
                        size: 100,
                        total: AtomicU64::new(1000),
                        done: Arc::new(AtomicU64::new(250)),
                        created: now,
                        begun_ms: AtomicU64::new(0),
                    })
                })
                .collect(),
            jobs_done: 3,
            jobs_total,
            finished: 300,
            total: 1000,
            samples: VecDeque::new(),
            drawn: 0,
        }
    }

    #[test]
    fn rows_and_total() {
        let st = state(&["a.zip", "bb.zip"], 5);
        let l = render(&st, Instant::now(), 80, 24);
        assert_eq!(l.len(), 3);
        assert!(l[0].starts_with(" a.zip    [====="), "{}", l[0]);
        assert!(l[0].contains(" 25% 250 B/1000 B "), "{}", l[0]);
        // 300 finished + 2 * 100 * 0.25
        assert!(
            l[2].starts_with("total 3/5  35.0%  350 B/1000 B"),
            "{}",
            l[2]
        );
        assert!(l.iter().all(|s| s.chars().count() < 80));
    }

    #[test]
    fn one_job_has_no_total() {
        let st = state(&["a.zip"], 1);
        assert_eq!(render(&st, Instant::now(), 80, 24).len(), 1);
    }

    #[test]
    fn rows_capped_to_height() {
        let names: Vec<String> = (0..30).map(|i| format!("{i}.zip")).collect();
        let names: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let st = state(&names, 40);
        let l = render(&st, Instant::now(), 80, 10);
        assert_eq!(l.len(), 9);
        assert_eq!(l[7], " ... 23 more");
    }

    #[test]
    fn long_name_cut() {
        let long = "x".repeat(200);
        let st = state(&[&long], 1);
        let l = &render(&st, Instant::now(), 60, 24)[0];
        assert!(l.chars().count() < 60, "{l}");
        assert!(l.contains("~ "), "{l}");
        assert!(l.contains("25%"), "{l}");
    }

    #[test]
    fn checking_before_total() {
        let st = state(&["a.zip"], 1);
        st.rows[0].total.store(0, Ordering::Relaxed);
        let l = &render(&st, Instant::now(), 80, 24)[0];
        assert!(l.contains("checking 0:00"), "{l}");
        assert!(!l.contains('['), "{l}");
    }
}
