use serde_json::Value;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const ROOT: &str = "/sys/fs/cgroup";
const MAGIC: &[u8; 6] = b"i3-ipc";

fn cgroup_path(text: &str) -> io::Result<PathBuf> {
    let group = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing v2 cgroup"))?;
    let path = Path::new(group);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cgroup path",
        ));
    }
    Ok(Path::new(ROOT).join(group.strip_prefix('/').unwrap()))
}

fn pid_group(pid: u64) -> io::Result<PathBuf> {
    cgroup_path(&fs::read_to_string(format!("/proc/{pid}/cgroup"))?)
}

fn regions(text: &str) -> io::Result<Vec<(String, u64)>> {
    let mut result = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let name = fields.next().ok_or_else(invalid_regions)?;
        let value = fields
            .next()
            .ok_or_else(invalid_regions)?
            .parse()
            .map_err(|_| invalid_regions())?;
        if fields.next().is_some()
            || !name.starts_with("drm/")
            || result.iter().any(|(n, _)| n == name)
        {
            return Err(invalid_regions());
        }
        result.push((name.to_owned(), value));
    }
    if result.is_empty() {
        return Err(invalid_regions());
    }
    Ok(result)
}

fn invalid_regions() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid dmem regions")
}

fn matching_values(file: &Path, capacity: &[(String, u64)], boosted: bool) -> io::Result<bool> {
    let values = regions(&fs::read_to_string(file)?)?;
    Ok(values.len() == capacity.len()
        && capacity.iter().all(|(name, limit)| {
            values
                .iter()
                .any(|(key, value)| key == name && *value == if boosted { *limit } else { 0 })
        }))
}

fn write_regions(file: &File, capacity: &[(String, u64)], boost: bool) -> io::Result<()> {
    write_regions_with(capacity, boost, |name, value| {
        let mut target = file;
        target.write_all(format!("{name} {value}\n").as_bytes())
    })
}

fn write_regions_with(
    capacity: &[(String, u64)],
    boost: bool,
    mut write: impl FnMut(&str, u64) -> io::Result<()>,
) -> io::Result<()> {
    for (name, limit) in capacity {
        write(name, if boost { *limit } else { 0 })?;
    }
    Ok(())
}

fn clear_owned(file: &File, capacity: &[(String, u64)]) -> io::Result<()> {
    let values = regions(&fs::read_to_string(format!(
        "/proc/self/fd/{}",
        file.as_raw_fd()
    ))?)?;
    for (name, limit) in capacity {
        if values
            .iter()
            .any(|(key, value)| key == name && value == limit)
        {
            write_regions(file, &[(name.clone(), 0)], false)?;
        }
    }
    Ok(())
}

fn same_scope(group: &Path, inode: u64) -> bool {
    fs::metadata(group).is_ok_and(|metadata| metadata.ino() == inode)
}

fn scope_file(group: &Path, inode: u64) -> io::Result<File> {
    let directory = File::open(group)?;
    if directory.metadata()?.ino() != inode {
        return Err(io::Error::other("tracked scope was replaced"));
    }
    OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/fd/{}/dmem.low", directory.as_raw_fd()))
}

fn descends_from(pid: u64, ancestor: u64) -> io::Result<bool> {
    let mut current = pid;
    for _ in 0..64 {
        if current == ancestor {
            return Ok(true);
        }
        let stat = fs::read_to_string(format!("/proc/{current}/stat"))?;
        let fields = stat
            .rsplit_once(") ")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process stat"))?
            .1;
        current = fields
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing parent PID"))?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid parent PID"))?;
        if current <= 1 {
            return Ok(false);
        }
    }
    Ok(false)
}

fn reaper_launch(args: &[u8], app_id: &str) -> bool {
    let mut words = args.split(|byte| *byte == 0);
    words.next();
    words.next() == Some(b"SteamLaunch".as_slice())
        && words.next() == Some(format!("AppId={}", &app_id[10..]).as_bytes())
        && words.next() == Some(b"--".as_slice())
}

