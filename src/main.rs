#![cfg_attr(rustc_nightly, feature(test))]

#[cfg(test)]
extern crate byteorder;
extern crate chrono;
#[macro_use]
extern crate clap;
extern crate ctrlc;
extern crate elf;
extern crate env_logger;
extern crate inferno;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate failure_derive;
extern crate libc;
#[cfg(target_os = "macos")]
extern crate libproc;
#[cfg(unix)]
extern crate proc_maps;
#[macro_use]
extern crate log;
extern crate rand;
#[cfg(test)]
extern crate rbspy_testdata;
extern crate remoteprocess;

extern crate rbspy_ruby_structs as bindings;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate tempdir;
extern crate term_size;
#[cfg(windows)]
extern crate winapi;


use chrono::prelude::*;
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use failure::Error;
use failure::ResultExt;
use rand::{thread_rng, Rng};
use rand::distributions::Alphanumeric;

use std::collections::HashSet;
use std::fs::{DirBuilder, File};
use std::path::{Path, PathBuf};
use std::env;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering, AtomicUsize};
use std::sync::Arc;
use std::time::{Instant, Duration};
#[cfg(unix)]
use std::os::unix::prelude::*;
use std::sync::mpsc::{sync_channel, channel, SyncSender, Receiver};
#[cfg(windows)]
use winapi::um::timeapi;

pub mod core;
pub mod ui;
pub(crate) mod storage;

use crate::core::initialize::initialize;
use crate::core::types::{MemoryCopyError, Pid, Process, StackTrace};
use ui::output;

const BILLION: u64 = 1000 * 1000 * 1000; // for nanosleep

/// The kinds of things we can call `rbspy record` on.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
enum Target {
    Pid { pid: Pid },
    Subprocess { prog: String, args: Vec<String> },
}

// Formats we can write to
arg_enum!{
    // The values of this enum get translated directly to command line arguments. Make them
    // lowercase so that we don't have camelcase command line arguments
    #[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
    #[allow(non_camel_case_types)]
    pub enum OutputFormat {
        flamegraph,
        callgrind,
        speedscope,
        summary,
        summary_by_line,
    }
}

/// Subcommand.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
enum SubCmd {
    /// Record `target`, writing output `output`.
    Record {
        target: Target,
        out_path: PathBuf,
        raw_path: PathBuf,
        sample_rate: u32,
        maybe_duration: Option<std::time::Duration>,
        format: OutputFormat,
        no_drop_root: bool,
        with_subprocesses: bool,
        silent: bool
    },
    /// Capture and print a stacktrace snapshot of process `pid`.
    Snapshot { pid: Pid },
    Report { format: OutputFormat, input: PathBuf, output: PathBuf, },
}
use SubCmd::*;

/// Top level args type.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
struct Args {
    cmd: SubCmd,
}


fn do_main() -> Result<(), Error> {
    env_logger::init();

    let args = Args::from_args()?;

    #[cfg(target_os="macos")]
    {
        let root_cmd = match args.cmd {
            Snapshot{..} => Some("snapshot"),
            Record{..} => Some("record"),
            _ => None,
        };
        if let Some(root_cmd) = root_cmd {
            if !check_root_user() {
                return Err(format_err!("rbspy {} needs to run as root on Mac", root_cmd))
            }
        }
    }

    match args.cmd {
        Snapshot { pid } => {
            #[cfg(all(windows, target_arch = "x86_64"))]
            check_wow64_process(pid);

            snapshot(pid)
        },
        Record {
            target,
            out_path,
            raw_path,
            sample_rate,
            maybe_duration,
            format,
            no_drop_root,
            with_subprocesses,
            silent,
        } => {
            let pid = match target {
                Target::Pid { pid } => pid,
                Target::Subprocess { prog, args } => {
                    if cfg!(target_os = "macos") {
                        // sleep to prevent freezes (because of High Sierra kernel bug)
                        // TODO: figure out how to work around this race in a cleaner way
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }

                    #[cfg(unix)]
                    {
                        // let uid_str = std::env::var("SUDO_UID");
                        // if nix::unistd::Uid::effective().is_root() && !no_drop_root && uid_str.is_ok() {
                        //     let uid: u32 = uid_str.unwrap().parse::<u32>().context(
                        //         "Failed to parse UID",
                        //     )?;
                        //     eprintln!(
                        //         "Dropping permissions: running Ruby command as user {}",
                        //         std::env::var("SUDO_USER")?
                        //     );
                        //     Command::new(prog).uid(uid).args(args).spawn()?.id() as Pid
                        // } else {
                            Command::new(prog).args(args).spawn()?.id() as Pid
                        // }
                    }
                    #[cfg(windows)]
                    { Command::new(prog).args(args).spawn()?.id() as Pid }
                }
            };

            #[cfg(all(windows, target_arch = "x86_64"))]
            check_wow64_process(pid);

            parallel_record(
                format,
                &raw_path,
                &out_path,
                pid,
                with_subprocesses,
                silent,
                sample_rate,
                maybe_duration,
            )
        },
        Report{format, input, output} => report(format, input, output),
    }
}

