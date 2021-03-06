use anyhow::{anyhow, Result};
use clap::{ArgEnum, Parser};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use simplelog::{
    ColorChoice, Config, ConfigBuilder, LevelFilter, TermLogger, TerminalMode, WriteLogger,
};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

use crate::filename_scanner::FilenameScanner;
use crate::filescanner::*;
use crate::hash_scanner::HashScanner;
use crate::levenshtein_scanner::LevenshteinScanner;
use crate::scanner_result::{ScannerResult, ScannerFinding};
use crate::yara::YaraScanner;

/// this needs to be a global variable,
/// because it is read by serialization code, which has no state by default
static mut DISPLAY_STRINGS: bool = false;

pub(crate) fn display_strings() -> bool {
    unsafe { DISPLAY_STRINGS }
}

pub(crate) fn skip_display_strings() -> bool {
    !display_strings()
}

fn set_display_strings(val: bool) {
    unsafe { DISPLAY_STRINGS = val }
}

#[derive(ArgEnum, Clone)]
pub(crate) enum OutputFormat {
    Csv,
    Txt,
    Json,
}

impl OutputFormat {
    pub fn into_options<W: Write>(self, destination: W) -> OutputMethods<W> {
        let destination = match self {
            OutputFormat::Csv => OutputDestination::Csv(csv::Writer::from_writer(destination)),
            OutputFormat::Txt => OutputDestination::Txt(destination),
            OutputFormat::Json => OutputDestination::Json(destination),
        };
        let print_strings = false;
        OutputMethods {
            destination,
            print_strings,
        }
    }
}

pub(crate) struct OutputMethods<W: Write> {
    destination: OutputDestination<W>,
    print_strings: bool,
}

pub(crate) enum OutputDestination<W: Write> {
    Csv(csv::Writer<W>),
    Txt(W),
    Json(W),
}

impl<W> OutputMethods<W>
where
    W: Write,
{
    pub fn with_print_strings(mut self, print_strings: bool) -> Self {
        self.print_strings = print_strings;
        self
    }

    pub fn print_strings(&self) -> bool {
        self.print_strings
    }

    pub fn destination(&self) -> &OutputDestination<W> {
        &self.destination
    }

    pub fn print_result(&mut self, result: &ScannerResult) {
        for finding in result.findings() {
            match self.destination {
                OutputDestination::Csv(ref mut wtr) => {
                    let _ = finding.format_csv().into_iter().map(|csv| wtr.serialize(csv));
                },
                OutputDestination::Txt(ref mut wtr) => {
                    let _ = write!(wtr, "{}", finding);
                },
                OutputDestination::Json(ref mut wtr) => {
                    let _ = serde_json::to_writer(wtr, &finding.to_json());
                }
            }
        }
    }
}

#[derive(Parser, Clone)]
#[clap(author, version, about, long_about = None)]
pub(crate) struct Cli {
    #[clap(flatten)]
    verbose: clap_verbosity_flag::Verbosity,

    /// path which must be scanned
    #[clap(short('P'), long("path"), display_order(10))]
    path: Option<String>,

    /// output format
    #[clap(short('f'),long("format"), arg_enum, default_value_t=OutputFormat::Txt, display_order(20))]
    pub(crate) output_format: OutputFormat,

    /// use yara scanner with the specified ruleset. This can be a
    /// single file, a zip file or a directory containing lots of
    /// yara files. Yara files must end with 'yar' or 'yara', and zip
    /// files must end with 'zip'
    #[clap(short('Y'), long("yara"), display_order(100))]
    yara: Option<String>,

    /// timeout for the yara scanner, in seconds
    #[clap(long("yara-timeout"), default_value_t = 240, display_order(110))]
    yara_timeout: u16,

    /// print matching strings (only used by yara currently)
    #[clap(short('s'), long("print-strings"), display_order(120))]
    pub(crate) print_strings: bool,

    /// also do YARA scan in Windows EVTX records (exported as JSON)
    #[clap(long("evtx"), display_order(130))]
    #[cfg(feature = "scan_evtx")]
    pub(crate) yara_scan_evtx: bool,

    /// also do YARA scan in Windows registry hive files
    #[clap(long("reg"), display_order(130))]
    #[cfg(feature = "scan_reg")]
    pub(crate) yara_scan_reg: bool,

    /// allow yara to scan compressed files. Currently, xz, bz2 and gz are supported
    #[clap(short('C'), long("scan-compressed"), display_order(140))]
    scan_compressed: bool,

