//! The embedded ssh transport (russh), for machines a person adds with a
//! password: no ssh program is spawned, so the password and the host-key
//! decision pass through hc itself instead of a terminal. The contract is
//! the same as the ssh pipe in machines.rs: a byte pipe to
//! `herdr-core connect` there.
//!
//! Trust is TOFU: the first connection to a host stops with a `host_key`
//! error carrying the key's SHA256 fingerprint; only a caller that passes
//! that fingerprint back as trusted gets in, and it is pinned in the
//! machine's record (machines.rs). A password is used once, in memory, to
//! plant a per-machine key; later connections authenticate with that key.

use crate::rpc::RpcError;
use russh::client::{AuthResult, Config, Handle, Handler};
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg};
use russh::{MethodKind, MethodSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// An ssh target taken apart: `ssh://can@rohan:2222`, `can@rohan`, `rohan`.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
}

pub fn parse_target(target: &str) -> Result<Target, RpcError> {
    let t = target.strip_prefix("ssh://").unwrap_or(target);
    let (user, hostport) = match t.rsplit_once('@') {
        Some((user, host)) if !user.is_empty() => (Some(user.to_string()), host),
        _ => (None, t),
    };
    let bad = || RpcError::invalid_params(format!("bad ssh target {target:?}"));
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        // An IPv6 literal: [::1] or [::1]:2222.
        let Some((host, after)) = rest.split_once(']') else { return Err(bad()) };
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| bad())?,
            None if after.is_empty() => 22,
            None => return Err(bad()),
        };
        (host.to_string(), port)
    } else {
        match hostport.split_once(':') {
            Some((host, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                (host.to_string(), p.parse().map_err(|_| bad())?)
            }
            _ => (hostport.to_string(), 22),
        }
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok(Target { user, host, port })
}

/// A server key's identity: its SHA256 fingerprint (`SHA256:…`, the form
/// ssh itself prints) and its algorithm name.
fn key_identity(key: &PublicKeyOrCertificate) -> (String, String) {
    let public: russh::keys::PublicKey = match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
        PublicKeyOrCertificate::Certificate(cert) => cert.public_key().clone().into(),
    };
    (public.fingerprint(russh::keys::ssh_key::HashAlg::Sha256).to_string(), public.algorithm().to_string())
}

/// Captures the server key and lets it through only when it is the pinned
/// (or just-confirmed) one: the TOFU gate.
struct HostKey {
    expected: Option<String>,
    seen: Arc<Mutex<Option<(String, String)>>>,
}

impl Handler for HostKey {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let identity = key_identity(key);
        let trusted = self.expected.as_deref() == Some(identity.0.as_str());
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = Some(identity);
        Ok(trusted)
    }
}

fn host_key_error(target: &Target, fingerprint: &str, algo: &str, pinned: bool) -> RpcError {
    if pinned {
        RpcError::new(
            "host_key",
            format!("{} presents host key {fingerprint} ({algo}), not the pinned one", target.host),
            "if the server was rebuilt this is expected: remove the machine and add it again to pin the new key",
        )
    } else {
        RpcError::new(
            "host_key",
            format!("{}'s host key is not trusted yet: {fingerprint} ({algo})", target.host),
            "check the fingerprint against the server, then add again with --trust-host-key <fingerprint>",
        )
    }
}

/// Whether the server will take a password at all: it offers the password
/// method, or keyboard-interactive (which usually asks for the password
/// through PAM). A failure offering neither means password logins are
/// switched off there — a different problem from a wrong password, and the
/// caller hears about it differently.
fn password_login_offered(methods: &MethodSet) -> bool {
    methods.contains(&MethodKind::Password) || methods.contains(&MethodKind::KeyboardInteractive)
}

/// The one command that turns password login on for just `user` there: a
/// Match block in sshd_config.d (picked up by the sshd_config Include most
/// distros ship), then a reload — the service is `ssh` on Debian/Ubuntu,
/// `sshd` on RHEL-style systems, so try both. Run once, with sudo, on the
/// server itself.
pub fn enable_password_command(user: &str) -> String {
    format!(
        "printf 'Match User {user}\\n    PasswordAuthentication yes\\n' | \
         sudo tee /etc/ssh/sshd_config.d/99-fleet-{user}.conf >/dev/null && \
         (sudo systemctl reload ssh 2>/dev/null || sudo systemctl reload sshd)"
    )
}