fn owned_scope(group: &Path, app: &Path, game_pid: u64, app_id: &str) -> io::Result<bool> {
    let group = group.canonicalize()?;
    let app = app.canonicalize()?;
    let Some(name) = group.file_name().and_then(|n| n.to_str()) else {
        return Ok(false);
    };
    if group.parent() != Some(app.as_path())
        || !name.starts_with("run-p")
        || !name.ends_with(".scope")
    {
        return Ok(false);
    }
    if pid_group(game_pid)?.canonicalize()? != group {
        return Ok(false);
    }
    let members = fs::read_to_string(group.join("cgroup.procs"))?;
    let pids: Vec<u64> = members
        .split_whitespace()
        .map(|pid| {
            pid.parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid PID"))
        })
        .collect::<io::Result<_>>()?;
    let mut reaper = None;
    for &pid in &pids {
        if fs::read_to_string(format!("/proc/{pid}/comm"))?.trim() == "reaper" {
            let args = fs::read(format!("/proc/{pid}/cmdline"))?;
            if reaper_launch(&args, app_id) && reaper.replace(pid).is_some() {
                return Ok(false);
            }
        }
    }
    let Some(reaper) = reaper else {
        return Ok(false);
    };
    if !pids.contains(&game_pid) {
        return Ok(false);
    }
    for &pid in &pids {
        if pid_group(pid)?.canonicalize()? != group || !descends_from(pid, reaper)? {
            return Ok(false);
        }
    }
    Ok(group.join("dmem.low").exists())
}

fn send(stream: &mut UnixStream, kind: u32, payload: &[u8]) -> io::Result<()> {
    stream.write_all(MAGIC)?;
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(&kind.to_le_bytes())?;
    stream.write_all(payload)
}

fn read_full(
    stream: &mut UnixStream,
    buf: &mut [u8],
    stop: Option<&AtomicBool>,
    deadline: Option<Instant>,
) -> io::Result<()> {
    let mut offset = 0;
    let mut deadline = deadline;
    while offset < buf.len() {
        match stream.read(&mut buf[offset..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => {
                offset += n;
                if deadline.is_none() {
                    deadline = Some(Instant::now() + Duration::from_secs(3));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if stop.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                if (offset == 0 && deadline.is_none())
                    || deadline.is_some_and(|end| Instant::now() >= end)
                {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn receive(
    stream: &mut UnixStream,
    stop: Option<&AtomicBool>,
    deadline: Option<Instant>,
) -> io::Result<(u32, Value)> {
    let mut header = [0; 14];
    read_full(stream, &mut header, stop, deadline)?;
    if &header[..6] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Sway IPC header",
        ));
    }
    let size = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    if size > 16 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized Sway IPC reply",
        ));
    }
    let kind = u32::from_le_bytes(header[10..14].try_into().unwrap());
    let mut payload = vec![0; size];
    read_full(stream, &mut payload, stop, deadline)?;
    let value = serde_json::from_slice(&payload)?;
    Ok((kind, value))
}

fn focused(node: &Value) -> Option<&Value> {
    for key in ["nodes", "floating_nodes"] {
        for child in node[key].as_array().into_iter().flatten() {
            if let Some(value) = focused(child) {
                return Some(value);
            }
        }
    }
    (node["focused"] == true).then_some(node)
}

fn snapshot(socket: &str, stop: &AtomicBool) -> io::Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    send(&mut stream, 4, b"")?;
    let (kind, tree) = receive(
        &mut stream,
        Some(stop),
        Some(Instant::now() + Duration::from_secs(3)),
    )?;
    if kind != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected Sway tree reply",
        ));
    }
    Ok(tree)
}

fn steam_id(node: &Value) -> Option<&str> {
    fn valid(id: &str) -> bool {
        id.strip_prefix("steam_app_")
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    }
    let app = node["app_id"].as_str().filter(|id| valid(id));
    let class = node["window_properties"]["class"]
        .as_str()
        .filter(|id| valid(id));
    match (app, class) {
        (Some(a), Some(b)) if a != b => None,
        (Some(a), _) => Some(a),
        (_, Some(b)) => Some(b),
        _ => None,
    }
}

struct Booster {
    app: PathBuf,
    capacity: Vec<(String, u64)>,
    previous: Option<(PathBuf, u64)>,
}

