//! The open app answers `squirreldisk scan` from the scan it already has, so
//! the command line does not walk the disk again (issue #76). One request per
//! connection, on a socket in the config folder: a line of JSON in, the text
//! that `scan` prints out. No answer means: scan the folder yourself.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn socket_path() -> Option<PathBuf> {
    crate::settings::config_dir().map(|d| d.join("app.sock"))
}

/// The open app's answer to `request`, or None when no app answers or its
/// scans do not cover the request.
pub fn ask(socket: &Path, request: &str) -> Option<String> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok()?;
    s.write_all(format!("{request}\n").as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    (!out.is_empty()).then_some(out)
}

/// The scans the window holds, kept current by the app every frame.
#[cfg(feature = "gui")]
#[derive(Default)]
pub struct Scans {
    /// Finished scans.
    pub done: Vec<crate::scan::SharedTree>,
    /// The scan and folder the window shows, once that scan is finished.
    pub view: Option<(crate::scan::SharedTree, u32)>,
}

/// Answers each connection on `socket` with `answer(request)` on a thread.
/// Does nothing when another SquirrelDisk window answers there already.
#[cfg(feature = "gui")]
pub fn serve(socket: PathBuf, answer: impl Fn(&str) -> String + Send + 'static) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader};
    if UnixStream::connect(&socket).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_file(&socket); // left by an app that did not quit cleanly
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    std::thread::Builder::new().name("ipc".into()).spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_ok() {
                let _ = stream.write_all(answer(line.trim_end()).as_bytes());
            }
        }
    })?;
    Ok(())
}

#[cfg(all(test, feature = "gui"))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn answers_from_the_open_app() {
        let dir = std::env::temp_dir().join(format!("sqd-ipc-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("big")).unwrap();
        std::fs::write(dir.join("big/a.bin"), vec![1u8; 400_000]).unwrap();
        std::fs::write(dir.join("small.bin"), vec![1u8; 50_000]).unwrap();
        let handle = crate::scan::local::start(dir.clone(), "x".into());
        while !handle.progress.is_done() {
            std::thread::sleep(Duration::from_millis(10));
        }
        let scans = Arc::new(Mutex::new(Scans { done: vec![handle.tree.clone()], view: Some((handle.tree.clone(), 0)) }));
        // short path: a socket path must fit in about 100 bytes
        let socket = PathBuf::from(format!("/tmp/sqd-test-{}.sock", std::process::id()));
        let s = scans.clone();
        serve(socket.clone(), move |r| crate::cli::answer(&s, r)).unwrap();
        let req = |path: Option<&Path>| serde_json::json!({ "path": path, "depth": 1, "top": 20, "json": false }).to_string();

        let inside = ask(&socket, &req(Some(&dir.join("big")))).expect("a folder in the scan");
        assert!(inside.contains("a.bin") && inside.contains("from the open app"), "{inside}");
        let window = ask(&socket, &req(None)).expect("the folder the window shows");
        assert!(window.lines().nth(2).unwrap().ends_with("big/"), "{window}");
        assert_eq!(ask(&socket, &req(Some(Path::new("/nowhere/else")))), None);

        let _ = std::fs::remove_file(socket);
        let _ = std::fs::remove_dir_all(dir);
    }
}
