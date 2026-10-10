//! Command line: `squirreldisk scan <folder>` prints what uses the space,
//! without opening a window. Part of every build, the headless one too.
//! When the app is open and its scans cover the folder, the app answers.

use crate::scan::local;
use crate::tree::{fmt_count, fmt_size, Kind, Tree};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub const USAGE: &str = "\
SquirrelDisk: see what's using your disk space.

Usage:
  squirreldisk                        open the app
  squirreldisk <folder>               open the app and scan <folder>
  squirreldisk scan <folder> [options]
                                      scan <folder> and print the largest items;
                                      the open app answers from its own scan
  squirreldisk scan [options]         print the folder the app window shows
Options for scan:
  -d, --depth <n>   levels of subfolders to show (default 1)
  -n, --top <n>     largest items shown per folder, 0 for all (default 20)
      --json        print JSON instead (the same items, as a tree)

  squirreldisk --version
  squirreldisk --help
";

/// `path` made absolute, links resolved. On Windows without the `\\?\`
/// prefix `canonicalize` adds, which would show up in names and paths.
pub fn absolute(path: &std::path::Path) -> PathBuf {
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        // `\\?\C:\…` becomes `C:\…`; `\\?\UNC\server\share` becomes `\\server\share`
        return match rest.strip_prefix(r"UNC\") {
            Some(unc) => PathBuf::from(format!(r"\\{unc}")),
            None => PathBuf::from(rest),
        };
    }
    p
}

/// True for the arguments this module handles (the rest opens the app).
pub fn handles(args: &[String]) -> bool {
    matches!(args.first().map(|s| s.as_str()), Some("scan" | "help" | "--help" | "-h" | "--version" | "-V"))
}

pub fn main(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("scan") => match Options::parse(&args[1..]) {
            Ok(o) => scan(o),
            Err(e) => {
                eprintln!("squirreldisk: {e}\n\n{USAGE}");
                2
            }
        },
        Some("--version" | "-V") => {
            println!("squirreldisk {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            print!("{USAGE}");
            0
        }
    }
}

/// Also the request to the open app, as JSON.
#[derive(serde::Serialize, serde::Deserialize)]
struct Options {
    /// None: the folder the app window shows.
    path: Option<PathBuf>,
    depth: usize,
    top: usize,
    json: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut o = Options { path: None, depth: 1, top: 20, json: false };
        let mut path = None;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut number = |name: &str| -> Result<usize, String> {
                it.next().and_then(|v| v.parse().ok()).ok_or_else(|| format!("{name} needs a number"))
            };
            match a.as_str() {
                "-d" | "--depth" => o.depth = number(a)?,
                "-n" | "--top" => o.top = number(a)?,
                "--json" => o.json = true,
                s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s}")),
                _ if path.is_some() => return Err("scan takes one folder".into()),
                _ => path = Some(PathBuf::from(a)),
            }
        }
        o.path = path;
        Ok(o)
    }
}