impl Booster {
    fn update(&mut self, tree: &Value) {
        let mut focused_game = None;
        let candidate = focused(tree)
            .and_then(|node| Some((node["pid"].as_u64()?, steam_id(node)?)))
            .and_then(|(pid, app_id)| {
                focused_game = Some((pid, app_id));
                match pid_group(pid).and_then(|group| {
                    if owned_scope(&group, &self.app, pid, app_id)? {
                        Ok(Some(group))
                    } else {
                        Ok(None)
                    }
                }) {
                    Ok(group) => group,
                    Err(error) => {
                        eprintln!("scope check: {error}");
                        None
                    }
                }
            });
        if self.previous.as_ref().map(|(path, _)| path) == candidate.as_ref() {
            if let Some((group, inode)) = &self.previous {
                if !same_scope(group, *inode) {
                    eprintln!("tracked scope was replaced; refusing further writes");
                    self.previous = None;
                } else if !matching_values(&group.join("dmem.low"), &self.capacity, true)
                    .unwrap_or(false)
                    || !focused_game.is_some_and(|(pid, app_id)| {
                        owned_scope(group, &self.app, pid, app_id).unwrap_or(false)
                    })
                {
                    eprintln!("tracked scope changed; clearing owned regions");
                    self.clear();
                }
            }
            return;
        }
        if !self.clear() {
            return;
        }
        if let Some(group) = candidate {
            let file = group.join("dmem.low");
            match matching_values(&file, &self.capacity, false) {
                Ok(true) => match fs::metadata(&group) {
                    Ok(metadata) => {
                        let target = scope_file(&group, metadata.ino());
                        self.previous = Some((group, metadata.ino()));
                        if let Err(error) =
                            target.and_then(|target| write_regions(&target, &self.capacity, true))
                        {
                            eprintln!("boost failed: {error}");
                            self.clear();
                        }
                    }
                    Err(error) => eprintln!("scope disappeared: {error}"),
                },
                Ok(false) => eprintln!("scope has existing dmem.low values; refusing to overwrite"),
                Err(error) => eprintln!("cannot read scope dmem.low: {error}"),
            }
        }
    }

    fn clear(&mut self) -> bool {
        let Some((group, inode)) = self.previous.take() else {
            return true;
        };
        if !same_scope(&group, inode) {
            eprintln!("tracked scope disappeared or was replaced; refusing clear");
            return true;
        }
        if group.parent() != Some(self.app.as_path()) {
            eprintln!("tracked scope left app.slice; refusing clear");
            return false;
        }
        if let Err(error) =
            scope_file(&group, inode).and_then(|file| clear_owned(&file, &self.capacity))
        {
            eprintln!("cannot clear {}: {error}", group.display());
            if same_scope(&group, inode) {
                self.previous = Some((group, inode));
                return false;
            }
        }
        true
    }
}

fn sway_socket() -> io::Result<String> {
    let inherited = env::var("SWAYSOCK").ok();
    if let Some(socket) = &inherited {
        if UnixStream::connect(socket).is_ok() {
            return Ok(socket.clone());
        }
    }
    let uid = unsafe { libc_uid() };
    let directory = format!("/run/user/{uid}");
    let mut sockets = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_string_lossy()
            .starts_with(&format!("sway-ipc.{uid}."))
            && name.to_string_lossy().ends_with(".sock")
            && UnixStream::connect(entry.path()).is_ok()
        {
            sockets.push(entry.path());
        }
    }
    if sockets.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no unique Sway socket",
        ));
    }
    Ok(sockets.remove(0).to_string_lossy().into_owned())
}

