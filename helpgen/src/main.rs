use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use awsr::help::HelpRequest;
use awsr::help::pages::{self, Page};

const LEVEL: i32 = 19;
const DICT_SIZE: usize = 112_640;
const SAMPLE_EVERY: usize = 16;
const SAMPLE_MAX: usize = 128 * 1024;

fn parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else {
                        break;
                    };
                    *out[i].lock().unwrap() = Some(f(item));
                }
            });
        }
    });
    out.into_iter()
        .map(|m| m.into_inner().unwrap().expect("every item ran"))
        .collect()
}

fn main() -> Result<()> {
    let out = std::env::args().nth(1).context("usage: awsr-helpgen OUT")?;
    let version = pages::groff_version().context("groff is not on PATH")?;
    let requests = pages::requests();
    eprintln!("{} pages, {version}", requests.len());

    let sampled: Vec<&HelpRequest> = requests.iter().step_by(SAMPLE_EVERY).collect();
    let samples: Vec<Vec<u8>> = parallel(&sampled, |r| pages::render(r).ok())
        .into_iter()
        .flatten()
        .map(|(stderr, output)| [stderr, output].concat())
        .filter(|p| p.len() <= SAMPLE_MAX)
        .collect();
    let dict = zstd::dict::from_samples(&samples, DICT_SIZE)?;

    let done = AtomicUsize::new(0);
    let results = parallel(&requests, |r| {
        let page = pages::render(r).and_then(|(stderr, output)| {
            let raw = [stderr.as_slice(), &output].concat();
            let data = zstd::bulk::Compressor::with_dictionary(LEVEL, &dict)
                .and_then(|mut c| c.compress(&raw))
                .map_err(|e| e.to_string())?;
            Ok(Page {
                key: pages::key(r),
                data,
                stderr_len: stderr.len(),
                raw_len: raw.len(),
            })
        });
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(1000) {
            eprintln!("{n} / {}", requests.len());
        }
        page.map_err(|e| format!("{}: {e}", pages::key(r)))
    });
    let mut formatted = Vec::new();
    for result in results {
        match result {
            Ok(page) => formatted.push(page),
            Err(e) => eprintln!("skipped {e}"),
        }
    }
    if formatted.is_empty() {
        bail!("no help page was formatted");
    }
    eprintln!("{} of {} pages formatted", formatted.len(), requests.len());
    std::fs::write(&out, pages::encode(&version, &dict, &mut formatted))
        .with_context(|| format!("writing {out}"))?;
    Ok(())
}
