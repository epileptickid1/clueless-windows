//! Speech-to-text through Deepgram's streaming API. Audio is sent while it is
//! being spoken, so words appear well under a second behind the speaker.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{HandshakeError, Message, WebSocket};

use crate::audio::Speaker;
use crate::event::Event;

pub const URL: &str = "wss://api.deepgram.com/v1/listen";

/// Frames kept while the connection is down: one minute of 30 ms frames.
const BACKLOG: usize = 2000;
/// Cut a line this long even if the speaker never pauses.
const LONG_LINE: usize = 400;

#[derive(Clone)]
pub struct Config {
    pub url: String,
    pub key: String,
    /// A Deepgram language code, or `multi`.
    pub language: String,
}

impl Config {
    fn address(&self) -> String {
        // `endpointing` ends a line after a short pause. Whether a question is
        // complete is decided later, so splitting early costs nothing.
        format!(
            "{}?model=nova-3&language={}&encoding=linear16&sample_rate=16000&channels=1\
             &interim_results=true&smart_format=true&endpointing=400&utterance_end_ms=1000",
            self.url, self.language
        )
    }
}

#[derive(Debug, PartialEq)]
pub enum Update {
    /// The line being spoken so far; empty once it has ended.
    Partial(String),
    /// A finished line.
    Line(String),
}

/// Builds lines out of Deepgram's interim and final results.
#[derive(Default)]
pub struct Assembler {
    finals: String,
    interim: String,
    shown: String,
}

impl Assembler {
    /// Words have arrived that are not part of a finished line yet.
    pub fn pending(&self) -> bool {
        !self.finals.is_empty() || !self.interim.is_empty()
    }

    pub fn push(&mut self, message: &str) -> Vec<Update> {
        let Ok(v) = serde_json::from_str::<Value>(message) else {
            return Vec::new();
        };
        match v["type"].as_str() {
            Some("Results") => {
                let text = v["channel"]["alternatives"][0]["transcript"]
                    .as_str()
                    .unwrap_or("")
                    .trim();
                if v["is_final"].as_bool() != Some(true) {
                    self.interim = text.to_string();
                    return self.partial();
                }
                self.interim.clear();
                if !text.is_empty() {
                    if !self.finals.is_empty() {
                        self.finals.push(' ');
                    }
                    self.finals.push_str(text);
                }
                let ended = v["speech_final"].as_bool() == Some(true) || v["from_finalize"].as_bool() == Some(true);
                if ended || self.finals.len() > LONG_LINE {
                    self.flush()
                } else {
                    self.partial()
                }
            }
            // Noise can hide the pause from `speech_final`; the gap between words does not.
            Some("UtteranceEnd") if self.interim.is_empty() => self.flush(),
            _ => Vec::new(),
        }
    }

    /// Ends the current line, keeping words that were never marked final.
    pub fn flush(&mut self) -> Vec<Update> {
        let mut line = std::mem::take(&mut self.finals);
        let rest = std::mem::take(&mut self.interim);
        if !rest.is_empty() {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&rest);
        }
        let mut updates = self.partial();
        if line.chars().any(char::is_alphanumeric) {
            updates.push(Update::Line(line));
        }
        updates
    }

    fn partial(&mut self) -> Vec<Update> {
        let now = [self.finals.as_str(), self.interim.as_str()]
            .join(" ")
            .trim()
            .to_string();
        if now == self.shown {
            return Vec::new();
        }
        self.shown.clone_from(&now);
        vec![Update::Partial(now)]
    }
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

enum ConnectError {
    /// Trying again cannot help.
    Fatal(String),
    Retry(String),
}