fn scan(mut o: Options) -> i32 {
    if let Some(p) = &o.path {
        if !p.is_dir() {
            eprintln!("squirreldisk: {}: not a folder", p.display());
            return 1;
        }
        o.path = Some(absolute(p));
    }
    #[cfg(unix)]
    if let Some(out) = serde_json::to_string(&o).ok().and_then(|req| crate::ipc::ask(&crate::ipc::socket_path()?, &req)) {
        let _ = std::io::stdout().write_all(out.as_bytes()); // no panic when piped to `head`
        return 0;
    }
    let Some(path) = o.path.clone() else {
        eprintln!("squirreldisk: the app shows no finished scan. Name a folder, e.g. squirreldisk scan .");
        return 2;
    };
    let started = Instant::now();
    let handle = local::start(path.clone(), path.to_string_lossy().into_owned());
    let p = &handle.progress;
    let live = std::io::stderr().is_terminal();
    while !p.is_done() {
        if live {
            eprint!("\r\x1b[2KScanning… {} files, {}", fmt_count(p.files.load(Ordering::Relaxed)), fmt_size(p.bytes.load(Ordering::Relaxed)));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    if live {
        eprint!("\r\x1b[2K");
    }
    let tree = handle.tree.read().unwrap();
    let note = format!("{:.1}s", started.elapsed().as_secs_f32());
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(render(&tree, tree.root, &o, &note).as_bytes());
    let _ = out.flush();
    0
}

/// What `scan` prints for node `id`: the size tree as text, or as JSON.
fn render(tree: &Tree, id: u32, o: &Options, note: &str) -> String {
    let mut out = Vec::new();
    if o.json {
        let _ = serde_json::to_writer_pretty(&mut out, &node_json(tree, id, o, 0));
        let _ = writeln!(out);
    } else {
        let n = tree.get(id);
        let _ = writeln!(out, "{}  {} · {} files · {note}", tree.path(id), fmt_size(n.size), fmt_count(n.files as u64));
        let _ = writeln!(out);
        print_children(&mut out, tree, id, o, 1, "", n.size.max(1));
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The open app's answer to a `scan` request: the deepest finished scan that
/// has the folder, or the folder the window shows when the request names
/// none. Empty when no scan has it, and the command line scans by itself.
#[cfg(all(unix, feature = "gui"))]
pub fn answer(scans: &std::sync::Mutex<crate::ipc::Scans>, request: &str) -> String {
    let Ok(o) = serde_json::from_str::<Options>(request) else { return String::new() };
    let (done, view) = {
        let s = scans.lock().unwrap();
        (s.done.clone(), s.view.clone())
    };
    let found = match &o.path {
        None => view,
        Some(p) => {
            let mut names = vec![p.to_string_lossy().into_owned()];
            // On macOS a startup-disk scan reaches the Data volume through
            // its firmlinks: there /System/Volumes/Data/Users is /Users.
            #[cfg(target_os = "macos")]
            if let Some(rest) = names[0].strip_prefix("/System/Volumes/Data").filter(|r| r.is_empty() || r.starts_with('/')) {
                names.push(if rest.is_empty() { "/".into() } else { rest.into() });
            }
            done.into_iter()
                .filter_map(|t| {
                    let tr = t.read().unwrap();
                    let id = names.iter().map(|n| tr.find_path(n)).find(|&(_, exact)| exact)?.0;
                    let depth = tr.root_path.len();
                    let local = tr.source.is_local();
                    drop(tr);
                    local.then_some((depth, t, id))
                })
                .max_by_key(|&(depth, ..)| depth)
                .map(|(_, t, id)| (t, id))
        }
    };
    let Some((t, id)) = found else { return String::new() };
    let tree = t.read().unwrap();
    if !tree.is_alive(id) {
        return String::new();
    }
    render(&tree, id, &o, "from the open app")
}

/// The children of `id` worth showing: biggest first, at most `top`, and the
/// size of the ones left out.
fn shown(tree: &Tree, id: u32, top: usize) -> (Vec<u32>, usize, u64) {
    let mut kids = tree.sorted_children(id);
    if top == 0 || kids.len() <= top {
        return (kids, 0, 0);
    }
    let rest = kids.split_off(top);
    let rest_size = rest.iter().map(|&c| tree.get(c).size).sum();
    (kids, rest.len(), rest_size)
}

fn label(tree: &Tree, id: u32) -> String {
    let n = tree.get(id);
    if n.kind == Kind::SmallFiles {
        return format!("({} smaller files)", fmt_count(n.files as u64));
    }
    let mut s = n.name.to_string();
    if matches!(n.kind, Kind::Dir | Kind::Mount) {
        s.push('/');
    }
    if n.denied() {
        s.push_str("  (no access)");
    }
    s
}

fn print_children(out: &mut impl Write, tree: &Tree, id: u32, o: &Options, depth: usize, indent: &str, total: u64) {
    let (kids, more, more_size) = shown(tree, id, o.top);
    let rows = kids.len() + (more > 0) as usize;
    for (k, &c) in kids.iter().enumerate() {
        let n = tree.get(c);
        let last = k + 1 == rows;
        let branch = if depth == 1 { "" } else if last { "└─ " } else { "├─ " };
        let pct = n.size as f64 * 100.0 / total as f64;
        let _ = writeln!(out, "{:>9} {:>4.0}%  {indent}{branch}{}", fmt_size(n.size), pct, label(tree, c));
        if depth < o.depth && n.kind == Kind::Dir {
            let next = if depth == 1 { String::new() } else { format!("{indent}{}", if last { "   " } else { "│  " }) };
            print_children(out, tree, c, o, depth + 1, &next, total);
        }
    }
    if more > 0 {
        let branch = if depth == 1 { "" } else { "└─ " };
        let _ = writeln!(out, "{:>9} {:>4.0}%  {indent}{branch}… {more} more", fmt_size(more_size), more_size as f64 * 100.0 / total as f64);
    }
}

#[derive(serde::Serialize)]
struct JsonNode {
    name: String,
    kind: &'static str,
    size: u64,
    files: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    denied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<JsonNode>>,
    /// The children left out by `--top`.
    #[serde(skip_serializing_if = "Option::is_none")]
    more: Option<JsonMore>,
}

#[derive(serde::Serialize)]
struct JsonMore {
    count: usize,
    size: u64,
}

fn node_json(tree: &Tree, id: u32, o: &Options, depth: usize) -> JsonNode {
    let n = tree.get(id);
    let kind = match n.kind {
        Kind::Dir => "dir",
        Kind::File => "file",
        Kind::SmallFiles => "small_files",
        Kind::Mount => "mount",
        Kind::Hidden => "hidden",
        Kind::Link => "link",
    };
    let name = if depth == 0 { tree.path(id) } else { n.name.to_string() };
    let mut v = JsonNode { name, kind, size: n.size, files: n.files, denied: n.denied(), children: None, more: None };
    if depth < o.depth && n.kind == Kind::Dir {
        let (kids, more, more_size) = shown(tree, id, o.top);
        v.children = Some(kids.iter().map(|&c| node_json(tree, c, o, depth + 1)).collect());
        v.more = (more > 0).then_some(JsonMore { count: more, size: more_size });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_scan_options() {
        let o = Options::parse(&args("some/dir -d 3 --top 5 --json")).unwrap();
        assert_eq!((o.path, o.depth, o.top, o.json), (Some(PathBuf::from("some/dir")), 3, 5, true));
        assert!(Options::parse(&args("-d x")).is_err());
        assert!(Options::parse(&args("a b")).is_err());
        assert!(Options::parse(&args("--nope a")).is_err());
        assert!(Options::parse(&[]).unwrap().path.is_none());
        assert!(handles(&args("scan .")) && !handles(&args("/some/folder")) && !handles(&[]));
    }

    #[test]
    fn scans_a_folder() {
        let dir = std::env::temp_dir().join(format!("sqd-cli-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("big")).unwrap();
        std::fs::write(dir.join("big/a.bin"), vec![1u8; 400_000]).unwrap();
        std::fs::write(dir.join("small.bin"), vec![1u8; 50_000]).unwrap();
        let o = Options { path: Some(dir.clone()), depth: 2, top: 20, json: true };
        let handle = local::start(dir.clone(), "x".into());
        while !handle.progress.is_done() {
            std::thread::sleep(Duration::from_millis(10));
        }
        let tree = handle.tree.read().unwrap();
        let json = serde_json::to_value(node_json(&tree, tree.root, &o, 0)).unwrap();
        assert_eq!(json["children"][0]["name"], "big");
        assert_eq!(json["children"][0]["children"][0]["name"], "a.bin");
        let mut text = Vec::new();
        print_children(&mut text, &tree, tree.root, &o, 1, "", tree.get(tree.root).size);
        let text = String::from_utf8(text).unwrap();
        assert!(text.lines().next().unwrap().ends_with("big/"), "{text}");
        assert!(text.contains("└─ a.bin"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