fn password_auth_disabled_error(target: &Target, user: &str) -> RpcError {
    RpcError::new(
        "password_auth_disabled",
        format!(
            "{} does not accept password logins for {user}: password authentication is turned off \
             on that server, so the password was never checked — this is not a wrong password",
            target.host
        ),
        format!("run this once on {} (it needs sudo), then add again:\n{}", target.host, enable_password_command(user)),
    )
}

/// How a session authenticates. A password is never stored; it plants a key
/// (see `new_key`) and the key is used from then on.
pub enum Auth {
    Password(String),
    Key(PrivateKey),
}

/// An authenticated embedded-ssh session to one machine.
pub struct Session {
    handle: Handle<HostKey>,
    /// The server key's SHA256 fingerprint, as presented and accepted.
    pub fingerprint: String,
}

pub async fn open(target: &str, auth: Auth, expected: Option<&str>) -> Result<Session, RpcError> {
    let t = parse_target(target)?;
    let user = t
        .user
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| RpcError::invalid_params("the target names no user: give one as user@host or --user"))?;
    let seen = Arc::new(Mutex::new(None));
    let handler = HostKey { expected: expected.map(str::to_string), seen: seen.clone() };
    let config = Config { keepalive_interval: Some(Duration::from_secs(15)), keepalive_max: 4, ..Default::default() };
    let mut handle = match russh::client::connect(Arc::new(config), (t.host.as_str(), t.port), handler).await {
        Ok(handle) => handle,
        Err(e) => {
            // A refused host key is a decision for a person, not a failure.
            if let Some((fingerprint, algo)) = seen.lock().unwrap_or_else(|p| p.into_inner()).clone() {
                return Err(host_key_error(&t, &fingerprint, &algo, expected.is_some()));
            }
            return Err(RpcError::new(
                "unreachable",
                format!("cannot reach {}: {e}", t.host),
                "check the host and port, and that ssh is listening there",
            ));
        }
    };
    let result = match &auth {
        Auth::Password(password) => handle.authenticate_password(user.as_str(), password.as_str()).await,
        Auth::Key(key) => {
            handle.authenticate_publickey(user.as_str(), PrivateKeyWithHashAlg::new(Arc::new(key.clone()), None)).await
        }
    };
    match result {
        Ok(AuthResult::Success) => {}
        Ok(AuthResult::Failure { remaining_methods, .. }) => {
            return Err(match &auth {
                Auth::Password(_) if !password_login_offered(&remaining_methods) => {
                    password_auth_disabled_error(&t, &user)
                }
                Auth::Password(_) => RpcError::new(
                    "auth_failed",
                    format!("{} refused that password for {user}", t.host),
                    "check the user and password, then add again",
                ),
                Auth::Key(_) => RpcError::new(
                    "needs_login",
                    format!("{} refused the planted key for {user}", t.host),
                    "remove the machine and add it again with its password",
                ),
            });
        }
        Err(e) => {
            return Err(RpcError::new("unreachable", format!("the login to {} broke off: {e}", t.host), ""));
        }
    }
    let fingerprint =
        seen.lock().unwrap_or_else(|p| p.into_inner()).clone().map(|(fingerprint, _)| fingerprint).unwrap_or_default();
    Ok(Session { handle, fingerprint })
}

/// Output kept from one `exec`: enough for a script's verdict, not a stream.
const EXEC_CAP: usize = 1 << 20;