fn connect(config: &Config) -> Result<Socket, ConnectError> {
    use ConnectError::{Fatal, Retry};

    let mut request = config
        .address()
        .into_client_request()
        .map_err(|e| Fatal(format!("Invalid Deepgram address: {e}")))?;
    let mut auth = HeaderValue::from_str(&format!("Token {}", config.key))
        .map_err(|_| Fatal("The Deepgram API key contains invalid characters.".into()))?;
    auth.set_sensitive(true);
    request.headers_mut().insert("Authorization", auth);

    let uri = request.uri();
    let host = uri
        .host()
        .ok_or_else(|| Fatal("The Deepgram address has no host.".into()))?;
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("ws") { 80 } else { 443 });
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|e| Retry(format!("cannot resolve {host}: {e}")))?;
    let mut tcp = Err(Retry(format!("no address for {host}")));
    for address in addresses {
        match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            Ok(stream) => {
                tcp = Ok(stream);
                break;
            }
            Err(e) => tcp = Err(Retry(format!("cannot connect: {e}"))),
        }
    }
    let tcp = tcp?;
    let io = |e: std::io::Error| Retry(e.to_string());
    tcp.set_nodelay(true).map_err(io)?;
    tcp.set_read_timeout(Some(Duration::from_secs(10))).map_err(io)?;
    tcp.set_write_timeout(Some(Duration::from_secs(10))).map_err(io)?;

    let (socket, _) = tungstenite::client_tls(request, tcp).map_err(|e| match e {
        HandshakeError::Failure(tungstenite::Error::Http(response)) => {
            let status = response.status().as_u16();
            let body = response
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            match status {
                401 | 403 => Fatal(format!(
                    "Deepgram rejected the API key (HTTP {status}). Check the key and its permissions."
                )),
                402 => Fatal("The Deepgram account has no credit left (HTTP 402).".into()),
                400 => Fatal(format!("Deepgram refused the request: {body}")),
                _ => Retry(format!("Deepgram answered HTTP {status}")),
            }
        }
        e => Retry(e.to_string()),
    })?;
    // From here on reads return quickly, so one thread can send and receive.
    // On Windows, changing the timeout on a cloned socket leaves this socket unchanged.
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::NativeTls(tls) => tls.get_ref(),
        _ => return Err(Fatal("Unsupported TLS transport for Deepgram.".into())),
    };
    tcp.set_read_timeout(Some(Duration::from_millis(20))).map_err(io)?;
    Ok(socket)
}

struct Stream {
    audio: Receiver<Vec<u8>>,
    /// Frames not yet delivered.
    backlog: VecDeque<Vec<u8>>,
    /// The capture is still running.
    open: bool,
    lines: Assembler,
    generation: u64,
    speaker: Speaker,
    events: Sender<Event>,
}

impl Stream {
    fn keep(&mut self, frame: Vec<u8>) {
        if self.backlog.len() >= BACKLOG {
            self.backlog.pop_front();
        }
        self.backlog.push_back(frame);
    }

    fn emit(&self, updates: Vec<Update>) {
        for update in updates {
            let _ = self.events.send(match update {
                Update::Partial(text) => Event::Partial(self.generation, self.speaker, text),
                Update::Line(text) => Event::Line(self.speaker, text),
            });
        }
    }