#[cfg(target_os="macos")]
fn check_root_user() -> bool {
    // let euid = nix::unistd::Uid::effective();
    // if euid.is_root() {
    //     return true;
    // } else {
    //     eprintln!("rbspy only works as root on Mac. Try rerunning with `sudo --preserve-env !!`.");
    //     eprintln!(
    //         "If you run `sudo rbspy record ruby your-program.rb`, rbspy will drop privileges when running `ruby your-program.rb`. If you want the Ruby program to run as root, use `rbspy --no-drop-root`."
    //     );
    //     return false;
    // }
    true
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn check_wow64_process(pid: Pid) {
    if is_wow64_process(pid).unwrap() {
        eprintln!("Unable to profile 32-bit Ruby with 64-bit rbspy.");
        std::process::exit(1);
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn is_wow64_process(pid: Pid) -> Result<bool, Error> {
    use std::os::windows::io::RawHandle;
    use winapi::um::wow64apiset::IsWow64Process;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winnt::PROCESS_QUERY_INFORMATION;
    use winapi::shared::minwindef::{BOOL, FALSE, PBOOL};

    let handle = unsafe {
        OpenProcess(PROCESS_QUERY_INFORMATION, FALSE, pid)
    };

    if handle == (0 as RawHandle) {
        return Err(format_err!(
            "Unable to fetch process handle for process {}", pid
        ));
    }

    let mut is_wow64: BOOL = 0;

    if unsafe { IsWow64Process(handle, &mut is_wow64 as PBOOL) } == FALSE {
        return Err(format_err!(
            "Could not determine process bitness! {}", pid
        ))
    }

    Ok(is_wow64 != 0)
}

#[test]
#[cfg(all(windows, target_arch = "x86_64"))]
fn test_is_wow64_process() {
    let programs = vec![
        "C:\\Program Files (x86)\\Internet Explorer\\iexplore.exe",
        "C:\\Program Files\\Internet Explorer\\iexplore.exe",
    ];

    let results: Vec<bool> = programs.iter().map(|path| {
        let mut cmd = Command::new(path)
            .spawn()
            .expect("ls command failed to start");

        let result = is_wow64_process(cmd.id());

        cmd.kill();

        result.unwrap()
    }).collect();

    assert_eq!(results, vec![true, false]);
}

fn main() {
    if let Err(x) = do_main() {
        eprintln!("Error. Causes: ");
        for c in x.iter_chain() {
            eprintln!("- {}", c);
        }
        eprintln!("{}", x.backtrace());
        std::process::exit(1);
    }
}

fn snapshot(pid: Pid) -> Result<(), Error> {
    let mut getter = initialize(pid)?;
    let trace = getter.get_trace()?;
    for x in trace.iter().rev() {
        println!("{}", x);
    }
    Ok(())
}

impl OutputFormat {
    fn outputter(self) -> Box<dyn ui::output::Outputter> {
        match self {
            OutputFormat::flamegraph => Box::new(output::Flamegraph(ui::flamegraph::Stats::new())),
            OutputFormat::callgrind => Box::new(output::Callgrind(ui::callgrind::Stats::new())),
            OutputFormat::speedscope => Box::new(output::Speedscope(ui::speedscope::Stats::new())),
            OutputFormat::summary => Box::new(output::Summary(ui::summary::Stats::new())),
            OutputFormat::summary_by_line => Box::new(output::SummaryLine(ui::summary::Stats::new())),
        }
    }

    fn extension(&self) -> String {
        match *self {
            OutputFormat::flamegraph => "flamegraph.svg",
            OutputFormat::callgrind => "callgrind.txt",
            OutputFormat::speedscope => "speedscope.json",
            OutputFormat::summary => "summary.txt",
            OutputFormat::summary_by_line => "summary_by_line.txt",
        }.to_string()
    }
}

// This SampleTime struct helps us sample on a regular schedule ("exactly" 100 times per second, if
// the sample rate is 100).
// What we do is -- when doing the 1234th sample, we calculate the exact time the 1234th sample
// should happen at, which is (start time + nanos_between_samples * 1234) and then sleep until that
// time
struct SampleTime {
    start_time: Instant,
    nanos_between_samples: u64,
    num_samples: u64,
}

impl SampleTime {
    pub fn new(rate: u32) -> SampleTime {
        SampleTime{
            start_time: Instant::now(),
            nanos_between_samples: BILLION / u64::from(rate),
            num_samples: 0,
        }
    }

    pub fn get_sleep_time(&mut self) -> Result<u32, u32> {
        // Returns either the amount of time to sleep (Ok(x)) until next sample time or an error of
        // how far we're behind if we're behind the expected next sample time
        self.num_samples += 1;
        let elapsed = self.start_time.elapsed();
        let nanos_elapsed = elapsed.as_secs() * BILLION + u64::from(elapsed.subsec_nanos());
        let target_elapsed = self.num_samples * self.nanos_between_samples;
        if target_elapsed < nanos_elapsed {
            Err((nanos_elapsed - target_elapsed) as u32)
        } else {
            Ok((target_elapsed - nanos_elapsed) as u32)
        }
    }
}

/// Start thread(s) recording a PID and possibly its children. Tracks new processes
/// Returns a pair of Receivers from which you can consume recorded stacktraces and errors
fn spawn_recorder_children(pid: Pid, with_subprocesses: bool, sample_rate: u32, maybe_stop_time: Option<Instant>) -> Result<(Receiver<StackTrace>, Receiver<Result<(), Error>>, Arc<AtomicUsize>, Arc<AtomicUsize>), Error> {
    let done = Arc::new(AtomicBool::new(false));
    let total_traces = Arc::new(AtomicUsize::new(0));
    let timing_error_traces = Arc::new(AtomicUsize::new(0));
    let total_traces_clone = total_traces.clone();
    let timing_error_traces_clone = timing_error_traces.clone();

    // Set up the Ctrl+C handler + the done mutex that we send to each recorder so that it knows
    // when to stop
    let done_clone = done.clone();
    ctrlc::set_handler(move || {
        if done_clone.load(Ordering::Relaxed) {
            eprintln!("Multiple interrupts received, exiting with haste!");
            std::process::exit(1);
        }
        eprintln!("Interrupted.");
        // Trigger the end of the loop
        done_clone.store(true, Ordering::Relaxed);
    }).expect("Error setting Ctrl-C handler");

    eprintln!("Press Ctrl+C to stop");

    // Create the sender/receiver channels and start the child threads off collecting stack traces
    // from each target process.
    // Give the child threads a buffer in case we fall a little behind with aggregating the stack
    // traces, but not an unbounded buffer.
    let (trace_sender, trace_receiver) = sync_channel(100);
    let (error_sender, result_receiver) = channel();

    if with_subprocesses {
        // Start a thread which watches for new descendents and starts new recorders when they
        // appear
        let done_clone = done.clone();
        std::thread::spawn(move || {
            let process = Process::new(pid).unwrap();
            let mut pids: HashSet<Pid> = HashSet::new();
            let done = done.clone();
            // we need to exit this loop when the process we're monitoring exits, otherwise the
            // sender channels won't get closed and rbspy will hang. So we check the done
            // mutex.
            while !done_clone.load(Ordering::Relaxed) {
                let descendents = process.child_processes()
                    .and_then(|tuples| {
                        let mut children: Vec<Pid> = tuples.into_iter()
                            .map(|x| x.0).collect();

                        children.push(pid);

                        Ok(children)
                    }).expect("Error finding descendents of pid");

                for pid in descendents {
                    if pids.contains(&pid) {
                        // already recording it, no need to start a new recording thread
                        continue;
                    }
                    pids.insert(pid);
                    let trace_sender = trace_sender.clone();
                    let error_sender = error_sender.clone();
                    let done = done.clone();
                    let timing_error_traces = timing_error_traces.clone();
                    let total_traces = total_traces.clone();
                    std::thread::spawn(move || {
                        let result = record(
                            pid,
                            sample_rate,
                            maybe_stop_time,
                            done,
                            timing_error_traces,
                            total_traces,
                            trace_sender
                            );
                        error_sender.send(result).expect("couldn't send error");
                        drop(error_sender);
                    });
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    } else {
        // Start a single recorder thread
        let done = done.clone();
        let timing_error_traces = timing_error_traces.clone();
        let total_traces = total_traces.clone();
        std::thread::spawn(move || {
            let result = record(
                pid,
                sample_rate,
                maybe_stop_time,
                done,
                timing_error_traces,
                total_traces,
                trace_sender
                );
            error_sender.send(result).unwrap();
            drop(error_sender);
        });
    }
    Ok((trace_receiver, result_receiver, total_traces_clone, timing_error_traces_clone))
}

#[test]
fn test_spawn_record_children_subprocesses() {
    let which = if cfg!(target_os = "windows") {
        "C:\\Windows\\System32\\WHERE.exe"
    } else {
        "/usr/bin/which"
    };

    let output = Command::new(which)
        .arg("ruby")
        .output()
        .expect("failed to execute process");

    let ruby_binary_path = String::from_utf8(output.stdout).unwrap();

    let ruby_binary_path_str = ruby_binary_path.lines()
        .next()
        .expect("failed to execute ruby process");

    let mut process = std::process::Command::new(ruby_binary_path_str)
        .arg("ci/ruby-programs/ruby_forks.rb")
        .spawn()
        .unwrap();

    let pid = process.id() as Pid;

    let (trace_receiver, result_receiver, _, _) = spawn_recorder_children(pid, true, 10, None).unwrap();

    let results: Vec<_> = result_receiver.iter().take(4).collect();

    // check that there are 4 distinct PIDs in the stack traces
    let pids: HashSet<Pid> = trace_receiver.iter().take(20).map(|x| x.pid.unwrap()).collect();
    for r in results {
        assert!(r.is_ok());
    }

    assert_eq!(pids.len(), 4);
    process.wait().unwrap();
}

fn parallel_record(
    format: OutputFormat,
    raw_path: &PathBuf,
    out_path: &PathBuf,
    pid: Pid,
    with_subprocesses: bool,
    silent: bool,
    sample_rate: u32,
    maybe_duration: Option<std::time::Duration>,
) -> Result<(), Error> {

    let maybe_stop_time = match maybe_duration {
        Some(duration) => Some(std::time::Instant::now() + duration),
        None => None
    };

    let (trace_receiver, result_receiver, total_traces, timing_error_traces) = spawn_recorder_children(pid, with_subprocesses, sample_rate, maybe_stop_time)?;

    // Aggregate stack traces as we receive them from the threads that are collecting them
    // Aggregate to 3 places: the raw output (`.raw.gz`), some summary statistics we display live,
    // and the formatted output (a flamegraph or something)
    let mut out = format.outputter();
    let mut summary_out = ui::summary::Stats::new();
    let mut raw_store = storage::Store::new(raw_path, sample_rate)?;
    let mut summary_time = std::time::Instant::now() + Duration::from_secs(1);
    let start_time = Instant::now();

    for trace in trace_receiver.iter() {
        out.record(&trace)?;
        summary_out.add_function_name(&trace.trace);
        raw_store.write(&trace)?;

        if !silent {
            // Print a summary every second
            if std::time::Instant::now() > summary_time {
                print_summary(&summary_out, &start_time, sample_rate, timing_error_traces.load(Ordering::Relaxed), total_traces.load(Ordering::Relaxed))?;
                summary_time = std::time::Instant::now() + Duration::from_secs(1);
            }
        }
    }

    // Finish writing all data to disk
    eprintln!("Wrote raw data to {}", raw_path.display());
    eprintln!("Writing formatted output to {}", out_path.display());

    let out_file = File::create(&out_path).context(format!( "Failed to create output file {}", &out_path.display()))?;
    out.complete(out_file)?;
    raw_store.complete();

    // Check for errors from the child threads. Ignore errors unless every single thread
    // returned an error. If that happens, return the last error. This lets rbspy successfully
    // record processes even if the parent thread isn't a Ruby process.
    let mut num_ok = 0;
    let mut last_result = Ok(());
    for result in result_receiver.iter() {
        if result.is_ok() {
            num_ok += 1;
        }
        last_result = result;
    }

    match num_ok {
        0 => last_result,
        _ => Ok(()),
    }
}

/// Records stack traces and sends them to a channel in another thread where they can be aggregated
fn record(
    pid: Pid,
    sample_rate: u32,
    maybe_stop_time: Option<Instant>,
    done: Arc<AtomicBool>,
    timing_error_traces: Arc<AtomicUsize>,
    total_traces: Arc<AtomicUsize>,
    sender: SyncSender<StackTrace>
) -> Result<(), Error> {
    let mut getter = core::initialize::initialize(pid)?;

    let mut total = 0;
    let mut errors = 0;

    let mut sample_time = SampleTime::new(sample_rate);
    #[cfg(windows)]
    {
        // This changes a system-wide setting on Windows so that the OS wakes up every 1ms
        // instead of the default 15.6ms. This is required to have a sleep call
        // take less than 15ms, which we need since we usually profile at more than 64hz.
        // The downside is that this will increase power usage: good discussions are:
        // https://randomascii.wordpress.com/2013/07/08/windows-timer-resolution-megawatts-wasted/
        // and http://www.belshe.com/2010/06/04/chrome-cranking-up-the-clock/
        unsafe { timeapi::timeBeginPeriod(1); }
    }

    while !done.load(Ordering::Relaxed) {
        total += 1;
        let trace = getter.get_trace();
        match trace {
            Ok(ok_trace) => {
                sender.send(ok_trace)?;
            }
            Err(x) => {
                if let Some(MemoryCopyError::ProcessEnded) = x.downcast_ref() {
                    // we need to store done = true here to signal the other threads here that we
                    // should stop profiling
                    done.store(true, Ordering::Relaxed);
                    debug!("Process ended");
                    break;
                }

                errors += 1;
                if errors > 20 && (errors as f64) / (total as f64) > 0.5 {
                    print_errors(errors, total);
                    return Err(x);
                }
            }
        }
        if let Some(stop_time) = maybe_stop_time {
            if std::time::Instant::now() > stop_time {
                // need to store done for same reason as above
                done.store(true, Ordering::Relaxed);
                break;
            }
        }
        // Sleep until the next expected sample time
        total_traces.fetch_add(1, Ordering::Relaxed);
        match sample_time.get_sleep_time() {
            Ok(sleep_time) => {std::thread::sleep(std::time::Duration::new(0, sleep_time));},
            Err(_) => { timing_error_traces.fetch_add(1, Ordering::Relaxed); },
        }
    }

   // reset time period calls
    #[cfg(windows)]
    {
        unsafe { timeapi::timeEndPeriod(1); }
    }
    Ok(())
}

fn report(format: OutputFormat, input: PathBuf, output: PathBuf) -> Result<(), Error>{
    let input_file = File::open(input)?;
    let stuff = storage::from_reader(input_file)?.traces;
    let mut outputter = format.outputter();
    for trace in stuff {
        outputter.record(&trace)?;
    }
    outputter.complete(File::create(output)?)?;
    Ok(())
}

fn print_summary(summary_out: &ui::summary::Stats, start_time: &Instant, sample_rate: u32, timing_error_traces: usize, total_traces: usize) -> Result<(), Error> {
    let width = match term_size::dimensions() {
        Some((w, _)) => Some(w as usize),
        None => None,
    };
    println!("{}[2J", 27 as char); // clear screen
    println!("{}[0;0H", 27 as char); // go to 0,0
    eprintln!("Time since start: {}s. Press Ctrl+C to stop.", start_time.elapsed().as_secs());
    let percent_timing_error = (timing_error_traces as f64) / (total_traces as f64) * 100.0;
    eprintln!("Summary of profiling data so far:");
    summary_out.print_top_n(20, width)?;

    if total_traces > 100 && percent_timing_error > 0.5 {
        // Only print if timing errors are more than 0.5% of total traces -- it's a statistical
        // profiler so smaller differences don't really matter
        eprintln!("{:.1}% ({}/{}) of stack traces were sampled late because we couldn't sample at expected rate, results may be inaccurate. Current rate: {}. Try sampling at a lower rate with `--rate`.", percent_timing_error, timing_error_traces, total_traces, sample_rate);
    }
    Ok(())
}

fn print_errors(errors: usize, total: usize) {
    if errors > 0 {
        eprintln!(
            "Dropped {}/{} stack traces because of errors. Please consider reporting a GitHub issue -- this isn't normal.",
            errors,
            total
        );
    }
}

#[test]
fn test_output_filename() {
    let d = tempdir::TempDir::new("temp").unwrap();
    let dirname = d.path().to_str().unwrap();
    assert_eq!(output_filename("", Some("foo"), "txt").unwrap(), Path::new("foo"));
    let generated_filename = output_filename(dirname, None, "txt").unwrap();

    let filename_pattern = if cfg!(target_os = "windows") {
        ".cache\\rbspy\\records\\rbspy-"
    } else {
        ".cache/rbspy/records/rbspy-"
    };

    assert!(
        generated_filename
            .to_string_lossy()
            .contains(filename_pattern)
    );
}

fn output_filename(base_dir: &str, maybe_filename: Option<&str>, extension: &str) -> Result<PathBuf, Error> {
    let mut rng = thread_rng();

    let path = match maybe_filename {
        Some(filename) => filename.into(),
        None => {
            let s = ::std::iter::repeat(()).map(|()| rng.sample(Alphanumeric))
                .take(10)
                .collect::<String>();
            let filename = format!("{}-{}-{}.{}", "rbspy", Utc::now().format("%Y-%m-%d"), s, extension);
            let dirname = Path::new(base_dir).join(".cache").join("rbspy").join("records");
            DirBuilder::new().recursive(true).create(&dirname)?;
            dirname.join(&filename)
        }
    };
    Ok(path)
}

/// Check `s` is a positive integer.
// This assumes a process group isn't a sensible thing to snapshot; could be wrong!
fn validate_pid(s: String) -> Result<(), String> {
    let pid: Pid = s.parse().map_err(|_| "PID must be an integer".to_string())?;
    if pid <= 0 {
        return Err("PID must be positive".to_string());
    }
    Ok(())
}

// Prevent collision for the flamegraph filename
fn validate_filename(s: String) -> Result<(), String> {
    if s.ends_with(".svg") {
        return Err("Filename must not end with .svg".to_string());
    }
    Ok(())
}

fn arg_parser() -> App<'static, 'static> {
    App::new("rbspy")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Sampling profiler for Ruby programs")
        .setting(AppSettings::SubcommandRequired)
        .subcommand(
            SubCommand::with_name("snapshot")
                .about("Snapshot a single stack trace")
                .arg(
                    Arg::from_usage("-p --pid=[PID] 'PID of the Ruby process you want to profile'")
                        .validator(validate_pid)
                        .required(true),
                ),
        )
        .subcommand(
            SubCommand::with_name("record")
                .about("Record process")
                .arg(
                    Arg::from_usage(
                        "-p --pid=[PID] 'PID of the Ruby process you want to profile'")
                    .validator(validate_pid)
                    // It's a bit confusing but this is how to get exactly-one-of behaviour
                    // for `--pid` and `cmd`.
                    .required_unless("cmd")
                    .conflicts_with("cmd"),
                )
                .arg(
                    Arg::from_usage("--raw-file=[FILE] 'File to write raw data to (will be gzipped)'")
                        .validator(validate_filename)
                        .required(false),
                )
                .arg(
                    Arg::from_usage("-f --file=[FILE] 'File to write formatted output to'")
                        .validator(validate_filename)
                        .required(false),
                )
                .arg(
                    Arg::from_usage("-r --rate=[RATE] 'Samples per second collected'")
                        .default_value("100"),
                )
                .arg(
                    Arg::from_usage("--no-drop-root 'Don't drop root privileges when running a Ruby program as a subprocess'")
                        .required(false),
                )
                .arg(
                    Arg::from_usage("--format=[FORMAT] 'Output format to write'")
                        .possible_values(&OutputFormat::variants())
                        .case_insensitive(true)
                        .default_value("flamegraph"),
                )
                .arg(
                    Arg::from_usage(
                        "-d --duration=[DURATION] 'Number of seconds to record for'",
                    ).conflicts_with("cmd")
                        .required(false),
                )
                .arg(
                    Arg::from_usage( "-s --subprocesses='Record all subprocesses of the given PID or command'")
                        .required(false)
                )
                .arg(
                    Arg::from_usage( "--silent='Don't print the summary profiling data every second'")
                        .required(false)
                )
                .arg(Arg::from_usage("<cmd>... 'command to run'").required(false)),
        )
        .subcommand(
            SubCommand::with_name("report")
                .about("Generate visualization from raw data recorded by `rbspy record`")
                .arg(Arg::from_usage("-i --input=<FILE> 'Input raw data to use'"))
                .arg(Arg::from_usage("-o --output=<FILE> 'Output file'"))
                .arg(
                    Arg::from_usage("-f --format=[FORMAT] 'Output format to write'")
                        .possible_values(&OutputFormat::variants())
                        .case_insensitive(true)
                        .default_value("flamegraph"),
                )
        )
}

impl Args {
    /// Converts from clap's matches.
    // TODO(TryFrom): Replace with TryFrom whenever that stabilizes.
    // TODO(maybe): Consider replacing with one of the derive-based arg thingies.
    fn from<'a, I: IntoIterator<Item = String> + 'a>(args: I) -> Result<Args, Error> {
        let matches: ArgMatches<'a> = arg_parser().get_matches_from(args);

        fn get_pid(matches: &ArgMatches) -> Option<Pid> {
            if let Some(pid_str) = matches.value_of("pid") {
                Some(
                    pid_str
                        .parse()
                        .expect("this shouldn't happen because clap validated the arg"),
                )
            } else {
                None
            }
        }

        let cmd = match matches.subcommand() {
            ("snapshot", Some(submatches)) => Snapshot {
                pid: get_pid(submatches)
                    .expect("this shouldn't happen because clap requires a pid"),
            },
            ("record", Some(submatches)) => {
                let format = value_t!(submatches, "format", OutputFormat).unwrap();

                #[cfg(unix)]
                let home = &std::env::var("HOME")?;
                #[cfg(windows)]
                let home = &std::env::var("userprofile")?;

                let raw_path = output_filename(home, submatches.value_of("raw-file"), "raw.gz")?;
                let out_path = output_filename(home, submatches.value_of("file"), &format.extension())?;
                let maybe_duration = match value_t!(submatches, "duration", u64) {
                    Err(_) => None,
                    Ok(integer_duration) => Some(std::time::Duration::from_secs(integer_duration)),
                };

                let no_drop_root = submatches.occurrences_of("no-drop-root") == 1;
                let silent = submatches.is_present("silent");
                let with_subprocesses = submatches.is_present("subprocesses");

                let sample_rate = value_t!(submatches, "rate", u32).unwrap();
                let target = if let Some(pid) = get_pid(submatches) {
                    Target::Pid { pid }
                } else {
                    let mut cmd = submatches.values_of("cmd").expect("shouldn't happen");
                    let prog = cmd.next().expect("nope");
                    let args = cmd;
                    Target::Subprocess {
                        prog: prog.to_string(),
                        args: args.map(String::from).collect(),
                    }
                };
                Record {
                    target,
                    out_path,
                    raw_path,
                    sample_rate,
                    maybe_duration,
                    format,
                    no_drop_root,
                    with_subprocesses,
                    silent,
                }
            }
            ("report", Some(submatches)) => Report {
                format: value_t!(submatches, "format", OutputFormat).unwrap(),
                input: value_t!(submatches, "input", String).unwrap().into(),
                output: value_t!(submatches, "output", String).unwrap().into(),
            },
            _ => panic!("this shouldn't happen, please report the command you ran!"),
        };

        Ok(Args { cmd })
    }

    fn from_args() -> Result<Args, Error> {
        Args::from(env::args())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make_args(args: &str) -> Vec<String> {
        args.split_whitespace().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_arg_parsing() {
        match Args::from(make_args("rbspy record --pid 1234")).unwrap() {
            Args {
                cmd:
                    Record {
                        target: Target::Pid { pid: 1234 },
                        ..
                    },
            } => (),
            x => panic!("Unexpected: {:?}", x),
        };

        // test snapshot
        let args = Args::from(make_args("rbspy snapshot --pid 1234")).unwrap();
        assert_eq!(
            args,
            Args {
                cmd: Snapshot { pid: 1234 },
            }
        );

        // test record with subcommand
        match Args::from(make_args("rbspy record ruby blah.rb")).unwrap() {
            Args {
                cmd:
                    Record {
                        target: Target::Subprocess { prog, args },
                        ..
                    },
            } => {
                assert_eq!(prog, "ruby");
                assert_eq!(args, vec!["blah.rb".to_string()]);
            }
            x => panic!("Unexpected: {:?}", x),
        };

        let args = Args::from(make_args("rbspy record --pid 1234 --file foo.txt --raw-file raw.gz")).unwrap();
        assert_eq!(
            args,
            Args {
                cmd: Record {
                    target: Target::Pid { pid: 1234 },
                    out_path: "foo.txt".into(),
                    raw_path: "raw.gz".into(),
                    sample_rate: 100,
                    maybe_duration: None,
                    format: OutputFormat::flamegraph,
                    no_drop_root: false,
                    with_subprocesses: false,
                    silent: false,
                },
            }
        );

        let args = Args::from(make_args(
            "rbspy record --pid 1234 --file foo.txt --raw-file raw.gz --rate 25",
        )).unwrap();
        assert_eq!(
            args,
            Args {
                cmd: Record {
                    target: Target::Pid { pid: 1234 },
                    out_path: "foo.txt".into(),
                    raw_path: "raw.gz".into(),
                    sample_rate: 25,
                    maybe_duration: None,
                    format: OutputFormat::flamegraph,
                    no_drop_root: false,
                    with_subprocesses: false,
                    silent: false,
                },
            }
        );

        let args = Args::from(make_args(
            "rbspy record --pid 1234 --file foo.txt --raw-file raw.gz --duration 60",
        )).unwrap();
        assert_eq!(
            args,
            Args {
                cmd: Record {
                    target: Target::Pid { pid: 1234 },
                    out_path: "foo.txt".into(),
                    raw_path: "raw.gz".into(),
                    sample_rate: 100,
                    maybe_duration: Some(std::time::Duration::from_secs(60)),
                    format: OutputFormat::flamegraph,
                    no_drop_root: false,
                    with_subprocesses: false,
                    silent: false,
                },
            }
        );

        let args = Args::from(make_args(
            "rbspy record --pid 1234 --raw-file raw.gz --file foo.txt --format callgrind --duration 60",
        )).unwrap();
        assert_eq!(
            args,
            Args {
                cmd: Record {
                    target: Target::Pid { pid: 1234 },
                    out_path: "foo.txt".into(),
                    raw_path: "raw.gz".into(),
                    sample_rate: 100,
                    maybe_duration: Some(std::time::Duration::from_secs(60)),
                    format: OutputFormat::callgrind,
                    no_drop_root: false,
                    with_subprocesses: false,
                    silent: false,
                },
            }
        );

        let args = Args::from(make_args(
            "rbspy record --pid 1234 --raw-file raw.gz --file foo.txt --no-drop-root",
        )).unwrap();
        assert_eq!(
            args,
            Args {
                cmd: Record {
                    target: Target::Pid { pid: 1234 },
                    out_path: "foo.txt".into(),
                    raw_path: "raw.gz".into(),
                    sample_rate: 100,
                    maybe_duration: None,
                    format: OutputFormat::flamegraph,
                    no_drop_root: true,
                    with_subprocesses: false,
                    silent: false,
                },
            }
        );

        let args = Args::from(make_args(
            "rbspy record --pid 1234 --raw-file raw.gz --file foo.txt --subprocesses",
        )).unwrap();
        assert_eq!(
            args,
            Args {
                cmd: Record {
                    target: Target::Pid { pid: 1234 },
                    out_path: "foo.txt".into(),
                    raw_path: "raw.gz".into(),
                    sample_rate: 100,
                    maybe_duration: None,
                    format: OutputFormat::flamegraph,
                    no_drop_root: false,
                    with_subprocesses: true,
                    silent: false,
                    },
            }
        );
    }

    #[test]
    fn test_report_arg_parsing() {
        let args = Args::from(make_args(
            "rbspy report --input xyz.raw.gz --output xyz",
        )).unwrap();
        assert_eq!(
            args,
            Args {
                cmd: Report {
                    format: OutputFormat::flamegraph,
                    input: PathBuf::from("xyz.raw.gz"),
                    output: PathBuf::from("xyz"),
                },
            }
        );
    }
}
