//! `awsr-difftest`: byte-for-byte differential testing of awsr against
//! the reference AWS CLI.
//!
//! Every case runs both CLIs against a local stub endpoint that answers
//! with a response generated from the operation's output shape (or an
//! error) in the service's protocol, then compares stdout, stderr, the
//! exit code, any streaming output file, and the recorded requests.

mod case;
mod compare;
mod exec;
mod response;
mod rng;
mod run;
mod scenario;
mod s3run;
mod s3scenarios;
mod s3stub;
mod server;
mod table;
mod values;
mod wire;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tokio::sync::Semaphore;

use case::{Case, ErrorMode, InputMode};
use exec::{Config, Outcome};

#[derive(Parser)]
#[command(about = "Differential test harness for awsr against the reference AWS CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one case.
    Case {
        #[command(flatten)]
        common: Common,
        /// `service Operation seed format [input=args] [error=modeled|unmodeled] [pty=COLS] [-- extra]`.
        spec: Vec<String>,
    },
    /// Run every case listed in a file (one case per line, `#` comments).
    List {
        #[command(flatten)]
        common: Common,
        file: PathBuf,
    },
    /// Run generated cases over the whole model corpus.
    Corpus {
        #[command(flatten)]
        common: Common,
        /// Only these services (botocore names).
        #[arg(long = "service")]
        services: Vec<String>,
        /// Only services of these protocols.
        #[arg(long = "protocol")]
        protocols: Vec<String>,
        /// Only operations whose `service.Operation` matches.
        #[arg(long)]
        filter: Option<String>,
        /// Seeds per operation and format.
        #[arg(long, default_value_t = 1)]
        seeds: u64,
        #[arg(long, default_value_t = 0)]
        seed_start: u64,
        /// Output formats, comma separated.
        #[arg(long, default_value = "json")]
        formats: String,
        /// Every K-th seed is an error case (0 disables).
        #[arg(long, default_value_t = 4)]
        error_every: u64,
        /// Stop after this many cases.
        #[arg(long)]
        limit: Option<usize>,
        /// Keep every K-th generated case (a sample across all services).
        #[arg(long, default_value_t = 1)]
        stride: usize,
    },
    /// Run hand-written scenarios (`difftest/scenarios/*.json`) for the
    /// commands and argument customizations the models do not describe.
    Scenario {
        #[command(flatten)]
        common: Common,
        /// Only scenarios whose name matches.
        #[arg(long)]
        filter: Option<String>,
        files: Vec<PathBuf>,
    },
    /// Run the `aws s3` scenarios against the stateful S3 stub.
    S3 {
        #[arg(long, env = "AWSR_REFERENCE_AWS", default_value = "aws")]
        reference: PathBuf,
        #[arg(long, env = "AWSR_BIN")]
        awsr_bin: Option<PathBuf>,
        /// Only scenarios whose name matches.
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, default_value_t = 3)]
        show: usize,
        #[arg(long, default_value_t = default_jobs())]
        jobs: usize,
        /// Run only the reference CLI and print what it did.
        #[arg(long)]
        reference_only: bool,
    },
    /// Print the prepared command line, input, and responses of a case.
    Show {
        #[arg(long)]
        cli_table: Option<PathBuf>,
        spec: Vec<String>,
    },
}

