//! Other machines: their profiles, and the pipe to the core on each.

use crate::addr::check_name;
use crate::rpc::RpcError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Machine {
    /// What this core calls it: `rohan` in `rohan/t7`.
    pub name: String,
    /// How it is reached: an ssh target, or (`via`) a command.
    pub target: String,
    /// `target` is a command that runs a shell line there, given as its last
    /// argument, like ssh (a provider's own CLI, ssh with its options).
    #[serde(default)]
    pub via: bool,
    /// Short-lived (a sandbox for a task): clients may show it apart from the
    /// machines that stay. hc treats it like any other.
    #[serde(default)]
    pub temporary: bool,
    /// The hc session there (null: its default).
    pub session: Option<String>,
    /// The other core's permanent id.
    pub id: String,
    /// What that core calls itself (its hostname).
    pub host: String,
    /// Its hc version when last seen.
    pub version: String,
    /// The pinned host key (SHA256 fingerprint), for a machine added with a
    /// password: it is reached with the embedded ssh (machines_russh), and a
    /// different key there is refused. Null for machines reached by the ssh
    /// program, and in records written before this field.
    #[serde(default)]
    pub host_key: Option<String>,
    /// The planted login key's file name (in the state folder's `keys/`),
    /// for a machine added with a password.
    #[serde(default)]
    pub key_file: Option<String>,
    /// Last observed systemd logout protection; null for old or unsupported cores.
    #[serde(default)]
    pub keeps_running_after_logout: Option<bool>,
    /// Remote account for loginctl enable-linger, when applicable.
    #[serde(default)]
    pub logout_linger_user: Option<String>,
}

impl Machine {
    pub fn reach(&self) -> Reach<'_> {
        Reach {
            target: &self.target,
            via: self.via,
            host_key: self.host_key.as_deref(),
            key_file: self.key_file.as_deref(),
        }
    }
}

/// How hc gets there: `ssh <target> <line>`, or `<target> <line>` (`via`).
/// With a pinned host key, the embedded ssh (machines_russh) and the
/// planted key get there instead of the ssh program.
#[derive(Clone, Copy)]
pub struct Reach<'a> {
    pub target: &'a str,
    pub via: bool,
    /// A pinned host key fingerprint: the mark of an embedded-ssh machine.
    pub host_key: Option<&'a str>,
    /// The planted login key's file name, with `host_key`.
    pub key_file: Option<&'a str>,
}

impl Reach<'_> {
    /// Reached by the embedded ssh, not the ssh program.
    pub fn embedded(&self) -> bool {
        !self.via && self.host_key.is_some()
    }
}

/// A plain ssh reach (the ssh program, no planted key).
pub fn ssh_reach(target: &str) -> Reach<'_> {
    Reach { target, via: false, host_key: None, key_file: None }
}

impl Reach<'_> {
    /// Says how to add it again, for hints.
    fn add(&self) -> String {
        match self.via {
            true => crate::platform::via_add_hint(self.target),
            false => format!("hc machine add {}", self.target),
        }
    }

    /// The command that runs `line` there.
    fn command(
        &self,
        paths: &crate::paths::Paths,
        extra: &[&str],
        line: &str,
    ) -> Result<std::process::Command, RpcError> {
        if self.via {
            return crate::platform::via_command(self.target, line).map_err(|e| unreachable(*self, &e.to_string()));
        }
        let mut cmd = ssh(paths, self.target, extra);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // A pipe from the detached core must not open a console of its own.
            cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        if std::env::var_os("HC_SSH").is_none() {
            cmd.arg("-T");
        }
        cmd.arg(self.target).arg(line);
        Ok(cmd)
    }
}

/// No machine by that name: the error lists the ones there are.
pub fn unknown(machines: &[Machine], name: &str) -> crate::rpc::RpcError {
    let names: Vec<&str> = machines.iter().map(|m| m.name.as_str()).collect();
    let hint = if names.is_empty() {
        "no machines yet: hc machine add <ssh-target>".into()
    } else {
        format!("machines: {}", names.join(", "))
    };
    crate::rpc::RpcError::new("machine_unknown", format!("unknown machine {name:?}"), hint)
}

pub fn save(state: &Path, machines: &[Machine]) {
    let bytes = serde_json::to_vec_pretty(machines).expect("machines serialize");
    if let Err(e) = crate::paths::write_atomic(&state.join("machines.json"), &bytes) {
        let _ = writeln!(io::stderr(), "core: cannot save machines: {e}");
    }
}

/// `can@rohan:2222`, as people write a port, in the form ssh takes it
/// (`ssh://can@rohan:2222`): ssh reads a plain `rohan:2222` as a host name.
pub fn ssh_target(target: String) -> String {
    let host = target.rsplit_once('@').map_or(target.as_str(), |(_, host)| host);
    match host.split_once(':') {
        Some((h, port))
            if !target.starts_with("ssh://")
                && !h.is_empty()
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            format!("ssh://{target}")
        }
        _ => target,
    }
}

/// An ssh option token taken apart: its name and any value stuck to it
/// (`-p2222` -> (`-p`, `2222`), `--opt=v` -> (`--opt`, `v`)).
fn option_parts(tok: &str) -> (&str, Option<&str>) {
    if tok.starts_with("--") {
        return match tok.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (tok, None),
        };
    }
    let name_len = tok.chars().take(2).map(|c| c.len_utf8()).sum::<usize>();
    let (name, rest) = tok.split_at(name_len.min(tok.len()));
    (name, if rest.is_empty() { None } else { Some(rest) })
}

