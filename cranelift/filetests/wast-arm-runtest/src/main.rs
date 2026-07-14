mod cases;
mod compile;
mod parse;
mod report;
mod run;

use anyhow::Result;
use parse::parse_wast_file;
use report::Report;
use std::path::Path;
use wasmtime_environ::{PtrSize, VMOffsets};

fn main() {
    let mut args = std::env::args().skip(1);

    let mut path: Option<String> = None;
    let mut verbose = false;
    let mut stop_on_error = false;

    for arg in args {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--stop-on-error" => stop_on_error = true,
            _ if path.is_none() => path = Some(arg),
            _ => panic!("Unrecognized argument {arg}"),
        }
    }

    let path = match path {
        Some(p) => p,
        None => {
            eprintln!(
                "usage: wast-arm-runtest [--spike] [--c-driver] [--one] [--stop-on-error] <file.wast>"
            );
            std::process::exit(1);
        }
    };

    println!("Processing: {}", path);

    let parsed = parse_wast_file(Path::new(&path), verbose).expect("Failed to parse file");

    if verbose {
        println!(
            "Commands: {}, Modules: {}",
            parsed.command_count, parsed.module_count
        );
    }

    let report = process_all_cases(Path::new(&path), verbose, stop_on_error)
        .expect("Failed to process cases");
    report.print_and_exit();

    // Keep temp dirs alive for a bit
    //std::thread::sleep(std::time::Duration::from_secs(10));
}

fn process_all_cases(path: &Path, verbose: bool, stop_on_error: bool) -> Result<Report> {
    let parsed = parse_wast_file(path, verbose)?;

    let mut report = Report::default();
    let workdir = tempfile::tempdir().expect("Failed to create temp dir");
    let workdir_path = workdir.path().to_path_buf();
    let mut failed = false;

    for (idx, case) in parsed.cases.iter().enumerate() {
        if verbose {
            eprintln!(
                "Processing case {}: {} with args {:?}",
                idx, case.export, case.args
            );
        }

        match run::run_case_single(case, workdir.path()) {
            Ok(outcome) => match outcome {
                run::CaseOutcome::Pass => {
                    report.add_passed(1);
                    if verbose {
                        eprintln!("  PASS");
                    }
                }
                run::CaseOutcome::Fail(reason) => {
                    report.add_failed(1);
                    failed = true;
                    eprintln!(
                        "FAIL {}: {} (args: {:?}, expected: {}) - {}",
                        idx, case.export, case.args, case.expected, reason
                    );
                    if stop_on_error {
                        eprintln!("Stopping on error due to --stop-on-error flag");
                        break;
                    }
                }
                run::CaseOutcome::Skip(reason) => {
                    report.add_skipped(1);
                    if verbose {
                        eprintln!("SKIP {}: {} - {}", idx, case.export, reason);
                    }
                }
            },
            Err(e) => {
                report.add_failed(1);
                failed = true;
                eprintln!("ERROR {}: {} - {}", idx, case.export, e);
                if verbose {
                    eprintln!("  Backtrace: {:?}", e.backtrace());
                }
                if stop_on_error {
                    eprintln!("Stopping on error due to --stop-on-error flag");
                    break;
                }
            }
        }
    }

    // Only cleanup temp dir if no failure or not in stop-on-error mode
    if !failed || !stop_on_error {
        let _ = std::fs::remove_dir_all(&workdir_path);
    } else {
        // Keep the temp dir from being deleted by TempDir's Drop
        workdir.keep();
        eprintln!(
            "Preserving temp directory for debugging: {}",
            workdir_path.display()
        );
    }

    Ok(report)
}
