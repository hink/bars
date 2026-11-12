//! Linux: mutes each other app's playback stream on PulseAudio, or on
//! PipeWire through its PulseAudio server, the way a volume mixer does.
//! Bars' own sounds (cpal's client) are left alone.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, anyhow};
use pulseaudio::protocol::{self, Command, Prop, Props};

use crate::platform::mute::{Backend, Muted};

/// A server this slow to answer is given up on.
const TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Streams {
    server: Option<Server>,
    /// Streams seen since the last unmute (muted or not, they're left be).
    seen: Vec<u32>,
    muted: Vec<u32>,
    /// The server couldn't be reached (said once per dictation).
    warned: bool,
}

struct Stream {
    index: u32,
    /// Its app, the same from one run to the next.
    app: String,
    muted: bool,
}

impl Streams {
    fn server(&mut self) -> Option<&mut Server> {
        if self.server.is_none() {
            match Server::connect() {
                Ok(server) => self.server = Some(server),
                Err(e) if !self.warned => {
                    self.warned = true;
                    log::warn!("couldn't reach the sound server to mute other apps: {e:#}");
                }
                Err(_) => {}
            }
        }
        self.server.as_mut()
    }

    /// Runs `f` on the server, reconnecting next time if it fails.
    fn with_server<T>(&mut self, f: impl FnOnce(&mut Server) -> anyhow::Result<T>) -> Option<T> {
        let result = f(self.server()?);
        result.map_err(|e| log::warn!("muting other apps: {e:#}")).inspect_err(|_| self.server = None).ok()
    }
}

impl Backend for Streams {
    fn mute(&mut self) -> Vec<Muted> {
        let Some(streams) = self.with_server(Server::streams) else { return Vec::new() };
        let fresh: Vec<Stream> = streams.into_iter().filter(|s| !self.seen.contains(&s.index)).collect();
        let mut new = Vec::new();
        for s in fresh {
            self.seen.push(s.index);
            if !s.muted && self.with_server(|server| server.set_mute(s.index, true)).is_some() {
                self.muted.push(s.index);
                new.push(Muted { id: s.app, volume: None });
            }
        }
        new
    }

    fn unmute(&mut self) {
        let muted = std::mem::take(&mut self.muted);
        if !muted.is_empty() {
            // Only those still muted (a stream that ended is gone).
            let streams = self.with_server(Server::streams).unwrap_or_default();
            for s in streams.into_iter().filter(|s| s.muted && muted.contains(&s.index)) {
                self.with_server(|server| server.set_mute(s.index, false));
            }
        }
        self.seen.clear();
        self.server = None;
        self.warned = false;
    }

    fn recover(&mut self, muted: &[Muted]) -> Vec<Muted> {
        let streams = self.with_server(Server::streams).unwrap_or_default();
        for s in streams.into_iter().filter(|s| s.muted && muted.iter().any(|m| m.id == s.app)) {
            self.with_server(|server| server.set_mute(s.index, false));
        }
        self.server = None;
        Vec::new()
    }
}

/// A connection to the sound server, spoken to one request at a time.
struct Server {
    socket: BufReader<UnixStream>,
    seq: u32,
    version: u16,
}

impl Server {
    fn connect() -> anyhow::Result<Server> {
        let path = pulseaudio::socket_path_from_env().ok_or_else(|| anyhow!("no PulseAudio socket"))?;
        let cookie = pulseaudio::cookie_path_from_env().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
        let socket = UnixStream::connect(&path).with_context(|| path.display().to_string())?;
        socket.set_read_timeout(Some(TIMEOUT))?;
        socket.set_write_timeout(Some(TIMEOUT))?;
        let mut server = Server { socket: BufReader::new(socket), seq: 0, version: protocol::MAX_VERSION };
        let auth: protocol::AuthReply = server.ask(Command::Auth(protocol::AuthParams {
            version: protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie,
        }))?;
        server.version = server.version.min(auth.version);
        let mut props = Props::new();
        props.set(Prop::ApplicationName, c"Bars");
        let _: protocol::SetClientNameReply = server.ask(Command::SetClientName(props))?;
        Ok(server)
    }

    fn send(&mut self, cmd: &Command) -> anyhow::Result<()> {
        self.seq += 1;
        protocol::write_command_message(self.socket.get_mut(), self.seq, cmd, self.version)?;
        Ok(())
    }

    fn ask<R: protocol::CommandReply>(&mut self, cmd: Command) -> anyhow::Result<R> {
        self.send(&cmd)?;
        let (seq, reply) = protocol::read_reply_message(&mut self.socket, self.version)?;
        anyhow::ensure!(seq == self.seq, "the sound server answered out of turn");
        Ok(reply)
    }

    fn set_mute(&mut self, index: u32, mute: bool) -> anyhow::Result<()> {
        self.send(&Command::SetSinkInputMute(protocol::SetStreamMuteParams { index, mute }))?;
        let seq = protocol::read_ack_message(&mut self.socket)?;
        anyhow::ensure!(seq == self.seq, "the sound server answered out of turn");
        Ok(())
    }

    /// Every app's playback stream but Bars' own.
    fn streams(&mut self) -> anyhow::Result<Vec<Stream>> {
        // cpal names its client after the process; Bars plays its sounds there.
        let ours = format!("cpal-pulseaudio-{}", std::process::id());
        let clients: protocol::ClientInfoList = self.ask(Command::GetClientInfoList)?;
        let own: Vec<u32> = clients.iter().filter(|c| c.name.to_bytes() == ours.as_bytes()).map(|c| c.index).collect();
        let inputs: protocol::SinkInputInfoList = self.ask(Command::GetSinkInputInfoList)?;
        Ok(inputs
            .into_iter()
            .filter(|s| !s.client_index.is_some_and(|c| own.contains(&c)))
            .map(|s| Stream {
                index: s.index,
                app: text(&s.props, Prop::ApplicationProcessBinary)
                    .or_else(|| text(&s.props, Prop::ApplicationName))
                    .unwrap_or_else(|| s.name.to_string_lossy().into_owned()),
                muted: s.muted,
            })
            .collect())
    }
}

/// A text property (stored with its terminating NUL).
fn text(props: &Props, prop: Prop) -> Option<String> {
    let bytes = props.get(prop)?;
    Some(String::from_utf8_lossy(bytes.strip_suffix(&[0]).unwrap_or(bytes)).into_owned()).filter(|s| !s.is_empty())
}