#[derive(Args, Clone)]
struct Common {
    /// Run only the reference CLI and check its exit code.
    #[arg(long)]
    reference_only: bool,
    /// The reference CLI.
    #[arg(long, env = "AWSR_REFERENCE_AWS", default_value = "aws")]
    reference: PathBuf,
    /// The candidate CLI.
    #[arg(long, env = "AWSR_BIN")]
    awsr_bin: Option<PathBuf>,
    #[arg(long, default_value_t = default_jobs())]
    jobs: usize,
    #[arg(long, default_value_t = 1)]
    max_attempts: u32,
    /// Seconds before a CLI run is killed.
    #[arg(long, default_value_t = 60)]
    timeout: u64,
    /// Host in `--endpoint-url`. Host prefixes from the model and from
    /// endpoint rules (`query-`, `{AccountId}.`, S3 virtual hosting) need
    /// a name whose prefixed forms resolve to the stub: every name under
    /// `.localhost` resolves to loopback. With an IP address, operations
    /// with a modeled host prefix still use `difftest.localhost`.
    #[arg(long, default_value = "difftest.localhost")]
    endpoint_host: String,
    /// `service operation reason` lines; `*` matches every operation.
    #[arg(long)]
    exclusions: Option<PathBuf>,
    /// Where failure reports go.
    #[arg(long)]
    report_dir: Option<PathBuf>,
    /// Print the full report of up to this many failures.
    #[arg(long, default_value_t = 3)]
    show: usize,
    /// Print the report of passing cases too.
    #[arg(long)]
    print_passing: bool,
    #[arg(long)]
    cli_table: Option<PathBuf>,
    /// Keep each case's working directory.
    #[arg(long)]
    keep_dirs: bool,
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    match cli.command {
        Command::S3 {
            reference,
            awsr_bin,
            filter,
            show,
            jobs,
            reference_only,
        } => {
            let ok = s3scenarios::run(s3scenarios::Options {
                reference,
                candidate: awsr_bin.unwrap_or_else(|| manifest_dir().join("../target/debug/awsr")),
                filter: filter.map(|f| regex::Regex::new(&f)).transpose()?,
                show,
                report_dir: manifest_dir().join("../target/difftest/s3report"),
                jobs,
                reference_only,
            })
            .await?;
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Show { cli_table, spec } => {
            let table = load_table(cli_table.as_deref())?;
            let case = Case::parse(&spec.join(" "))?;
            let p = case::prepare(&case, &table)?;
            println!("argv: {}", p.argv.join(" "));
            for f in &p.files {
                println!("== {}\n{}", f.name, compare::visible(&f.content));
            }
            println!("expected exit: {}", p.expect_exit);
            print!("{}", exec::responses_text(&p));
            Ok(())
        }
        Command::Scenario {
            common,
            filter,
            files,
        } => run_scenarios(&common, filter.as_deref(), &files).await,
        Command::Case { common, spec } => {
            let case = Case::parse(&spec.join(" "))?;
            run_cases(&common, vec![case]).await
        }
        Command::List { common, file } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            let cases = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(Case::parse)
                .collect::<Result<Vec<_>>>()?;
            run_cases(&common, cases).await
        }
        Command::Corpus {
            common,
            services,
            protocols,
            filter,
            seeds,
            seed_start,
            formats,
            error_every,
            limit,
            stride,
        } => {
            let filter = filter.map(|f| regex::Regex::new(&f)).transpose()?;
            let formats: Vec<String> = formats.split(',').map(str::to_string).collect();
            let mut cases = vec![];
            for service in awsr::model::service_names() {
                if !services.is_empty() && !services.iter().any(|s| s == service) {
                    continue;
                }
                let model = case::model(service)?;
                if !protocols.is_empty() && !protocols.iter().any(|p| p == model.protocol()) {
                    continue;
                }
                for op in model.operation_names() {
                    if let Some(re) = &filter
                        && !re.is_match(&format!("{service}.{op}"))
                    {
                        continue;
                    }
                    let has_errors = model.operation(op).is_some_and(|o| !o.errors().is_empty());
                    for seed in seed_start..seed_start + seeds {
                        for format in &formats {
                            let mut c = Case::new(service, op, seed, format);
                            if seed % 3 == 1 {
                                c.input = InputMode::Args;
                            }
                            if error_every > 0 && seed % error_every == error_every - 1 {
                                c.error = if has_errors && (seed / error_every) % 2 == 0 {
                                    ErrorMode::Modeled
                                } else {
                                    ErrorMode::Unmodeled
                                };
                            }
                            if format == "table" && seed % 2 == 1 {
                                c.pty = Some([80, 120, 200][(seed / 2 % 3) as usize]);
                            }
                            cases.push(c);
                        }
                    }
                }
                if limit.is_some_and(|l| cases.len() >= l * stride.max(1)) {
                    break;
                }
            }
            if stride > 1 {
                cases = cases.into_iter().step_by(stride).collect();
            }
            if let Some(l) = limit {
                cases.truncate(l);
            }
            run_cases(&common, cases).await
        }
    }
}

