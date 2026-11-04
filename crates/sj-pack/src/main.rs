//! `sj-pack`: packs a folder of Source maps into SourceJump viewer files, for staff to upload in bulk.
//! Packing is `viewer_pack::pack`, the code the site's own packer runs, so
//! the API accepts the output unchanged; natively and on several cores it's far faster than in a browser.
//!
//! Each map is written to `packed/{map}.viewer.br` through a `.part` file renamed when done, so a run stopped
//! halfway leaves no half files, and running again skips the maps already packed.

use std::{
    collections::{BTreeMap, VecDeque},
    env, fmt, fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use viewer_pack::{MAX_MAP_BYTES, Stage};

const USAGE: &str = concat!(
    "sj-pack ",
    env!("CARGO_PKG_VERSION"),
    "\n",
    "\
usage: sj-pack [folder] [--jobs N]
  Packs every .bsp and .bsp.bz2 in the folder (default: the current one) into
  packed/<map>.viewer.br, ready to upload on SourceJump's staff Maps page.
  Maps already in packed/ are skipped, so a stopped run picks up where it left off.
  --jobs N   maps packed at once (default: up to 4, fewer on machines with fewer cores)
  --version  the version, to compare with the release you downloaded"
);

/// Each map being packed holds the map, its stripped copy and brotli's buffers: up to a few GB for the largest.
/// Four at once stays within a 16 GB machine.
const DEFAULT_MAX_JOBS: usize = 4;

const OUTPUT_FOLDER: &str = "packed";

struct Options {
    folder: PathBuf,
    jobs: usize,
}

struct Map {
    name: String,
    path: PathBuf,
    bytes: u64,
}

/// One map's result, as a worker reports it.
struct Done {
    name: String,
    bytes: u64,
    outcome: Result<Packed, String>,
}

struct Packed {
    source_bytes: usize,
    stored_bytes: usize,
    took: Duration,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    // Double-clicked in Explorer, the window would close before anyone could read the summary.
    let double_clicked = cfg!(windows) && args.is_empty();
    let code = match parse(&args) {
        Ok(Some(options)) => run(&options),
        Ok(None) => ExitCode::SUCCESS,
        Err(problem) => {
            eprintln!("{problem}\n\n{USAGE}");
            ExitCode::from(2)
        }
    };
    if double_clicked {
        println!("\nPress Enter to close.");
        let _ = io::stdin().read_line(&mut String::new());
    }
    code
}

/// The options, or None when help or the version was asked for and shown.
fn parse(args: &[String]) -> Result<Option<Options>, String> {
    let cores = thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let mut options = Options {
        folder: PathBuf::from("."),
        jobs: cores.min(DEFAULT_MAX_JOBS),
    };
    let mut folder_given = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("sj-pack {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-j" | "--jobs" => {
                options.jobs = args
                    .next()
                    .and_then(|jobs| jobs.parse().ok())
                    .filter(|&jobs| jobs > 0)
                    .ok_or("--jobs needs a number above 0")?;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            folder if !folder_given => {
                options.folder = PathBuf::from(folder);
                folder_given = true;
            }
            extra => return Err(format!("only one folder, please (also got {extra})")),
        }
    }
    Ok(Some(options))
}

fn run(options: &Options) -> ExitCode {
    let output = options.folder.join(OUTPUT_FOLDER);
    let (maps, refused) = match find_maps(&options.folder) {
        Ok(found) => found,
        Err(error) => {
            eprintln!("{} can't be read: {error}", options.folder.display());
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = fs::create_dir_all(&output) {
        eprintln!("{} can't be made: {error}", output.display());
        return ExitCode::FAILURE;
    }
    let (to_pack, already): (Vec<Map>, Vec<Map>) = maps
        .into_values()
        .partition(|map| !packed_path(&output, &map.name).exists());

    print!(
        "{} maps in {}: {} to pack, {} already packed.",
        to_pack.len() + already.len(),
        options.folder.display(),
        to_pack.len(),
        already.len(),
    );
    if to_pack.is_empty() {
        println!();
    } else {
        println!(" Packing {} at once.", options.jobs.min(to_pack.len()));
    }
    for (file, why) in &refused {
        println!("  skipping {file}: {why}");
    }
    let failed = pack_all(to_pack, &output, options.jobs);

    if !failed.is_empty() {
        println!("\n{} maps failed:", failed.len());
        for (name, why) in &failed {
            println!("  {name}: {why}");
        }
    }
    println!(
        "\nDone. Upload the folder {} on SourceJump's staff Maps page.",
        output.display()
    );
    if failed.is_empty() && refused.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The maps in `folder` by name, and the map files that can't be packed with why. A map there as both `.bsp`
/// and `.bsp.bz2` is read from the `.bsp`, which needs no unpacking.
#[expect(
    clippy::type_complexity,
    reason = "a private pair, named where it's used"
)]
fn find_maps(folder: &Path) -> io::Result<(BTreeMap<String, Map>, Vec<(String, String)>)> {
    let mut maps = BTreeMap::new();
    let mut refused = Vec::new();
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        let file = entry.file_name().to_string_lossy().into_owned();
        let lower = file.to_lowercase();
        let (stem, compressed) = if let Some(stem) = lower.strip_suffix(".bsp.bz2") {
            (stem, true)
        } else if let Some(stem) = lower.strip_suffix(".bsp") {
            (stem, false)
        } else {
            continue;
        };
        let bytes = entry.metadata()?.len();
        if !is_map_name(stem) {
            refused.push((file, "not a map name SourceJump accepts".to_owned()));
        // Only a size no map is packed at; whether a map fits SourceJump's limits shows once it's stripped.
        } else if !compressed && bytes > MAX_MAP_BYTES {
            refused.push((file, format!("over {} MB", MAX_MAP_BYTES >> 20)));
        } else if !compressed || !maps.contains_key(stem) {
            let map = Map {
                name: stem.to_owned(),
                path: entry.path(),
                bytes,
            };
            maps.insert(stem.to_owned(), map);
        }
    }
    Ok((maps, refused))
}

/// SourceJump's map name rules (its API's `MapName`), on a lowercased name.
fn is_map_name(name: &str) -> bool {
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.".contains(c);
    !name.is_empty() && name.len() <= 64 && name.chars().all(allowed)
}

fn packed_path(output: &Path, map: &str) -> PathBuf {
    output.join(format!("{map}.viewer.br"))
}

/// Packs the maps on `jobs` threads, showing each one's progress and the run's; returns the maps that failed.
fn pack_all(maps: Vec<Map>, output: &Path, jobs: usize) -> Vec<(String, String)> {
    let count = maps.len();
    if count == 0 {
        return Vec::new();
    }
    let total_bytes: u64 = maps.iter().map(|map| map.bytes).sum();
    let queue = Arc::new(Mutex::new(VecDeque::from(maps)));
    let screen = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
    // Progress goes by bytes, since maps range from 1 to 500 MB: those of the maps done, and the compressed
    // share of those being packed. Time left is the run's average pace so far, not indicatif's recent rate,
    // which falls to nothing while large maps are read and stripped and then shows absurd times.
    let overall = screen.add(
        ProgressBar::new(total_bytes).with_style(
            ProgressStyle::with_template("{bar:30} {msg} · {elapsed} so far, {left}")
                .expect("a valid template")
                .with_key("left", time_left)
                .progress_chars("=> "),
        ),
    );
    overall.set_message(format!("0/{count} maps"));
    overall.enable_steady_tick(Duration::from_secs(1));

    let (report, results) = mpsc::channel();
    let partials: Vec<_> = (0..jobs.min(count))
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    let workers: Vec<_> = partials
        .iter()
        .map(|partial| {
            let (queue, report, output) = (Arc::clone(&queue), report.clone(), output.to_owned());
            let partial = Arc::clone(partial);
            let line = screen.insert_before(
                &overall,
                ProgressBar::new(100).with_style(
                    ProgressStyle::with_template("  {prefix:<32!} {msg}")
                        .expect("a valid template"),
                ),
            );
            thread::spawn(move || work(&queue, &output, &line, &partial, &report))
        })
        .collect();
    drop(report);

    let mut failed = Vec::new();
    let (mut done, mut done_bytes) = (0, 0);
    loop {
        let result = match results.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                let packing: u64 = partials
                    .iter()
                    .map(|partial| partial.load(Ordering::Relaxed))
                    .sum();
                overall.set_position(done_bytes + packing);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        done += 1;
        done_bytes += result.bytes;
        overall.set_position(done_bytes);
        overall.set_message(format!("{done}/{count} maps"));
        let line = match result.outcome {
            Ok(packed) => format!(
                "packed {}: {} to {} in {} s",
                result.name,
                megabytes(packed.source_bytes),
                megabytes(packed.stored_bytes),
                packed.took.as_secs()
            ),
            Err(why) => {
                let line = format!("FAILED {}: {why}", result.name);
                failed.push((result.name, why));
                line
            }
        };
        // Printed above the progress lines, which keep redrawing below; plainly when they're hidden.
        if screen.is_hidden() {
            println!("{line}");
        } else {
            screen.println(line).expect("the terminal is writable");
        }
    }
    for worker in workers {
        worker.join().expect("a packing thread panicked");
    }
    overall.finish_and_clear();
    failed
}

/// Takes maps off the queue until it's empty, keeping `partial` at the bytes of its map counted as done.
fn work(
    queue: &Mutex<VecDeque<Map>>,
    output: &Path,
    line: &ProgressBar,
    partial: &AtomicU64,
    report: &mpsc::Sender<Done>,
) {
    loop {
        let Some(map) = queue.lock().expect("not poisoned").pop_front() else {
            line.finish_and_clear();
            return;
        };
        line.set_prefix(map.name.clone());
        line.set_message("reading");
        let outcome = pack(&map, output, line, partial);
        partial.store(0, Ordering::Relaxed);
        let done = Done {
            name: map.name,
            bytes: map.bytes,
            outcome,
        };
        if report.send(done).is_err() {
            return;
        }
    }
}

fn pack(
    map: &Map,
    output: &Path,
    line: &ProgressBar,
    partial: &AtomicU64,
) -> Result<Packed, String> {
    let started = Instant::now();
    let file = fs::read(&map.path).map_err(|error| format!("can't be read: {error}"))?;
    let packed = viewer_pack::pack(&file, |stage| {
        line.set_message(match stage {
            Stage::Unpacking => "unpacking the .bz2".to_owned(),
            Stage::Stripping => "stripping".to_owned(),
            Stage::Compressing { done, total } => {
                // Compressing is most of a map's time, so its share stands for the map's.
                let (done, total) = (done as u64, total.max(1) as u64);
                partial.store(map.bytes * done / total, Ordering::Relaxed);
                format!("compressing {}%", done * 100 / total)
            }
        });
    })?;
    drop(file);
    let done = packed_path(output, &map.name);
    let part = done.with_extension("br.part");
    fs::write(&part, &packed.file)
        .and_then(|()| fs::rename(&part, &done))
        .map_err(|error| format!("can't be written to {}: {error}", output.display()))?;
    Ok(Packed {
        source_bytes: packed.source_bytes,
        stored_bytes: packed.file.len(),
        took: started.elapsed(),
    })
}

/// The run's time left at its average pace so far; none until a little of it is done, when the pace means little.
fn time_left(state: &ProgressState, out: &mut dyn fmt::Write) {
    let (done, total) = (state.pos(), state.len().unwrap_or(0));
    let elapsed = state.elapsed();
    if done == 0 || done * 100 < total || elapsed < Duration::from_secs(10) {
        let _ = out.write_str("estimating time left");
        return;
    }
    let left_ms = elapsed.as_millis() * u128::from(total - done.min(total)) / u128::from(done);
    let left = Duration::from_millis(u64::try_from(left_ms).unwrap_or(u64::MAX));
    let _ = write!(out, "about {} left", short_duration(left));
}

/// "45s", "12m", "1h 05m".
fn short_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds.div_ceil(60)),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn megabytes(bytes: usize) -> String {
    #[expect(clippy::cast_precision_loss, reason = "shown to one decimal")]
    let megabytes = bytes as f64 / f64::from(1 << 20);
    format!("{megabytes:.1} MB")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{find_maps, is_map_name, parse, short_duration};

    #[test]
    fn durations_are_short() {
        let text = |seconds| short_duration(Duration::from_secs(seconds));
        assert_eq!(
            [text(45), text(61), text(3600), text(3900)],
            ["45s", "2m", "1h 00m", "1h 05m"]
        );
    }

    #[test]
    fn map_names_follow_the_apis_rules() {
        assert!(is_map_name("bhop_eazy_v2"));
        assert!(is_map_name("bhop_a-b.final"));
        assert!(!is_map_name("bhop eazy"));
        assert!(!is_map_name(""));
        assert!(!is_map_name(&"a".repeat(65)));
    }

    #[test]
    fn a_folder_lists_its_maps_once_preferring_the_bsp() {
        let folder = std::env::temp_dir().join(format!("sj-pack-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        for file in [
            "bhop_a.bsp",
            "bhop_a.bsp.bz2",
            "BHOP_B.BSP.BZ2",
            "bhop c.bsp",
            "notes.txt",
        ] {
            std::fs::write(folder.join(file), b"x").unwrap();
        }

        let (maps, refused) = find_maps(&folder).unwrap();
        std::fs::remove_dir_all(&folder).unwrap();

        assert_eq!(maps.keys().collect::<Vec<_>>(), ["bhop_a", "bhop_b"]);
        assert!(maps["bhop_a"].path.ends_with("bhop_a.bsp"));
        assert_eq!(refused.len(), 1, "bhop c.bsp");
    }

    #[test]
    fn options_take_a_folder_and_jobs() {
        let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        let options = parse(&args(&["maps", "--jobs", "2"])).unwrap().unwrap();
        assert_eq!((options.folder.to_str(), options.jobs), (Some("maps"), 2));
        assert!(parse(&args(&["--jobs", "0"])).is_err());
        assert!(parse(&args(&["a", "b"])).is_err());
        assert!(parse(&args(&["--help"])).unwrap().is_none());
    }
}