/// A target's host part taken apart as written: the host (an IPv6 literal
/// keeps its brackets) and the port it names, if any. A non-numeric port
/// stays part of the host, as in machines_russh's parse_target. None: a
/// bracket that never closes, or junk after it.
fn split_host_port(hostport: &str) -> Option<(String, Option<String>)> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?.to_string()),
        };
        if port.as_ref().is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
            return None;
        }
        return Some((format!("[{host}]"), port));
    }
    match hostport.split_once(':') {
        Some((host, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            Some((host.to_string(), Some(p.to_string())))
        }
        _ => Some((hostport.to_string(), None)),
    }
}

/// A pasted ssh command line (`ssh -p 2222 can@rohan`, `ssh rohan -p 2222`),
/// taken down to the target an add stores: the destination, with the user
/// and the port the command names (a `-p` after the destination counts
/// too: people write it there). An option an add cannot carry (-i, -J,
/// -o, …) is named in the error: it belongs in ~/.ssh/config, whose hosts
/// an add takes by name. Anything that is not an ssh command line comes
/// back as it was (trimmed), so plain targets read as before.
pub fn normalize_target(input: &str) -> Result<String, RpcError> {
    let trimmed = input.trim();
    let mut words = trimmed.split_whitespace();
    if words.next() != Some("ssh") {
        return Ok(trimmed.to_string());
    }
    let bad = || RpcError::invalid_params(format!("bad ssh target {trimmed:?}"));
    let rest: Vec<&str> = words.collect();
    let (mut user, mut port, mut dest) = (None, None, None);
    let mut positional_only = false;
    let mut i = 0;
    while i < rest.len() {
        let tok = rest[i];
        i += 1;
        if !positional_only && tok == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && tok.starts_with('-') && tok.len() > 1 {
            let (name, attached) = option_parts(tok);
            if name != "-p" && name != "-l" {
                return Err(RpcError::invalid_params(format!(
                    "that ssh command uses {name}, which an add cannot carry"
                ))
                .with_hint("put the option in ~/.ssh/config and add the host by its name there"));
            }
            let value = match attached {
                Some(v) => v,
                None => {
                    let Some(v) = rest.get(i) else {
                        return Err(RpcError::invalid_params(format!("ssh's {name} option needs a value after it")));
                    };
                    i += 1;
                    v
                }
            };
            if name == "-p" {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(RpcError::invalid_params(format!(
                        "ssh's -p option needs a port number, not {value:?}"
                    )));
                }
                port = Some(value);
            } else {
                user = Some(value);
            }
            continue;
        }
        if dest.is_none() {
            dest = Some(tok);
            continue;
        }
        return Err(RpcError::invalid_params(format!(
            "an add names one machine; {tok:?} is a command, not part of its address"
        ))
        .with_hint("add the machine first, then run commands in its terminals"));
    }
    let Some(dest) = dest else {
        return Err(RpcError::invalid_params("that ssh command names no machine to add"));
    };
    let (dest_user, hostport) = match dest.rsplit_once('@') {
        Some((u, h)) if !u.is_empty() => (Some(u), h),
        _ => (None, dest),
    };
    let Some((host, dest_port)) = split_host_port(hostport) else { return Err(bad()) };
    if host.is_empty() || host == "[]" {
        return Err(bad());
    }
    let user = dest_user.or(user);
    let port = port.map(str::to_string).or(dest_port);
    let mut out = match user {
        Some(u) => format!("{u}@{host}"),
        None => host,
    };
    if let Some(p) = port {
        // A host still carrying a colon (a port parse_target reads as part
        // of the name, or a bare IPv6 literal) takes no suffix.
        if !out.contains(':') || out.contains(']') {
            out = format!("{out}:{p}");
        }
    }
    Ok(out)
}

/// The ~/.ssh/known_hosts line that records trust in a host key, in the
/// form ssh itself writes it: `[host]:port` away from port 22, an IPv6
/// literal in brackets either way.
pub fn known_hosts_line(host: &str, port: u16, openssh_key: &str) -> String {
    let named = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    match port {
        22 => format!("{named} {openssh_key}"),
        _ if host.contains(':') => format!("{named}:{port} {openssh_key}"),
        _ => format!("[{named}]:{port} {openssh_key}"),
    }
}