fn load_table(path: Option<&Path>) -> Result<table::Table> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(table::default_path);
    table::load(&path)
}

struct Exclusions(Vec<(String, String, String)>);

impl Exclusions {
    fn load(path: &Path) -> Result<Exclusions> {
        let mut out = vec![];
        if !path.exists() {
            return Ok(Exclusions(out));
        }
        for line in std::fs::read_to_string(path)?.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(3, char::is_whitespace);
            let (Some(s), Some(o)) = (parts.next(), parts.next()) else {
                bail!("bad exclusion line: {line}");
            };
            let reason = parts.next().unwrap_or("").trim().to_string();
            out.push((s.to_string(), o.to_string(), reason));
        }
        Ok(Exclusions(out))
    }

    fn reason(&self, case: &Case) -> Option<&str> {
        self.0
            .iter()
            .find(|(s, o, _)| {
                (s == "*" || *s == case.service) && (o == "*" || *o == case.operation)
            })
            .map(|(_, _, r)| r.as_str())
    }
}

async fn run_scenarios(common: &Common, filter: Option<&str>, files: &[PathBuf]) -> Result<()> {
    let filter = filter.map(regex::Regex::new).transpose()?;
    let target = manifest_dir().join("../target/difftest");
    std::fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    let report_dir = common
        .report_dir
        .clone()
        .unwrap_or_else(|| target.join("scenario-report"));
    if report_dir.exists() {
        std::fs::remove_dir_all(&report_dir)?;
    }
    std::fs::create_dir_all(&report_dir)?;
    let candidate = Arc::new(
        common
            .awsr_bin
            .clone()
            .unwrap_or_else(|| manifest_dir().join("../target/debug/awsr")),
    );
    let reference = Arc::new(common.reference.clone());
    let env = Arc::new(run::Env {
        max_attempts: common.max_attempts,
        timeout: Duration::from_secs(common.timeout),
    });
    let work = Arc::new(target.join("work"));
    let mut scenarios = vec![];
    for f in files {
        for s in scenario::load(f)? {
            if filter.as_ref().is_none_or(|re| re.is_match(&s.name)) {
                scenarios.push(s);
            }
        }
    }
    let sem = Arc::new(Semaphore::new(common.jobs.max(1)));
    let mut tasks = vec![];
    for s in scenarios {
        let (sem, reference, candidate, env, work) = (
            sem.clone(),
            reference.clone(),
            candidate.clone(),
            env.clone(),
            work.clone(),
        );
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await;
            let r = scenario::exec(&s, &reference, &candidate, &work, &env).await;
            (s.name, r)
        }));
    }
    let (mut passed, mut failed) = (0, 0);
    let mut shown = 0;
    for t in tasks {
        let (name, r) = t.await?;
        let stem: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        match r {
            Ok(o) if o.pass => {
                passed += 1;
                if common.print_passing {
                    println!("PASS {name}\n{}", o.detail);
                }
            }
            Ok(o) => {
                failed += 1;
                println!("FAIL {name}");
                std::fs::write(report_dir.join(format!("{stem}.txt")), &o.detail)?;
                if shown < common.show {
                    shown += 1;
                    println!("{}", o.detail);
                }
            }
            Err(e) => {
                failed += 1;
                println!("FAIL {name}: harness: {e:#}");
            }
        }
    }
    println!(
        "{passed} passed, {failed} failed; reports: {}",
        report_dir.display()
    );
    if failed > 0 {
        bail!("{failed} scenarios failed");
    }
    Ok(())
}