    /// maximum size (in MiB) of decompression buffer (per thread), which is used to scan compressed files
    #[clap(
        long("decompression-buffer"),
        default_value_t = 128,
        display_order(150)
    )]
    decompression_buffer_size: usize,

    /// Hash of file to match against. Use any of MD5, SHA1 or SHA256.
    /// This parameter can be specified multiple times
    #[clap(short('H'), long("file-hash"), display_order(200))]
    file_hash: Vec<String>,

    /// regular expression to match against the basename of files.
    /// This parameter can be specified multiple times
    #[clap(short('F'), long("filename"), display_order(210))]
    filenames: Vec<String>,

    /// run the Levenshtein scanner
    #[clap(long("levenshtein"), display_order(220))]
    levenshtein: bool,

    /// use the specified NUMBER of threads
    #[clap(short('p'), long("threads"), default_value_t = num_cpus::get(), display_order(300))]
    threads: usize,

    /// display a progress bar (requires counting the number of files to be scanned before a progress bar can be displayed)
    #[clap(long("progress"), display_order(310))]
    pub(crate) display_progress: bool,

    /// path of the file to write error logs to. Error logs will always be appended
    /// Be aware that this are not the results (e.g. matching yara rules) of this program.
    #[clap(short('L'), long("log-file"), display_order(520))]
    log_file: Option<String>,
}

pub struct Dionysos {
    path: PathBuf,
    loglevel: LevelFilter,
    yara_rules: Option<PathBuf>,
    filenames: Vec<regex::Regex>,
    cli: Cli,
}

fn handle_file(
    scanners: &Arc<Vec<Box<dyn FileScanner>>>,
    entry: &walkdir::DirEntry,
) -> ScannerResult {
    let mut result = ScannerResult::from(entry.path());
    for scanner in scanners.iter() {
        log::trace!(
            "starting {} on {}",
            scanner,
            entry.file_name().to_string_lossy()
        );
        let begin = Instant::now();

        for res in scanner.scan_file(entry).into_iter() {
            match res {
                Err(why) => {
                    log::error!("{}", why);
                }

                Ok(res) => {
                    log::trace!(
                        "new finding from {} for {}",
                        scanner,
                        entry.path().display()
                    );
                    result.add_finding(res);
                }
            }
        }

        log::trace!(
            "finished {} on {} in {}s",
            scanner,
            entry.file_name().to_string_lossy(),
            Instant::now().duration_since(begin).as_secs_f64()
        );
    }
    result
}