/// Records a host key a person trusted during an add (its fingerprint
/// passed back as --trust-host-key) in ~/.ssh/known_hosts, so the ssh
/// program — the way this machine is reached — honors the same yes.
/// A host already named there, with any key, is left as it is: ssh's own
/// warning is the guide then.
pub fn record_trusted_host_key(target: &str, openssh_key: &str) -> Result<(), RpcError> {
    let t = crate::machines_russh::parse_target(target)?;
    let line = known_hosts_line(&t.host, t.port, openssh_key);
    let host_field = line.split_whitespace().next().unwrap_or_default().to_string();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .ok_or_else(|| RpcError::internal("cannot find the home folder to record the trusted host key"))?;
    let dir = home.join(".ssh");
    let file = dir.join("known_hosts");
    let already = std::fs::read_to_string(&file)
        .is_ok_and(|text| text.lines().any(|l| l.split_whitespace().next() == Some(host_field.as_str())));
    if already {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| RpcError::internal(format!("cannot make {}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| RpcError::internal(format!("cannot open {}: {e}", file.display())))?;
    writeln!(f, "{line}").map_err(|e| RpcError::internal(format!("cannot write {}: {e}", file.display())))?;
    Ok(())
}

/// A free name from the target's host: `can@rohan.lan` -> `rohan`; IPs stay whole.
pub fn default_name(target: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let t = target.strip_prefix("ssh://").unwrap_or(target);
    let host = t.rsplit_once('@').map_or(t, |(_, host)| host);
    let host = host.split(':').next()?.to_ascii_lowercase();
    let ip = host.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    let mut name = if ip { host } else { host.split('.').next()?.to_string() };
    if name.len() > 32 {
        name = name.chars().take(32).collect::<String>().trim_end_matches('-').into();
    }
    if !crate::terminals::id_shaped(&name) {
        check_name(&name).ok()?;
    }
    let base = name.clone();
    let mut number = 1;
    while crate::terminals::id_shaped(&name) || taken(&name) {
        number += 1;
        let suffix = format!("-{number}");
        name = format!("{}{}", base[..base.len().min(32 - suffix.len())].trim_end_matches('-'), suffix);
    }
    Some(name)
}

/// An ssh target goes to ssh as one argument: never an option, never two
/// words. A command (`via`) is one local command line.
pub fn check_target(to: Reach) -> Result<(), RpcError> {
    let target = to.target;
    if to.via {
        if target.trim().is_empty() || target.chars().any(char::is_control) {
            return Err(RpcError::invalid_params(format!("bad command {target:?}: one line, no control characters")));
        }
        return crate::platform::check_via(target)
            .map_err(|e| RpcError::invalid_params(format!("bad command {target:?}: {e}")));
    }
    if target.is_empty() || target.starts_with('-') || target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(RpcError::invalid_params(format!("bad ssh target {target:?}")));
    }
    Ok(())
}

/// A host as the add form gives it (host, user, port), in the target form
/// hc stores: `ssh://user@host:port` when a port is written (the form
/// `ssh_target` would make of it), else `user@host`.
pub fn compose_target(host: &str, user: Option<&str>, port: Option<u16>) -> String {
    let user = user.filter(|u| !u.is_empty()).map(|u| format!("{u}@")).unwrap_or_default();
    match port {
        Some(port) => format!("ssh://{user}{host}:{port}"),
        None => format!("{user}{host}"),
    }
}

/// Where a machine's planted key lives: the state folder's `keys/`.
fn key_path(state: &Path, name: &str) -> std::path::PathBuf {
    state.join("keys").join(format!("{name}.key"))
}

/// A machine's planted login key, made on first use and kept (0600, in a
/// 0700 folder): (its file name, its authorized_keys line). Reused as-is
/// afterwards, so re-adding a machine plants nothing new.
pub fn ensure_planted_key(state: &Path, name: &str) -> Result<(String, String), RpcError> {
    let path = key_path(state, name);
    let file_name = path.file_name().expect("key path has a file name").to_string_lossy().into_owned();
    if let Ok(text) = std::fs::read_to_string(&path) {
        let key = crate::machines_russh::load_key(&text)?;
        return Ok((file_name, crate::machines_russh::public_line(&key)?));
    }
    let key = crate::machines_russh::new_key_pair()?;
    let secret = crate::machines_russh::secret_text(&key)?;
    let dir = path.parent().expect("key path has a folder");
    std::fs::create_dir_all(dir).map_err(|e| RpcError::internal(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    crate::paths::write_atomic(&path, secret.as_bytes()).map_err(|e| RpcError::internal(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((file_name, crate::machines_russh::public_line(&key)?))
}

/// The planted key a dial authenticates with.
fn planted_auth(state: &Path, file_name: &str) -> Result<crate::machines_russh::Auth, RpcError> {
    let text = std::fs::read_to_string(state.join("keys").join(file_name)).map_err(|e| {
        RpcError::new(
            "needs_login",
            format!("the planted login key is unreadable: {e}"),
            "remove the machine and add it again with its password",
        )
    })?;
    Ok(crate::machines_russh::Auth::Key(crate::machines_russh::load_key(&text)?))
}

/// One login per target, shared: a person logs in once in a terminal (codes,
/// passwords), and the core's connections ride that login. Short, private path.
#[cfg(unix)]
fn control_path(paths: &crate::paths::Paths, target: &str) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    target.hash(&mut h);
    paths.socket.parent().expect("socket has a dir").join(format!("ssh-{:012x}", h.finish() & 0xffff_ffff_ffff))
}

/// `ssh` riding the shared login when there is one, with `extra` options; or
/// `$HC_SSH` in its place (tests), which takes only the target and the remote
/// command. Only `login` creates the shared login: one made by the line (or
/// an install) would end with it, and take every connection riding it along
/// (an update restarting hc there ended its own upload that way).
fn ssh(paths: &crate::paths::Paths, target: &str, extra: &[&str]) -> std::process::Command {
    if let Some(fake) = std::env::var_os("HC_SSH") {
        return std::process::Command::new(fake);
    }
    let mut ssh = std::process::Command::new("ssh");
    #[cfg(unix)]
    {
        let master = if extra.iter().any(|o| o.starts_with("ControlPersist")) { "auto" } else { "no" };
        ssh.arg("-S").arg(control_path(paths, target)).args(["-o", &format!("ControlMaster={master}")]);
    }
    #[cfg(windows)]
    let _ = (paths, target); // Windows OpenSSH has no ControlMaster support.
    for option in extra {
        ssh.args(["-o", option]);
    }
    ssh
}

/// Logs in from this terminal, so ssh can ask for a password or a code. The
/// login stays up for the core's connections (10 minutes once unused). A
/// command (`via`) logs in its own way: nothing to do.
pub fn login(paths: &crate::paths::Paths, to: Reach) -> Result<(), RpcError> {
    check_target(to)?;
    let target = to.target;
    if to.via || cfg!(windows) || to.embedded() {
        // An embedded machine's planted key is its login: nothing to type.
        return Ok(());
    }
    paths.create_dirs().map_err(|e| RpcError::internal(e.to_string()))?;
    let mut cmd = ssh(paths, target, &["BatchMode=no", "ControlPersist=600"]);
    cmd.env_remove("SSH_ASKPASS").env("SSH_ASKPASS_REQUIRE", "never").arg(target).arg("exit");
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(RpcError::new("needs_login", format!("logging in to {target} failed"), "ssh said why, above")),
        Err(e) => Err(unreachable(to, &format!("cannot run ssh: {e}"))),
    }
}

/// Ends the shared login to `target`, if one is up (a forgotten machine).
pub fn logout(paths: &crate::paths::Paths, to: Reach) {
    let target = to.target;
    if cfg!(unix) && std::env::var_os("HC_SSH").is_none() && !to.via {
        let mut cmd = ssh(paths, target, &[]);
        cmd.args(["-O", "exit", target])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = cmd.status();
    }
}

/// Targets found to be Windows, so redials go straight.
static WINDOWS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn is_windows(target: &str) -> bool {
    WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|t| t == target)
}

/// Where install.ps1 puts hc, from the home folder an ssh session starts in.
/// Not PATH: sessions riding a shared login keep the PATH from before an
/// install. `.\` reads the same in cmd and PowerShell.
const WINDOWS_HC: &str = r".\AppData\Local\Programs\herdr-core\herdr-core.exe";

/// The remote line that starts `herdr-core connect` there: found on PATH,
/// else in ~/.local/bin. On Windows, whose shell may be cmd or PowerShell,
/// one line both read alike; the installer put hc on PATH. The ssh pipe
/// and the embedded pipe (machines_russh) run the same line.
fn connect_line(session: Option<&str>, windows: bool) -> String {
    if windows {
        let session = session.map(|s| format!(" --session {s}")).unwrap_or_default();
        format!(
            "cmd /d /c \"if exist {WINDOWS_HC} ({WINDOWS_HC} connect{session}) else (echo {NOT_HERE} 1>&2 & exit /b 127)\""
        )
    } else {
        let session = session.map(|s| format!("HC_SESSION={s} ")).unwrap_or_default();
        format!(
            "sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x \"$p\" ] || {{ echo \"{NOT_HERE}\" >&2; exit 127; }}; {session}exec \"$p\" connect'"
        )
    }
}

/// The pipe to the core there, over the ssh program. Never prompts: a login
/// a person must type fails fast.
fn pipe(
    paths: &crate::paths::Paths,
    to: Reach,
    session: Option<&str>,
    windows: bool,
) -> Result<tokio::process::Command, RpcError> {
    let remote = connect_line(session, windows);
    let options = ["BatchMode=yes", "ConnectTimeout=10", "ServerAliveInterval=15", "ServerAliveCountMax=4"];
    let mut cmd = tokio::process::Command::from(to.command(paths, &options, &remote)?);
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    Ok(cmd)
}

/// What the pipe's line says when there is no hc there.
const NOT_HERE: &str = "herdr-core is not installed here";

fn unreachable(to: Reach, why: &str) -> RpcError {
    let target = to.target;
    let hint = match to.via {
        true => format!("check that `{target} 'echo ok'` prints ok without asking anything"),
        false => format!("check that `ssh {target}` logs in without a password prompt (keys or an agent)"),
    };
    RpcError::new("unreachable", format!("cannot reach {target}: {why}"), hint)
}

/// ssh refused us a login only a person can give (code, password, key).
fn wants_login(err: &str) -> bool {
    // Too many keys offered (sshd's MaxAuthTries) fails auth the same way; retrying fast gets banned.
    err.contains("Too many authentication failures")
        || err.contains("Permission denied")
            && ["publickey", "keyboard-interactive", "password"].iter().any(|k| err.contains(k))
}

/// What keeps a dialed line alive: the ssh child, or the embedded
/// connection's pump task.
pub(crate) enum LineGuard {
    Child(tokio::process::Child),
    Embedded(tokio::task::JoinHandle<()>),
}

/// An open pipe to the core there, past its hello.
pub struct Dialed {
    pub stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    /// Framed from here on (when dialed framed); may hold bytes already read.
    pub stdout: BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
    pub stderr: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    pub hello: Value,
    pub(crate) guard: LineGuard,
}

impl LineGuard {
    /// Hangs up now: the ssh child is killed, the embedded pump aborted.
    pub(crate) fn kill(&mut self) {
        match self {
            LineGuard::Child(child) => {
                let _ = child.start_kill();
            }
            LineGuard::Embedded(task) => task.abort(),
        }
    }
}

impl Dialed {
    /// Hangs up now (see LineGuard::kill).
    pub fn kill(&mut self) {
        self.guard.kill();
    }
}

/// Dials, says hello and hangs up: the other core's hello, or why not.
pub async fn hello(paths: &crate::paths::Paths, to: Reach<'_>, session: Option<&str>) -> Result<Value, RpcError> {
    Ok(dial(paths, to, session, json!({})).await?.hello)
}

/// Dials and says hello, with `hello` added to its params (`framed`, who we are).
pub async fn dial(
    paths: &crate::paths::Paths,
    to: Reach<'_>,
    session: Option<&str>,
    hello: Value,
) -> Result<Dialed, RpcError> {
    if to.embedded() {
        return dial_embedded(paths, to, session, hello).await;
    }
    let mut probe_windows = false;
    match dial_once(paths, to, session, hello.clone(), is_windows(to.target), &mut probe_windows).await {
        Err(e) if probe_windows => match dial_once(paths, to, session, hello, true, &mut probe_windows).await {
            Err(_) if !is_windows(to.target) => Err(e),
            result => result,
        },
        other => other,
    }
}

async fn dial_once(
    paths: &crate::paths::Paths,
    to: Reach<'_>,
    session: Option<&str>,
    hello: Value,
    windows: bool,
    probe_windows: &mut bool,
) -> Result<Dialed, RpcError> {
    let target = to.target;
    let mut child =
        pipe(paths, to, session, windows)?.spawn().map_err(|e| unreachable(to, &format!("cannot run it: {e}")))?;
    let (mut stdin, stdout, mut stderr) =
        (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let mut lines = BufReader::new(Box::new(stdout) as Box<dyn tokio::io::AsyncRead + Send + Unpin>);
    let reply = tokio::time::timeout(Duration::from_secs(20), read_hello(&mut stdin, &mut lines, hello)).await;
    let reply = match reply {
        Err(_) => return Err(unreachable(to, "no answer within 20 s")),
        Ok(Some(reply)) => reply,
        Ok(None) => {
            // Bounded: something it started may hold stderr open, and a dial
            // that never ends leaves the machine "connecting" for good.
            let mut err = String::new();
            let status = tokio::time::timeout(Duration::from_secs(5), async {
                let _ = stderr.read_to_string(&mut err).await;
                child.wait().await.ok().and_then(|s| s.code())
            })
            .await
            .ok()
            .flatten();
            if !windows {
                // Windows may find Git's sh but fail in its shim or mangle
                // the script's quoting. Only our marker proves the script ran.
                *probe_windows = status != Some(255) && !err.contains(NOT_HERE) && !wants_login(&err);
            }
            let missing_windows = windows && err.contains(NOT_HERE);
            if missing_windows && !is_windows(target) {
                WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
            }
            // Some connection wrappers collapse all remote failures to exit code 1.
            if status == Some(127) || err.contains(NOT_HERE) {
                return Err(RpcError::new(
                    "hc_missing",
                    format!("hc is not installed on {target}"),
                    format!("in a terminal, `{}` offers to copy this hc there", to.add()),
                ));
            }
            if wants_login(&err) {
                if cfg!(windows) && !to.via {
                    return Err(RpcError::new(
                        "needs_login",
                        format!("SSH authentication to {target} failed"),
                        format!("check that `ssh {target}` logs in without a password prompt (keys or an agent)"),
                    ));
                }
                return Err(RpcError::new(
                    "needs_login",
                    format!("{target} wants a login typed by a person (a password or a code)"),
                    format!("run `{}` in a terminal", to.add()),
                ));
            }
            let why = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("the connection closed");
            return Err(unreachable(to, why.trim()));
        }
    };
    if windows && !is_windows(target) {
        WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
    }
    let hello = check_hello(to, reply)?;
    Ok(Dialed {
        stdin: Box::new(stdin),
        stdout: lines,
        stderr: Box::new(stderr),
        hello,
        guard: LineGuard::Child(child),
    })
}

/// Writes the hello request and reads its reply: the first line that is a
/// JSON reply to it (a login may print a banner first). None: the pipe
/// closed before any reply.
async fn read_hello(
    stdin: &mut (impl tokio::io::AsyncWrite + Unpin),
    lines: &mut BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
    hello: Value,
) -> Option<Value> {
    let mut params = json!({"generation": crate::api::GENERATION});
    params.as_object_mut().unwrap().extend(hello.as_object().cloned().unwrap_or_default());
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": params});
    let _ = stdin.write_all(format!("{req}\n").as_bytes()).await;
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line).await {
            Ok(1..) => match serde_json::from_str::<Value>(&line) {
                Ok(reply) if reply["id"] == 1 => return Some(reply),
                _ => continue,
            },
            _ => return None,
        }
    }
}

/// A hello reply, judged: the other core's hello, or why it cannot serve.
fn check_hello(to: Reach, reply: Value) -> Result<Value, RpcError> {
    let target = to.target;
    match reply.get("error") {
        Some(e) if e["data"]["kind"] == "unsupported_generation" => Err(RpcError::new(
            "incompatible",
            format!("hc on {target} speaks another protocol generation"),
            format!("in a terminal, `{}` offers to update it", to.add()),
        )),
        Some(e) => Err(RpcError::from_json(e)),
        None => Ok(reply["result"].clone()),
    }
}

/// Dials over the embedded ssh (machines_russh), with the planted key: the
/// same hello over the same connect line, errors classified the same way
/// from what the line printed and how it ended.
async fn dial_embedded(
    paths: &crate::paths::Paths,
    to: Reach<'_>,
    session: Option<&str>,
    hello: Value,
) -> Result<Dialed, RpcError> {
    let target = to.target;
    let Some(key_file) = to.key_file else {
        return Err(RpcError::new(
            "needs_login",
            format!("{target} was added with a password but has no planted key"),
            "remove the machine and add it again with its password",
        ));
    };
    let auth = planted_auth(&paths.state, key_file)?;
    let session_ssh = crate::machines_russh::open(target, auth, to.host_key).await?;
    let mut pipe = session_ssh.pipe(&connect_line(session, false)).await?;
    let mut lines = BufReader::new(pipe.stdout);
    let reply = tokio::time::timeout(Duration::from_secs(20), read_hello(&mut pipe.stdin, &mut lines, hello)).await;
    let reply = match reply {
        Err(_) => return Err(unreachable(to, "no answer within 20 s")),
        Ok(Some(reply)) => reply,
        Ok(None) => {
            // The pump may not have recorded the ending yet; give it a moment.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let err = {
                let seen = pipe.stderr_seen.lock().unwrap_or_else(|p| p.into_inner());
                String::from_utf8_lossy(&seen).into_owned()
            };
            let status = *pipe.exit.lock().unwrap_or_else(|p| p.into_inner());
            if status == Some(127) || err.contains(NOT_HERE) {
                return Err(RpcError::new(
                    "hc_missing",
                    format!("hc is not installed on {target}"),
                    "install hc there with the fleet install script, then add the machine again",
                ));
            }
            if wants_login(&err) {
                return Err(RpcError::new(
                    "needs_login",
                    format!("{target} wants a login typed by a person (a password or a code)"),
                    "remove the machine and add it again with its password",
                ));
            }
            let why = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("the connection closed");
            return Err(unreachable(to, why.trim()));
        }
    };
    let hello = check_hello(to, reply)?;
    Ok(Dialed { stdin: pipe.stdin, stdout: lines, stderr: pipe.stderr, hello, guard: LineGuard::Embedded(pipe.task) })
}

/// `uname -sm` as this binary names platforms: `linux x86_64`, `macos aarch64`.
fn platform(uname: &str) -> String {
    let mut words = uname.split_whitespace();
    let os = match words.next() {
        Some("Linux") => "linux",
        Some("Darwin") => "macos",
        Some(os) if os.starts_with("MINGW") || os.starts_with("MSYS") => "windows",
        other => other.unwrap_or("?"),
    };
    let arch = match words.next() {
        Some("arm64" | "aarch64") => "aarch64",
        Some("x86_64" | "amd64") => "x86_64",
        other => other.unwrap_or("?"),
    };
    format!("{os} {arch}")
}

/// Runs `command` there as its login shell takes it (riding the shared
/// login), feeding it `input`: the last line it printed.
fn run_there(paths: &crate::paths::Paths, to: Reach, command: &str, input: &[u8]) -> Result<String, RpcError> {
    let out = run_raw(paths, to, command, input)?;
    match out.status.success() {
        true => Ok(last_line(&out.stdout)),
        false => Err(RpcError::new(
            "install_failed",
            format!("on {}: {}", to.target, last_line(&out.stderr)),
            "nothing was changed there",
        )),
    }
}

fn last_line(b: &[u8]) -> String {
    String::from_utf8_lossy(b).lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_string()
}

fn run_raw(
    paths: &crate::paths::Paths,
    to: Reach,
    command: &str,
    input: &[u8],
) -> Result<std::process::Output, RpcError> {
    use std::io::Write;
    let mut cmd = to.command(paths, &["BatchMode=yes", "ConnectTimeout=10"], command)?;
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| unreachable(to, &format!("cannot run it: {e}")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(|e| unreachable(to, &e.to_string()))?;
    let _ = feeder.join();
    Ok(out)
}

/// Copies this very hc to `target` (same OS and CPU only), swaps it in at
/// ~/.local/bin/herdr-core (adding `hc` beside it only if there is none),
/// and restarts hc there in `session`, so terminals there keep running.
/// Returns what the installed one says it is.
pub fn install(paths: &crate::paths::Paths, to: Reach, session: Option<&str>) -> Result<String, RpcError> {
    check_target(to)?;
    let target = to.target;
    if to.embedded() {
        // The copy goes over the ssh program, which has no way into a
        // password-added machine; its hc is installed on the machine itself.
        return Err(RpcError::new(
            "install_failed",
            format!(
                "{target} was added with a password: its hc is installed by the fleet install script, run on {target}"
            ),
            "nothing was changed there",
        ));
    }
    if let Some(s) = session {
        check_name(s)?;
    }
    let here = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let probe = run_raw(paths, to, "uname -sm", b"")?;
    let there = platform(&last_line(&probe.stdout));
    let windows = if probe.status.success() {
        there.starts_with("windows ")
    } else {
        let native = run_raw(paths, to, "cmd /d /c \"echo herdr-core-windows\"", b"")?;
        native.status.success() && last_line(&native.stdout) == "herdr-core-windows"
    };
    if windows {
        if to.via {
            return Err(RpcError::new("install_failed", format!("{target} runs Windows"), "install hc there yourself"));
        }
        WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
        return install_windows(paths, target, session);
    }
    if !probe.status.success() {
        return Err(RpcError::new(
            "install_failed",
            format!("on {target}: {}", last_line(&probe.stderr)),
            "nothing was changed there",
        ));
    }
    // Same OS and CPU: this very hc. Else this version's released build for it.
    let scratch = crate::releases::Scratch::new()?;
    let macos = there.starts_with("macos ");
    let bundle = !macos || run_there(paths, to, "id -u", b"")? != "0";
    let current = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|e| RpcError::internal(format!("cannot find this hc: {e}")))?;
    let bin = if there == here && (bundle || !current.ends_with("Herdr Core.app/Contents/MacOS/herdr-core")) {
        current
    } else {
        let version = env!("CARGO_PKG_VERSION");
        let not_released = |_| {
            RpcError::new(
                "install_failed",
                format!("{target} is {there}, and hc {version} has no release to download"),
                "install hc there yourself",
            )
        };
        let m = crate::releases::manifest(Some(version), &scratch.0).map_err(not_released)?;
        crate::releases::download(&m, &there.replace(' ', "-"), &scratch.0, bundle)?
    };
    let (exe, bundled) = install_payload(&bin, &scratch.0)?;
    let n = exe.len();
    let session = session.map(|s| format!("HC_SESSION={s} ")).unwrap_or_default();
    if bundled {
        let script = format!(
            "set -e; d=$HOME/.local/bin; mkdir -p \"$d\"; t=$(mktemp -d); trap \"rm -rf \\\"$t\\\"\" EXIT; \
             head -c {n} > \"$t/core.tar\"; [ $(wc -c < \"$t/core.tar\") -eq {n} ]; \
             tar -xf \"$t/core.tar\" -C \"$t\"; b=\"$t/Herdr Core.app/Contents/MacOS/herdr-core\"; \
             \"$b\" --hc-install-bundle \"$b\" \"$d/herdr-core\"; \
             {session}\"$d/herdr-core\" server restart > /dev/null; \"$d/herdr-core\" --version"
        );
        return run_there(paths, to, &format!("sh -c '{script}'"), &exe);
    }
    let guard = if macos {
        "case $(readlink \"$d/herdr-core\" || true) in */Contents/MacOS/herdr-core) \
         echo \"a bundled remote core needs a complete bundle; run hc update on that machine\" >&2; exit 5;; esac; "
    } else {
        ""
    };
    // Into a temp file beside the target, checked whole and running, then swapped.
    let script = format!(
        "set -e; d=$HOME/.local/bin; mkdir -p $d; {guard}rm -f $d/.herdr-core.new.*; t=$d/.herdr-core.new.$$; \
         head -c {n} > $t; [ $(($(wc -c < $t))) -eq {n} ] || {{ rm -f $t; echo \"the copy arrived incomplete\" >&2; exit 3; }}; \
         chmod 755 $t; $t --version > /dev/null || {{ rm -f $t; echo \"this hc does not run there\" >&2; exit 4; }}; \
         mv -f $t $d/herdr-core; command -v hc > /dev/null 2>&1 || [ -e $d/hc ] || ln -s herdr-core $d/hc; \
         {session}$d/herdr-core server restart > /dev/null; $d/herdr-core --version"
    );
    run_there(paths, to, &format!("sh -c '{script}'"), &exe)
}

fn install_payload(bin: &Path, scratch: &Path) -> Result<(Vec<u8>, bool), RpcError> {
    let bundled = bin.ends_with("Herdr Core.app/Contents/MacOS/herdr-core");
    let path = if bundled {
        let root = bin.parent().unwrap().parent().unwrap().parent().unwrap();
        let archive = scratch.join("core-bundle.tar");
        let status = std::process::Command::new("tar")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(root.parent().unwrap())
            .arg("Herdr Core.app")
            .env("COPYFILE_DISABLE", "1")
            .status()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        if !status.success() {
            return Err(RpcError::internal("cannot pack the core bundle"));
        }
        archive
    } else {
        bin.to_path_buf()
    };
    let bytes = std::fs::read(path).map_err(|e| RpcError::internal(e.to_string()))?;
    Ok((bytes, bundled))
}

/// Windows there: this version's release zip and the installer go to its
/// home folder, and the installer runs from them (no download there). It
/// swaps an hc already there the planned way, so its terminals keep running.
fn install_windows(paths: &crate::paths::Paths, target: &str, session: Option<&str>) -> Result<String, RpcError> {
    let scratch = crate::releases::Scratch::new()?;
    let version = env!("CARGO_PKG_VERSION");
    let m = crate::releases::manifest(Some(version), &scratch.0).map_err(|_| {
        RpcError::new(
            "install_failed",
            format!("{target} runs Windows, and hc {version} has no release to download"),
            "install hc there yourself",
        )
    })?;
    let zip = crate::releases::fetch(&m, "windows-x86_64", &scratch.0)?;
    let script = scratch.0.join("hc-install.ps1");
    std::fs::write(&script, include_str!("../../../distribution/install.ps1"))
        .map_err(|e| RpcError::internal(e.to_string()))?;
    std::fs::copy(&zip, scratch.0.join("hc-install.zip")).map_err(|e| RpcError::internal(e.to_string()))?;
    let mut scp = std::process::Command::new("scp");
    scp.arg("-q");
    #[cfg(unix)]
    scp.arg("-o")
        .arg(format!("ControlPath={}", control_path(paths, target).display()))
        .args(["-o", "ControlMaster=no"]);
    scp.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    scp.arg(scratch.0.join("hc-install.zip")).arg(&script).arg(format!("{target}:"));
    let to = ssh_reach(target);
    let out = scp.output().map_err(|e| unreachable(to, &format!("cannot run scp: {e}")))?;
    if !out.status.success() {
        return Err(RpcError::new(
            "install_failed",
            format!("copying hc to {target}: {}", String::from_utf8_lossy(&out.stderr).trim()),
            "nothing was changed there",
        ));
    }
    // No quotes or $ in it: cmd and PowerShell read it the same.
    let session = session.map(|s| format!(" {s}")).unwrap_or_default();
    run_there(
        paths,
        to,
        &format!("powershell -NoProfile -ExecutionPolicy Bypass -File hc-install.ps1 hc-install.zip{session}"),
        b"",
    )?;
    run_there(paths, to, &format!("{WINDOWS_HC} --version"), b"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_comes_from_the_target_host() {
        let default_name = |target| super::default_name(target, |_| false);
        assert_eq!(default_name("rohan").as_deref(), Some("rohan"));
        assert_eq!(default_name("can@Rohan.lan").as_deref(), Some("rohan"));
        assert_eq!(default_name("ssh://can@rohan:2222").as_deref(), Some("rohan"));
        assert_eq!(ssh_target("can@10.0.0.7:4396".into()), "ssh://can@10.0.0.7:4396");
        assert_eq!(ssh_target("rohan:2222".into()), "ssh://rohan:2222");
        for unchanged in ["rohan", "can@rohan", "ssh://can@rohan:2222", "can@fe80::1", "[::1]:22x", "can@rohan:"] {
            assert_eq!(ssh_target(unchanged.into()), unchanged);
        }
        assert_eq!(default_name("root@10.0.0.7").as_deref(), Some("10.0.0.7"));
        assert_eq!(default_name("[::1]"), None);
        assert_eq!(default_name(&format!("{}-extra.example.com", "a".repeat(31))), Some("a".repeat(31)));
        assert_eq!(super::default_name("rohan", |name| name == "rohan"), Some("rohan-2".into()));
        let ssh = |target| ssh_reach(target);
        assert!(check_target(ssh("-oProxyCommand=x")).is_err() && check_target(ssh("a b")).is_err());
        assert!(
            check_target(Reach { target: "fly ssh console -C", via: true, host_key: None, key_file: None }).is_ok()
        );
        assert_eq!(platform("Darwin arm64"), "macos aarch64");
        assert_eq!(platform("Linux x86_64"), "linux x86_64");
        assert_eq!(platform("MINGW64_NT-10.0 x86_64"), "windows x86_64");
        assert_eq!(platform("MSYS_NT-10.0 x86_64"), "windows x86_64");
    }

    #[test]
    fn id_shaped_hosts_get_a_free_name() {
        for host in ["t440s", "t14", &format!("t1{}", "b".repeat(30))] {
            let expected = format!("{}-2", &host[..host.len().min(30)]);
            assert_eq!(default_name(host, |_| false), Some(expected));
            assert_eq!(
                default_name(host, |name| name.ends_with("-2")),
                Some(format!("{}-3", &host[..host.len().min(30)]))
            );
        }
        assert_eq!(default_name("t14!", |_| false), None);
    }

    #[test]
    fn a_pasted_ssh_command_comes_down_to_its_target() {
        let n = |s: &str| normalize_target(s).unwrap();
        assert_eq!(n("ssh -p 2222 can@rohan"), "can@rohan:2222");
        assert_eq!(n("ssh can@rohan -p 2222"), "can@rohan:2222");
        assert_eq!(n("ssh can@rohan"), "can@rohan");
        assert_eq!(n("ssh -p2222 rohan"), "rohan:2222");
        assert_eq!(n("ssh -l can rohan"), "can@rohan");
        assert_eq!(n("ssh -l can -p 2222 rohan"), "can@rohan:2222");
        assert_eq!(n("ssh -p 2222 can@[::1]"), "can@[::1]:2222");
        assert_eq!(n("  ssh   -p   2222   can@rohan  "), "can@rohan:2222");
        assert_eq!(n("ssh can@rohan:2222"), "can@rohan:2222");
        // Not commands: back as they were.
        assert_eq!(n("can@rohan:2222"), "can@rohan:2222");
        assert_eq!(n("ssh://can@rohan:2222"), "ssh://can@rohan:2222");
        assert_eq!(n("rohan"), "rohan");
        assert_eq!(n("sshbox"), "sshbox");
    }

    #[test]
    fn a_pasted_ssh_command_names_what_it_cannot_carry() {
        for (input, named) in [
            ("ssh -i key can@rohan", "-i"),
            ("ssh -J jump can@rohan", "-J"),
            ("ssh -o StrictHostKeyChecking=no can@rohan", "-o"),
            ("ssh --foo can@rohan", "--foo"),
        ] {
            let e = normalize_target(input).unwrap_err();
            assert!(e.message.contains(named), "{input}: {}", e.message);
        }
        assert!(normalize_target("ssh").is_err());
        assert!(normalize_target("ssh -p 2222").is_err());
        assert!(normalize_target("ssh -p x can@rohan").is_err());
        assert!(normalize_target("ssh -p").is_err());
        let e = normalize_target("ssh can@rohan ls").unwrap_err();
        assert!(e.message.contains("ls"), "{}", e.message);
    }

    #[test]
    fn a_known_hosts_line_reads_as_ssh_writes_it() {
        let key = "ssh-ed25519 AAAA";
        assert_eq!(known_hosts_line("rohan", 22, key), "rohan ssh-ed25519 AAAA");
        assert_eq!(known_hosts_line("ssh.instabox.sh", 2222, key), "[ssh.instabox.sh]:2222 ssh-ed25519 AAAA");
        assert_eq!(known_hosts_line("::1", 22, key), "[::1] ssh-ed25519 AAAA");
        assert_eq!(known_hosts_line("::1", 2222, key), "[::1]:2222 ssh-ed25519 AAAA");
    }

    #[test]
    fn a_composed_target_reads_like_an_ssh_target() {
        assert_eq!(compose_target("10.0.0.7", Some("root"), Some(2222)), "ssh://root@10.0.0.7:2222");
        assert_eq!(compose_target("vps.example", Some("can"), None), "can@vps.example");
        assert_eq!(compose_target("vps.example", None, None), "vps.example");
        assert_eq!(compose_target("vps.example", Some(""), Some(22)), "ssh://vps.example:22");
    }

    #[test]
    fn a_windows_box_gets_one_line_cmd_and_powershell_read_alike() {
        let paths = crate::paths::Paths::from_env().unwrap();
        let cmd = pipe(&paths, ssh_reach("winbox"), Some("s1"), true).unwrap();
        let line = cmd.as_std().get_args().last().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            line,
            format!(
                "cmd /d /c \"if exist {WINDOWS_HC} ({WINDOWS_HC} connect --session s1) else (echo {NOT_HERE} 1>&2 & exit /b 127)\""
            )
        );
    }
}