impl Session {
    /// Runs one line there to completion: (exit status, stdout, stderr).
    pub async fn exec(&self, line: &str) -> Result<(u32, Vec<u8>, Vec<u8>), RpcError> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| RpcError::new("unreachable", format!("cannot open a session: {e}"), ""))?;
        channel
            .exec(true, line)
            .await
            .map_err(|e| RpcError::new("unreachable", format!("cannot run a command there: {e}"), ""))?;
        let (mut out, mut err, mut status, mut eof) = (Vec::new(), Vec::new(), None, false);
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } if out.len() < EXEC_CAP => {
                    out.extend_from_slice(&data[..data.len().min(EXEC_CAP - out.len())]);
                }
                ChannelMsg::ExtendedData { data, ext: 1 } if err.len() < EXEC_CAP => {
                    err.extend_from_slice(&data[..data.len().min(EXEC_CAP - err.len())]);
                }
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::Eof => eof = true,
                ChannelMsg::Close => break,
                _ => {}
            }
            if status.is_some() && eof {
                break;
            }
        }
        Ok((status.unwrap_or(255), out, err))
    }

    /// Turns the session into the byte pipe to the core there: `line` is the
    /// remote command (machines.rs's connect line). The connection lives as
    /// long as the returned pipe's task does.
    pub async fn pipe(self, line: &str) -> Result<Pipe, RpcError> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| RpcError::new("unreachable", format!("cannot open a session: {e}"), ""))?;
        channel
            .exec(true, line)
            .await
            .map_err(|e| RpcError::new("unreachable", format!("cannot start hc there: {e}"), ""))?;
        let writer = channel.make_writer();
        let (in_write, in_read) = tokio::io::duplex(64 * 1024);
        let (out_write, out_read) = tokio::io::duplex(256 * 1024);
        let (err_write, err_read) = tokio::io::duplex(64 * 1024);
        let exit = Arc::new(Mutex::new(None));
        let stderr_seen = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(pump(Pump {
            handle: self.handle,
            channel,
            writer: Box::new(writer),
            input: in_read,
            out: out_write,
            err: err_write,
            exit: exit.clone(),
            stderr_seen: stderr_seen.clone(),
        }));
        Ok(Pipe {
            stdin: Box::new(in_write),
            stdout: Box::new(out_read),
            stderr: Box::new(err_read),
            task,
            exit,
            stderr_seen,
        })
    }
}

/// The byte pipe to the core there, as machines.rs's `Dialed` carries it.
pub struct Pipe {
    pub stdin: Box<dyn AsyncWrite + Send + Unpin>,
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    pub stderr: Box<dyn AsyncRead + Send + Unpin>,
    /// Owns the connection and the channel pump; aborting it hangs up.
    pub task: tokio::task::JoinHandle<()>,
    /// The remote command's exit status, once it exits.
    pub exit: Arc<Mutex<Option<u32>>>,
    /// What the remote command printed to stderr (bounded), for the error
    /// classification machines.rs does when a dial fails.
    pub stderr_seen: Arc<Mutex<Vec<u8>>>,
}

struct Pump {
    // Kept alive for the pipe's life: dropping the handle disconnects.
    #[allow(dead_code)]
    handle: Handle<HostKey>,
    channel: Channel<russh::client::Msg>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    input: tokio::io::DuplexStream,
    out: tokio::io::DuplexStream,
    err: tokio::io::DuplexStream,
    exit: Arc<Mutex<Option<u32>>>,
    stderr_seen: Arc<Mutex<Vec<u8>>>,
}

/// stderr kept for error classification.
const STDERR_CAP: usize = 64 * 1024;