fn run(booster: &mut Booster, stop: &AtomicBool) -> io::Result<()> {
    let socket = sway_socket()?;
    let mut events = UnixStream::connect(&socket)?;
    events.set_read_timeout(Some(Duration::from_secs(1)))?;
    send(&mut events, 2, br#"["window","workspace"]"#)?;
    let (kind, reply) = receive(
        &mut events,
        Some(stop),
        Some(Instant::now() + Duration::from_secs(3)),
    )?;
    if kind != 2 || reply["success"] != true {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Sway subscription failed",
        ));
    }
    booster.update(&snapshot(&socket, stop)?);
    let mut checked = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        match receive(&mut events, Some(stop), None) {
            Ok((kind, _)) if kind == 0x80000003 || kind == 0x80000000 => {
                booster.update(&snapshot(&socket, stop)?);
                checked = Instant::now();
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => break,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
        if checked.elapsed() >= Duration::from_secs(5) {
            booster.update(&snapshot(&socket, stop)?);
            checked = Instant::now();
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    if env::args_os().nth(1).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: sway-foreground-booster-dmemcg",
        ));
    }
    let uid = unsafe { libc_uid() };
    let app = PathBuf::from(format!(
        "{ROOT}/user.slice/user-{uid}.slice/user@{uid}.service/app.slice"
    ));
    let capacity = regions(&fs::read_to_string(format!("{ROOT}/dmem.capacity"))?)?;
    if !fs::read_to_string(app.join("cgroup.subtree_control"))?
        .split_whitespace()
        .any(|s| s == "dmem")
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "dmem not enabled for app scopes",
        ));
    }
    let mut booster = Booster {
        app,
        capacity,
        previous: None,
    };
    let stop = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stop))?;
    flag::register(SIGTERM, Arc::clone(&stop))?;
    while !stop.load(Ordering::Relaxed) {
        if let Err(error) = run(&mut booster, &stop) {
            if !stop.load(Ordering::Relaxed) {
                eprintln!("Sway IPC: {error}");
            }
            booster.clear();
            if !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
    if booster.clear() {
        Ok(())
    } else {
        Err(io::Error::other("could not clear boosted scope"))
    }
}

unsafe extern "C" {
    fn getuid() -> u32;
}

unsafe fn libc_uid() -> u32 {
    getuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_v2() {
        assert_eq!(
            cgroup_path("1:net_cls:/elsewhere\n0::/user.slice/game.scope\n").unwrap(),
            PathBuf::from("/sys/fs/cgroup/user.slice/game.scope")
        );
        assert!(cgroup_path("0::/user.slice/../session.scope").is_err());
        assert!(cgroup_path("1:net_cls:/a").is_err());
    }

    #[test]
    fn parses_regions() {
        assert_eq!(
            regions("drm/card/vram 42\ndrm/card/gtt 12\n")
                .unwrap()
                .len(),
            2
        );
        assert!(regions("drm/card/vram max").is_err());
        assert!(regions("drm/card/vram 1\ndrm/card/vram 2").is_err());
    }

    #[test]
    fn focus_tree() {
        let tree: Value = serde_json::json!({"focused":true,"nodes":[{"focused":true,"pid":42,"window_properties":{"class":"steam_app_960090"}}]});
        assert_eq!(focused(&tree).unwrap()["pid"], 42);
    }

    #[test]
    fn detects_only_unambiguous_steam_windows() {
        let node = serde_json::json!({"app_id":"steam_app_960090"});
        assert_eq!(steam_id(&node), Some("steam_app_960090"));
        let node = serde_json::json!({"window_properties":{"class":"steam_app_42"}});
        assert_eq!(steam_id(&node), Some("steam_app_42"));
        for bad in ["steam_app_", "steam_app_123x", "discord"] {
            assert_eq!(steam_id(&serde_json::json!({"app_id":bad})), None);
        }
        let conflict = serde_json::json!({"app_id":"steam_app_42","window_properties":{"class":"steam_app_43"}});
        assert_eq!(steam_id(&conflict), None);
    }

    #[test]
    fn matches_exact_steam_launch_arguments() {
        assert!(reaper_launch(
            b"/steam/reaper\0SteamLaunch\0AppId=960090\0--\0game\0",
            "steam_app_960090"
        ));
        assert!(!reaper_launch(
            b"/steam/reaper\0SteamLaunch\0AppId=9600900\0--\0",
            "steam_app_960090"
        ));
        assert!(!reaper_launch(
            b"/steam/reaper\0other\0AppId=960090\0--\0",
            "steam_app_960090"
        ));
    }

    #[test]
    fn per_region_writes() {
        let capacity = vec![("drm/a/vram".to_owned(), 42), ("drm/b/vram".to_owned(), 12)];
        let mut writes = Vec::new();
        write_regions_with(&capacity, true, |name, value| {
            writes.push((name.to_owned(), value));
            Ok(())
        })
        .unwrap();
        assert_eq!(writes, capacity);
        writes.clear();
        write_regions_with(&capacity, false, |name, value| {
            writes.push((name.to_owned(), value));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            writes,
            vec![("drm/a/vram".to_owned(), 0), ("drm/b/vram".to_owned(), 0)]
        );
    }

    #[test]
    fn write_failure_stops_before_next_region() {
        let capacity = vec![("drm/a/vram".to_owned(), 42), ("drm/b/vram".to_owned(), 12)];
        let mut writes = Vec::new();
        let result = write_regions_with(&capacity, true, |name, value| {
            writes.push((name.to_owned(), value));
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(writes, vec![("drm/a/vram".to_owned(), 42)]);
    }
}