    /// Runs one connection. `Ok` means the capture ended and everything was heard.
    fn session(&mut self, mut socket: Socket) -> Result<(), String> {
        let mut sent = Instant::now();
        let mut audio_sent = Instant::now();
        let mut finalized = false;
        let mut closing: Option<Instant> = None;
        let control = |socket: &mut Socket, kind: &str| {
            socket
                .send(Message::text(format!(r#"{{"type":"{kind}"}}"#)))
                .map_err(|e| e.to_string())
        };
        loop {
            if let Some(since) = closing {
                if since.elapsed() >= Duration::from_secs(3) {
                    return Ok(());
                }
            } else {
                loop {
                    match self.audio.try_recv() {
                        Ok(frame) => self.keep(frame),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            self.open = false;
                            break;
                        }
                    }
                }
                while let Some(frame) = self.backlog.front() {
                    socket.send(Message::binary(frame.clone())).map_err(|e| e.to_string())?;
                    self.backlog.pop_front();
                    sent = Instant::now();
                    audio_sent = sent;
                    finalized = false;
                }
                if !self.open {
                    control(&mut socket, "CloseStream")?;
                    closing = Some(Instant::now());
                } else if !finalized && self.lines.pending() && audio_sent.elapsed() >= Duration::from_secs(1) {
                    // A paused player delivers no audio, so ask for the last words.
                    control(&mut socket, "Finalize")?;
                    finalized = true;
                    sent = Instant::now();
                } else if sent.elapsed() >= Duration::from_secs(3) {
                    control(&mut socket, "KeepAlive")?;
                    sent = Instant::now();
                }
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let updates = self.lines.push(&text);
                    self.emit(updates);
                }
                Ok(Message::Close(frame)) => {
                    return match closing {
                        Some(_) => Ok(()),
                        None => Err(frame.map_or_else(|| "connection closed".into(), |f| f.reason.to_string())),
                    };
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => {
                    return match closing {
                        Some(_) => Ok(()),
                        None => Err(e.to_string()),
                    };
                }
            }
        }
    }

    fn run(mut self, config: Config) {
        let mut delay = Duration::from_secs(1);
        let mut failed = false;
        let mut attempts_after_end = 0;
        loop {
            let problem = match connect(&config) {
                Ok(socket) => {
                    if failed {
                        let _ = self
                            .events
                            .send(Event::SttStatus(self.generation, "Reconnected to Deepgram.".into()));
                    }
                    let connected = Instant::now();
                    let result = self.session(socket);
                    // Keep backing off if connections are dropped straight away.
                    if connected.elapsed() >= Duration::from_secs(30) {
                        delay = Duration::from_secs(1);
                    }
                    let updates = self.lines.flush();
                    self.emit(updates);
                    match result {
                        Ok(()) => return,
                        Err(e) => e,
                    }
                }
                Err(ConnectError::Fatal(message)) => {
                    let _ = self.events.send(Event::SttFailed(self.generation, message));
                    return;
                }
                Err(ConnectError::Retry(message)) => message,
            };
            failed = true;
            let _ = self.events.send(Event::SttStatus(
                self.generation,
                format!("Deepgram connection problem, retrying: {problem}"),
            ));
            // Wait before retrying, and keep what is said meanwhile.
            let until = Instant::now() + delay;
            while self.open {
                let Some(left) = until.checked_duration_since(Instant::now()) else {
                    break;
                };
                match self.audio.recv_timeout(left) {
                    Ok(frame) => self.keep(frame),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => self.open = false,
                }
            }
            if !self.open {
                attempts_after_end += 1;
                if self.backlog.is_empty() || attempts_after_end > 2 {
                    return;
                }
            }
            delay = (delay * 2).min(Duration::from_secs(8));
        }
    }
}

/// Starts a transcription stream for 16 kHz mono s16le audio. Dropping the
/// returned sender finishes the stream after the last words have arrived.
pub fn spawn(config: Config, generation: u64, speaker: Speaker, events: Sender<Event>) -> Sender<Vec<u8>> {
    let (tx, audio) = channel();
    let stream = Stream {
        audio,
        backlog: VecDeque::new(),
        open: true,
        lines: Assembler::default(),
        generation,
        speaker,
        events,
    };
    thread::spawn(move || stream.run(config));
    tx
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn results(text: &str, is_final: bool, speech_final: bool) -> String {
        serde_json::json!({
            "type": "Results", "is_final": is_final, "speech_final": speech_final,
            "channel": {"alternatives": [{"transcript": text}]}
        })
        .to_string()
    }

    fn partial(t: &str) -> Update {
        Update::Partial(t.into())
    }

    fn line(t: &str) -> Update {
        Update::Line(t.into())
    }

    #[test]
    fn assembles_lines() {
        let mut a = Assembler::default();
        assert!(a.push(&results("", false, false)).is_empty());
        assert_eq!(a.push(&results("how would", false, false)), [partial("how would")]);
        assert_eq!(
            a.push(&results("How would you", true, false)),
            [partial("How would you")]
        );
        assert_eq!(
            a.push(&results("design", false, false)),
            [partial("How would you design")]
        );
        assert_eq!(
            a.push(&results("design it?", true, true)),
            [partial(""), line("How would you design it?")]
        );
        assert!(!a.pending());
        assert!(a.push(&results("", true, true)).is_empty());
    }

    #[test]
    fn ends_lines_without_speech_final() {
        let mut a = Assembler::default();
        a.push(&results("One.", true, false));
        assert_eq!(a.push(r#"{"type":"UtteranceEnd"}"#), [partial(""), line("One.")]);
        // An empty final result with `speech_final` closes what came before.
        a.push(&results("Two.", true, false));
        assert_eq!(a.push(&results("", true, true)), [partial(""), line("Two.")]);
        // Words that never became final are not lost.
        a.push(&results("three", false, false));
        assert!(a.push(r#"{"type":"UtteranceEnd"}"#).is_empty());
        assert_eq!(a.flush(), [partial(""), line("three")]);
        assert!(a.push("garbage").is_empty());
    }

    /// Reads messages until one satisfies `wanted`.
    fn until(socket: &mut WebSocket<TcpStream>, wanted: impl Fn(&Message) -> bool) {
        while !wanted(&socket.read().expect("client message")) {}
    }

    #[test]
    fn streams_and_reconnects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/listen", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            // The first connection drops in the middle of a line.
            let mut first = tungstenite::accept(listener.accept().unwrap().0).unwrap();
            until(&mut first, Message::is_binary);
            first.send(Message::text(results("What is", false, false))).unwrap();
            drop(first);
            let mut second = tungstenite::accept(listener.accept().unwrap().0).unwrap();
            second.send(Message::text(results("Rust?", true, false))).unwrap();
            until(
                &mut second,
                |m| matches!(m, Message::Text(t) if t.contains("CloseStream")),
            );
            second.close(None).unwrap();
            while second.read().is_ok() {}
        });
        let (events, received) = channel();
        let config = Config {
            url,
            key: "test".into(),
            language: "en".into(),
        };
        let audio = spawn(config, 1, Speaker::Them, events);
        audio.send(vec![0; 960]).unwrap();
        let mut lines = Vec::new();
        let mut audio = Some(audio);
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Ok(event) = received.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            match event {
                Event::Line(_, text) => lines.push(text),
                // The second connection is up: stop capturing.
                Event::Partial(_, _, text) if text == "Rust?" => audio = None,
                Event::SttFailed(_, e) => panic!("{e}"),
                _ => {}
            }
        }
        assert!(audio.is_none());
        assert_eq!(lines, ["What is", "Rust?"]);
        server.join().unwrap();
    }

    /// Talks to the real service:
    /// `CLUELESS_TEST_AUDIO=clip.raw CLUELESS_TEST_LANGUAGE=uk cargo test live -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a Deepgram key and an audio file"]
    fn live() {
        let audio = std::fs::read(std::env::var("CLUELESS_TEST_AUDIO").unwrap()).unwrap();
        let config = Config {
            url: URL.into(),
            key: crate::config::deepgram_key().expect("no key"),
            language: std::env::var("CLUELESS_TEST_LANGUAGE").unwrap_or_else(|_| "multi".into()),
        };
        let (events, received) = channel();
        let sender = spawn(config, 1, Speaker::Them, events);
        let start = Instant::now();
        thread::spawn(move || {
            for (i, frame) in audio.chunks(960).enumerate() {
                let due = Duration::from_millis(30 * (i as u64 + 1));
                thread::sleep(due.saturating_sub(start.elapsed()));
                sender.send(frame.to_vec()).unwrap();
            }
        });
        let mut lines = 0;
        while let Ok(event) = received.recv_timeout(Duration::from_secs(15)) {
            let at = start.elapsed().as_secs_f32();
            match event {
                Event::Partial(_, _, t) => println!("{at:6.2}s  … {t}"),
                Event::Line(_, t) => {
                    lines += 1;
                    println!("{at:6.2}s  LINE {t}");
                }
                Event::SttStatus(_, e) | Event::SttFailed(_, e) => panic!("{e}"),
                _ => {}
            }
        }
        assert!(lines > 0);
    }
}