/// Shuttles bytes between the channel and the pipe's three streams until
/// either side closes.
async fn pump(p: Pump) {
    let Pump { handle: _handle, mut channel, mut writer, mut input, mut out, mut err, exit, stderr_seen } = p;
    let mut buf = [0u8; 16 * 1024];
    let mut input_open = true;
    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        if out.write_all(&data).await.is_err() { break; }
                    }
                    Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                        {
                            let mut seen = stderr_seen.lock().unwrap_or_else(|x| x.into_inner());
                            let room = STDERR_CAP.saturating_sub(seen.len());
                            seen.extend_from_slice(&data[..data.len().min(room)]);
                        }
                        if err.write_all(&data).await.is_err() { break; }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        *exit.lock().unwrap_or_else(|x| x.into_inner()) = Some(exit_status);
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                    Some(_) => {}
                }
            }
            n = input.read(&mut buf), if input_open => {
                match n {
                    Ok(0) => {
                        input_open = false;
                        let _ = channel.eof().await;
                    }
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).await.is_err() { break; }
                        if writer.flush().await.is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

/// A fresh ed25519 key pair (the private half is stored by machines.rs;
/// the public half is planted on the machine).
pub fn new_key_pair() -> Result<PrivateKey, RpcError> {
    let mut rng = russh::keys::ssh_key::rand_core::UnwrapErr(getrandom::SysRng);
    PrivateKey::random(&mut rng, Algorithm::Ed25519).map_err(|e| RpcError::internal(format!("cannot make a key: {e}")))
}

/// A stored private key, back from its OpenSSH text.
pub fn load_key(text: &str) -> Result<PrivateKey, RpcError> {
    PrivateKey::from_openssh(text.trim()).map_err(|e| RpcError::internal(format!("the stored key does not parse: {e}")))
}

pub fn secret_text(key: &PrivateKey) -> Result<String, RpcError> {
    key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .map(|s| s.to_string())
        .map_err(|e| RpcError::internal(format!("cannot write the key: {e}")))
}

/// `ssh-ed25519 AAAA… herdr-core`: the line authorized_keys takes.
pub fn public_line(key: &PrivateKey) -> Result<String, RpcError> {
    key.public_key()
        .to_openssh()
        .map(|line| format!("{line} herdr-core"))
        .map_err(|e| RpcError::internal(format!("cannot write the public key: {e}")))
}

/// The remote line that plants `public_line` in ~/.ssh/authorized_keys,
/// once (safe to run again: it changes nothing the second time).
pub fn plant_line(public_line: &str) -> String {
    format!(
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && \
         {{ grep -qxF '{public_line}' ~/.ssh/authorized_keys || echo '{public_line}' >> ~/.ssh/authorized_keys; }} && \
         chmod 600 ~/.ssh/authorized_keys"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_come_apart() {
        let t = |s: &str| parse_target(s).unwrap();
        assert_eq!(t("rohan"), Target { user: None, host: "rohan".into(), port: 22 });
        assert_eq!(t("can@rohan"), Target { user: Some("can".into()), host: "rohan".into(), port: 22 });
        assert_eq!(t("ssh://can@rohan:2222"), Target { user: Some("can".into()), host: "rohan".into(), port: 2222 });
        assert_eq!(t("can@10.0.0.7:4396"), Target { user: Some("can".into()), host: "10.0.0.7".into(), port: 4396 });
        assert_eq!(t("[::1]:2222"), Target { user: None, host: "::1".into(), port: 2222 });
        assert_eq!(t("can@[fe80::1]"), Target { user: Some("can".into()), host: "fe80::1".into(), port: 22 });
        // A non-numeric port is part of the host name to ssh; here it is a bad target.
        assert_eq!(t("rohan:22x").host, "rohan:22x");
        assert!(parse_target("").is_err() && parse_target("can@").is_err() && parse_target("[::1").is_err());
    }

    #[test]
    fn password_login_is_offered_only_when_the_server_takes_passwords() {
        let offered = |kinds: &[MethodKind]| password_login_offered(&MethodSet::from(kinds));
        assert!(offered(&[MethodKind::Password]));
        assert!(offered(&[MethodKind::KeyboardInteractive]));
        assert!(offered(&[MethodKind::PublicKey, MethodKind::Password]));
        assert!(!offered(&[MethodKind::PublicKey]));
        assert!(!offered(&[]));
    }

    #[test]
    fn the_enable_password_command_names_the_user_and_both_services() {
        let cmd = enable_password_command("can");
        assert!(cmd.contains("Match User can"), "{cmd}");
        assert!(cmd.contains("PasswordAuthentication yes"), "{cmd}");
        assert!(cmd.contains("/etc/ssh/sshd_config.d/99-fleet-can.conf"), "{cmd}");
        assert!(cmd.contains("systemctl reload ssh"), "{cmd}");
        assert!(cmd.contains("systemctl reload sshd"), "{cmd}");
        assert!(!cmd.contains('\n'), "one line, copyable whole: {cmd}");
    }

    #[test]
    fn a_server_without_password_login_gets_its_own_error() {
        let t = parse_target("can@rohan").unwrap();
        let e = password_auth_disabled_error(&t, "can");
        assert_eq!(e.kind, "password_auth_disabled");
        assert!(e.message.contains("rohan") && e.message.contains("can"), "{}", e.message);
        // The command rides the hint, alone on its last line, so a person
        // (or the app) can lift it out whole.
        assert_eq!(e.hint.lines().last(), Some(enable_password_command("can").as_str()), "{}", e.hint);
    }

    #[test]
    fn a_plant_line_quotes_the_key_once() {
        let line = plant_line("ssh-ed25519 AAAA test");
        assert_eq!(line.matches("ssh-ed25519 AAAA test").count(), 2, "grep once, echo once: {line}");
        assert!(line.contains("grep -qxF"), "{line}");
    }
}
