mod scrambler;

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use scrambler::ResourceScrambler;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let mut src: Option<PathBuf> = None;
    let mut dst = PathBuf::from("./scrambled_resources");
    let mut loader = PathBuf::from("./loader.lua");
    let mut timings = false;
    let mut quiet = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dst" => match args.next() {
                Some(v) => dst = PathBuf::from(v),
                None => {
                    eprintln!("error: --dst requires a value");
                    return ExitCode::from(2);
                }
            },
            "--loader" => match args.next() {
                Some(v) => loader = PathBuf::from(v),
                None => {
                    eprintln!("error: --loader requires a value");
                    return ExitCode::from(2);
                }
            },
            "--timings" => timings = true,
            "--quiet" | "-q" => quiet = true,
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown argument: {other}");
                print_usage();
                return ExitCode::from(2);
            }
            other => {
                if src.is_some() {
                    eprintln!("error: extra positional argument: {other}");
                    print_usage();
                    return ExitCode::from(2);
                }
                src = Some(PathBuf::from(other));
            }
        }
    }

    let Some(src) = src else {
        eprintln!("error: missing <resources-dir>\n");
        print_usage();
        return ExitCode::from(2);
    };

    if let Err(e) = run(&src, &dst, &loader, timings, quiet) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn print_usage() {
    eprintln!(
        "resource-scrambler — scramble FiveM resource event names\n\n\
         USAGE:\n  \
           resource-scrambler <resources-dir> [--dst <dir>] [--loader <path>] [--timings] [--quiet]\n\n\
         ARGUMENTS:\n  \
           <resources-dir>  directory containing the resources to scramble (required)\n\n\
         OPTIONS:\n  \
           --dst <dir>     output directory                        (default ./scrambled_resources)\n  \
           --loader <path> Lua manifest sandbox                    (default ./loader.lua)\n  \
           --timings       print per-step durations to stderr\n  \
           --quiet, -q     suppress per-script progress output"
    );
}

fn run(src: &Path, dst: &Path, loader: &Path, timings: bool, quiet: bool) -> Result<(), String> {
    if !src.exists() {
        return Err(format!(
            "source directory {} does not exist",
            src.display()
        ));
    }
    if !loader.exists() {
        return Err(format!("loader at {} not found", loader.display()));
    }

    let mut step = StepTimer::new(timings);

    if !quiet {
        println!("Cloning resources");
    }
    step.start("clone");
    if dst.exists() {
        fs::remove_dir_all(dst).map_err(|e| {
            format!("failed to clear {}: {e}", dst.display())
        })?;
    }
    copy_dir(src, dst)
        .map_err(|e| format!("failed to clone {} → {}: {e}", src.display(), dst.display()))?;
    step.end();

    let mut scrambler = ResourceScrambler::new(loader);

    if !quiet {
        println!("Loading scripts");
    }
    step.start("load_scripts");
    scrambler.load_scripts(dst)?;
    step.end();

    if !quiet {
        println!("Loading events");
    }
    step.start("load_events");
    scrambler.load_system_server_events();
    scrambler.load_system_client_events();
    scrambler.load_custom_server_events();
    scrambler.load_custom_client_events();
    step.end();

    if !quiet {
        println!("Generating new events");
    }
    step.start("generate");
    scrambler.generate_random_matching_events();
    scrambler.generate_matching_system_events();
    step.end();

    if !quiet {
        println!("Writing scrambled resources");
    }
    step.start("write_scripts");
    scrambler
        .write_scripts(|kind, file, i, total| {
            if !quiet {
                println!("{kind} => [{i}/{total}] {}", file.display());
            }
        })
        .map_err(|e| format!("write_scripts: {e}"))?;
    step.end();

    step.start("write_events_table");
    scrambler
        .write_events_table()
        .map_err(|e| format!("write_events_table: {e}"))?;
    step.end();

    step.start("write_cheat_detector");
    scrambler
        .write_cheat_detector()
        .map_err(|e| format!("write_cheat_detector: {e}"))?;
    step.end();

    if !quiet {
        println!("Done.");
    }
    step.summary();
    Ok(())
}

struct StepTimer {
    enabled: bool,
    started_at: Option<(String, Instant)>,
    entries: Vec<(String, std::time::Duration)>,
}

impl StepTimer {
    fn new(enabled: bool) -> Self {
        Self { enabled, started_at: None, entries: Vec::new() }
    }
    fn start(&mut self, label: &str) {
        if self.enabled {
            self.started_at = Some((label.to_owned(), Instant::now()));
        }
    }
    fn end(&mut self) {
        if let Some((label, t0)) = self.started_at.take() {
            self.entries.push((label, t0.elapsed()));
        }
    }
    fn summary(&self) {
        if !self.enabled || self.entries.is_empty() {
            return;
        }
        let total: u128 = self.entries.iter().map(|(_, d)| d.as_micros()).sum();
        eprintln!("--- timings ---");
        for (label, dur) in &self.entries {
            let us = dur.as_micros();
            let pct = if total == 0 { 0.0 } else { 100.0 * us as f64 / total as f64 };
            eprintln!("  {:<22} {:>10.3} ms  ({:>5.1}%)", label, us as f64 / 1000.0, pct);
        }
        eprintln!("  {:<22} {:>10.3} ms", "TOTAL", total as f64 / 1000.0);
    }
}

fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&from, &to)?;
        } else if file_type.is_symlink() {
            // Best-effort: read the link target and recreate, falling back to a copy
            // if symlink creation fails (e.g. on filesystems that disallow it).
            #[cfg(unix)]
            {
                let target = fs::read_link(&from)?;
                if let Err(_) = std::os::unix::fs::symlink(&target, &to) {
                    fs::copy(&from, &to)?;
                }
            }
            #[cfg(not(unix))]
            {
                fs::copy(&from, &to)?;
            }
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