fn worker(
    rx: spmc::Receiver<walkdir::DirEntry>,
    tx: mpsc::Sender<ScannerResult>,
    scanners: Arc<Vec<Box<dyn FileScanner>>>,
    mystatus: Option<ProgressBar>,
    progress: Option<Arc<ProgressBar>>,
) {
    let rx_ref = &rx;
    let tx_ref = &tx;
    loop {
        match rx_ref.try_recv() {
            Ok(entry) => {
                if let Some(s) = &mystatus {
                    s.set_message(entry.file_name().to_string_lossy().to_string());
                }
                if let Some(p) = &progress {
                    p.inc(1);
                }

                let result = handle_file(&scanners, &entry);

                if let Err(why) = tx_ref.send(result) {
                    log::error!(
                        "error while sending a scanner result from the worker: {}",
                        why
                    );
                    if let Some(s) = mystatus {
                        s.finish_and_clear();
                    }
                    drop(rx);
                    drop(tx);
                    return;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                if let Some(s) = &mystatus {
                    s.finish_and_clear();
                }
                drop(rx);
                drop(tx);
                return;
            }
        }
    }
}

impl Dionysos {
    pub fn new() -> Result<Self> {
        Self::parse_options()
    }

    pub fn run(&self) -> Result<()> {
        self.init_logging()?;

        log::info!("running dionysos version {}", env!("CARGO_PKG_VERSION"));

        let scanners = self.init_scanners()?;
        let (m_progress, progress) = self.create_progress()?;

        let spinner_style =
            ProgressStyle::with_template("{prefix:.bold.dim} {spinner} {wide_msg}")?;

        let max_workers = self.cli.threads;
        let mut workers = Vec::new();

        let (mut tx_in, rx_in) = spmc::channel();
        let (tx_out, rx_out) = mpsc::channel();
        for _id in 0..max_workers {
            log::trace!("creating worker #{}", _id);
            let pb = match &m_progress {
                None => None,
                Some(m_progress) => {
                    let pb = m_progress.add(ProgressBar::new_spinner());
                    pb.set_style(spinner_style.clone());
                    Some(pb)
                }
            };

            let scanner = Arc::clone(&scanners);
            let rx = rx_in.clone();
            let tx = tx_out.clone();
            let global_progress = progress.as_ref().map(Arc::clone);
            let worker = thread::spawn(move || worker(rx, tx, scanner, pb, global_progress));
            workers.push(worker);
        }
        drop(tx_out);

        let cli = self.cli.clone();
        let writer_thread = thread::spawn(move || {
            let mut output_options = cli.output_format.into_options(std::io::stdout());
            loop {
                match rx_out.recv() {
                    Err(mpsc::RecvError) => {
                        drop(rx_out);
                        break;
                    }
                    Ok(result) => {
                        if result.has_findings() {
                            output_options.print_result(&result);
                        }
                    }
                }
            }
        });

        for entry in WalkDir::new(&self.path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            log::info!("scanning '{}'", entry.path().display());

            tx_in.send(entry)?;
        }
        drop(tx_in);

        let _ = workers.into_iter().map(|w| w.join());
        let _ = writer_thread.join();

        if let Some(mp) = m_progress {
            mp.clear()?;
        }

        Ok(())
    }

    fn create_progress(&self) -> Result<(Option<MultiProgress>, Option<Arc<ProgressBar>>)> {
        let m_progress = match self.cli.display_progress {
            false => None,
            true => {
                let m_progress = MultiProgress::new();
                Some(m_progress)
            }
        };
        let progress = match &m_progress {
            None => None,
            Some(m_progress) => {
                let progress_style = ProgressStyle::default_bar()
                    .template(
                        "[{elapsed_precise}] {bar:32.cyan/blue} {pos:>9}/{len:9}({percent}%) {msg}",
                    )?
                    .progress_chars("##-");
                let count = WalkDir::new(&self.path).into_iter().count();
                let progress = Arc::new(m_progress.add(ProgressBar::new(count as u64)));
                progress.set_style(progress_style);
                Some(progress)
            }
        };
        Ok((m_progress, progress))
    }

    fn init_scanners(&self) -> Result<Arc<Vec<Box<dyn FileScanner>>>> {
        let mut scanners: Vec<Box<dyn FileScanner>> = Vec::new();

        if let Some(ref yara_rules) = self.yara_rules {
            let yara_scanner = YaraScanner::new(yara_rules)?
                .with_scan_compressed(self.cli.scan_compressed)
                .with_buffer_size(self.cli.decompression_buffer_size)
                .with_timeout(self.cli.yara_timeout);

            #[cfg(feature = "scan_evtx")]
            let yara_scanner = yara_scanner.with_scan_evtx(self.cli.yara_scan_evtx);

            #[cfg(feature = "scan_reg")]
            let yara_scanner = yara_scanner.with_scan_reg(self.cli.yara_scan_reg);

            scanners.push(Box::new(yara_scanner));
        };

        if !self.filenames.is_empty() {
            let filename_scanner = FilenameScanner::new(self.filenames.clone());
            scanners.push(Box::new(filename_scanner));
        }

        if self.cli.levenshtein {
            let levenshtein_scanner = LevenshteinScanner::default();
            scanners.push(Box::new(levenshtein_scanner));
        }

        if !self.cli.file_hash.is_empty() {
            let hash_scanner = HashScanner::default().with_hashes(&self.cli.file_hash)?;
            scanners.push(Box::new(hash_scanner));
        }

        Ok(Arc::new(scanners))
    }

    fn init_logging(&self) -> Result<()> {
        match &self.cli.log_file {
            None => match TermLogger::init(
                self.loglevel,
                Config::default(),
                TerminalMode::Stderr,
                ColorChoice::Auto,
            ) {
                Err(why) => Err(anyhow!(why)),
                _ => Ok(()),
            },
            Some(log_file_name) => {
                let log_file = match OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_file_name)
                {
                    Ok(file) => file,
                    Err(why) => {
                        eprintln!("unable to write to log file: '{}'", log_file_name);
                        return Err(anyhow!(why));
                    }
                };

                let config = ConfigBuilder::default().set_time_format_rfc3339().build();

                match WriteLogger::init(self.loglevel, config, log_file) {
                    Err(why) => Err(anyhow!(why)),
                    _ => Ok(()),
                }
            }
        }
    }

    fn parse_options() -> Result<Self> {
        let cli = Cli::parse();

        let path = match &cli.path {
            Some(path) => PathBuf::from(&path),

            #[cfg(target_os = "windows")]
            None => PathBuf::from("\\"),

            #[cfg(not(target_os = "windows"))]
            None => PathBuf::from("/"),
        };

        let yara_rules = match &cli.yara {
            None => None,
            Some(p) => {
                let yara_rules = PathBuf::from(&p);
                if !yara_rules.exists() {
                    return Err(anyhow!("unable to read yara rules from '{}'", p));
                }
                Some(yara_rules)
            }
        };

        let filenames: Vec<regex::Regex> = cli
            .filenames
            .iter()
            .map(|f| regex::Regex::new(f).unwrap())
            .collect();

        Ok(Self {
            path,
            loglevel: cli.verbose.log_level_filter(),
            yara_rules,
            filenames,
            cli,
        })
    }
}