async fn run_cases(common: &Common, cases: Vec<Case>) -> Result<()> {
    let table = Arc::new(load_table(common.cli_table.as_deref())?);
    let exclusions = Exclusions::load(
        &common
            .exclusions
            .clone()
            .unwrap_or_else(|| manifest_dir().join("exclusions.txt")),
    )?;
    let target = manifest_dir().join("../target/difftest");
    std::fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    let report_dir = common
        .report_dir
        .clone()
        .unwrap_or_else(|| target.join("report"));
    if report_dir.exists() {
        std::fs::remove_dir_all(&report_dir)?;
    }
    std::fs::create_dir_all(&report_dir)?;
    let candidate = common
        .awsr_bin
        .clone()
        .unwrap_or_else(|| manifest_dir().join("../target/debug/awsr"));
    if !common.reference_only && !candidate.exists() {
        bail!(
            "candidate CLI {} does not exist (build awsr or pass --awsr-bin)",
            candidate.display()
        );
    }
    let cfg = Arc::new(Config {
        reference: common.reference.clone(),
        candidate,
        reference_only: common.reference_only,
        env: run::Env {
            max_attempts: common.max_attempts,
            timeout: Duration::from_secs(common.timeout),
        },
        endpoint_host: common.endpoint_host.clone(),
        work_dir: target.join("work"),
        keep_dirs: common.keep_dirs,
    });

    let started = Instant::now();
    let total = cases.len();
    let sem = Arc::new(Semaphore::new(common.jobs.max(1)));
    let mut handles = vec![];
    let mut excluded = vec![];
    let done = Arc::new(AtomicUsize::new(0));
    for case in cases {
        if let Some(reason) = exclusions.reason(&case) {
            excluded.push((case, reason.to_string()));
            continue;
        }
        let permit = sem.clone().acquire_owned().await?;
        let (table, cfg, done) = (table.clone(), cfg.clone(), done.clone());
        handles.push(tokio::spawn(async move {
            let outcome = exec::exec(case.clone(), table, cfg).await;
            drop(permit);
            if let Outcome::Fail { cause, .. } = &outcome {
                eprintln!("FAIL {case}: {cause}");
            }
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 500 == 0 {
                eprintln!(
                    "progress: {n}/{total} ({:.0}s)",
                    started.elapsed().as_secs_f64()
                );
            }
            (case, outcome)
        }));
    }

    let mut passed = 0;
    let mut skipped: BTreeMap<String, Vec<Case>> = BTreeMap::new();
    let mut failed: BTreeMap<String, Vec<Case>> = BTreeMap::new();
    let mut shown = 0;
    for h in handles {
        let (case, outcome) = h.await?;
        match outcome {
            Outcome::Pass { detail } => {
                passed += 1;
                if common.print_passing {
                    println!("{detail}");
                }
            }
            Outcome::Skip(reason) => skipped.entry(reason).or_default().push(case),
            Outcome::Fail { cause, detail } => {
                let path = report_dir.join(format!("{}.txt", case.file_stem()));
                std::fs::write(&path, &detail)?;
                if shown < common.show {
                    shown += 1;
                    eprintln!("\n{detail}");
                }
                failed.entry(cause).or_default().push(case);
            }
        }
    }
    for (case, reason) in excluded {
        skipped
            .entry(format!("excluded: {reason}"))
            .or_default()
            .push(case);
    }

    let nfailed: usize = failed.values().map(Vec::len).sum();
    let nskipped: usize = skipped.values().map(Vec::len).sum();
    let mut summary = format!(
        "{total} cases: {passed} passed, {nfailed} failed, {nskipped} skipped ({}; {:.1}s)\n",
        if common.reference_only {
            "reference only"
        } else {
            "reference vs candidate"
        },
        started.elapsed().as_secs_f64()
    );
    let mut groups: Vec<_> = failed.iter().collect();
    groups.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    if !groups.is_empty() {
        summary.push_str("\nfailures by cause:\n");
    }
    for (cause, cases) in &groups {
        let _ = writeln!(summary, "{:6}  {cause}", cases.len());
        for c in cases.iter().take(3) {
            let _ = writeln!(summary, "        {c}");
        }
    }
    let mut groups: Vec<_> = skipped.iter().collect();
    groups.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    if !groups.is_empty() {
        summary.push_str("\nskipped:\n");
    }
    for (reason, cases) in &groups {
        let _ = writeln!(summary, "{:6}  {reason}", cases.len());
    }
    let mut all_failed = String::new();
    for (cause, cases) in &failed {
        for c in cases {
            let _ = writeln!(all_failed, "{c}\t{cause}");
        }
    }
    std::fs::write(report_dir.join("failed.txt"), all_failed)?;
    std::fs::write(report_dir.join("summary.txt"), &summary)?;
    println!("{summary}");
    println!("reports: {}", report_dir.display());
    if nfailed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
