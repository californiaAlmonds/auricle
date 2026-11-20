use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};

pub const ALPN: &[u8] = b"auricle/party/v1";
const MAX_QUEUE_LEN: usize = 500;
const MAX_GUESTS: usize = 32;
const MAX_INVITE_TOKEN_CHARS: usize = 8192;
const MAX_INVITE_JSON_BYTES: usize = 16 * 1024;
const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(25);

pub fn is_valid_video_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 24
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartyTrack {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub album_id: String,
    #[serde(default)]
    pub artist_id: String,
    #[serde(default)]
    pub duration_secs: u32,
    #[serde(default)]
    pub thumbnail_url: String,
    #[serde(default)]
    pub added_by: String,
}

impl PartyTrack {
    pub fn sanitized(mut self) -> Option<Self> {
        if !is_valid_video_id(&self.video_id) {
            return None;
        }
        self.title = truncate_chars(self.title.trim(), 256);
        self.artist = truncate_chars(self.artist.trim(), 256);
        self.album = truncate_chars(self.album.trim(), 256);
        self.album_id = truncate_chars(self.album_id.trim(), 64);
        self.artist_id = truncate_chars(self.artist_id.trim(), 64);
        self.added_by = truncate_chars(self.added_by.trim(), 64);
        let valid_https_thumb = self.thumbnail_url.starts_with("https://")
            && self.thumbnail_url.len() <= 512;
        if !valid_https_thumb {
            self.thumbnail_url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", self.video_id);
        }
        Some(self)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Participant {
    pub peer_id: String,
    pub name: String,
    pub platform: String,
    pub is_host: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PartySeedState {
    #[serde(default)]
    pub queue: Vec<PartyTrack>,
    #[serde(default)]
    pub current_index: usize,
    #[serde(default)]
    pub current_track: Option<PartyTrack>,
    #[serde(default)]
    pub is_playing: bool,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub repeat_mode: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PartyState {
    pub seq: u64,
    pub party_id: String,
    pub host_name: String,
    pub participants: Vec<Participant>,
    pub queue: Vec<PartyTrack>,
    pub current_index: usize,
    pub current_track: Option<PartyTrack>,
    pub is_playing: bool,
    pub position_ms: u64,
    pub anchor_host_mono_ms: u64,
    #[serde(default)]
    pub repeat_mode: String,
    #[serde(default)]
    pub last_action_by: String,
    #[serde(default)]
    pub last_action_desc: String,
}

impl PartyState {
    fn sanitized(mut self) -> Self {
        self.party_id = truncate_chars(&self.party_id, 64);
        self.host_name = truncate_chars(&self.host_name, 64);
        self.last_action_by = truncate_chars(&self.last_action_by, 64);
        self.last_action_desc = truncate_chars(&self.last_action_desc, 160);
        self.participants = self
            .participants
            .into_iter()
            .take(MAX_GUESTS + 1)
            .map(|mut p| {
                p.peer_id = truncate_chars(&p.peer_id, 64);
                p.name = truncate_chars(&p.name, 64);
                p.platform = truncate_chars(&p.platform, 24);
                p
            })
            .collect();
        self.queue = self
            .queue
            .into_iter()
            .filter_map(PartyTrack::sanitized)
            .take(MAX_QUEUE_LEN)
            .collect();
        if self.queue.is_empty() {
            self.current_index = 0;
        } else if self.current_index >= self.queue.len() {
            self.current_index = self.queue.len() - 1;
        }
        self.current_track = self.current_track.and_then(PartyTrack::sanitized);
        self.repeat_mode = match self.repeat_mode.as_str() {
            "all" | "one" => self.repeat_mode,
            _ => "off".to_string(),
        };
        self
    }

    fn apply_action(&mut self, action: PartyAction, actor_name: String, now_mono_ms: u64) {
        let actor_name = truncate_chars(actor_name.trim(), 64);
        if self.is_playing {
            let elapsed = now_mono_ms.saturating_sub(self.anchor_host_mono_ms);
            self.position_ms = self.position_ms.saturating_add(elapsed);
            if let Some(ref track) = self.current_track {
                if track.duration_secs > 0 {
                    let dur_ms = (track.duration_secs as u64) * 1000;
                    if self.position_ms > dur_ms {
                        self.position_ms = dur_ms;
                    }
                }
            }
        }
        self.anchor_host_mono_ms = now_mono_ms;
        self.last_action_by = actor_name;

        match action {
            PartyAction::PlayPause { is_playing, position_ms } => {
                self.is_playing = is_playing;
                self.position_ms = position_ms;
                self.last_action_desc = if is_playing { "resumed playback" } else { "paused playback" }.into();
            }
            PartyAction::Seek { position_ms } => {
                self.position_ms = position_ms;
                self.last_action_desc = "seeked".into();
            }
            PartyAction::Next => {
                if self.current_index + 1 < self.queue.len() {
                    self.current_index += 1;
                    self.current_track = Some(self.queue[self.current_index].clone());
                    self.position_ms = 0;
                } else if self.repeat_mode == "all" && !self.queue.is_empty() {
                    self.current_index = 0;
                    self.current_track = Some(self.queue[self.current_index].clone());
                    self.position_ms = 0;
                } else {
                    self.is_playing = false;
                }
                self.last_action_desc = "skipped to next track".into();
            }
            PartyAction::Prev => {
                if self.position_ms > 3000 {
                    self.position_ms = 0;
                } else if self.current_index > 0 {
                    self.current_index -= 1;
                    self.current_track = Some(self.queue[self.current_index].clone());
                    self.position_ms = 0;
                } else {
                    self.position_ms = 0;
                }
                self.last_action_desc = "skipped to previous track".into();
            }
            PartyAction::PlayTrackNow { track } => {
                let Some(track) = track.sanitized() else { return; };
                if let Some(pos) = self.queue.iter().position(|t| t.video_id == track.video_id) {
                    self.current_index = pos;
                } else if self.queue.is_empty() {
                    self.queue.push(track.clone());
                    self.current_index = 0;
                } else if self.queue.len() < MAX_QUEUE_LEN {
                    self.queue.insert(self.current_index + 1, track.clone());
                    self.current_index += 1;
                } else {
                    self.queue[self.current_index] = track.clone();
                }
                self.current_track = Some(self.queue[self.current_index].clone());
                self.is_playing = true;
                self.position_ms = 0;
                self.last_action_desc = format!("played {}", track.title);
            }
            PartyAction::PlayQueueIndex { index } => {
                if index < self.queue.len() {
                    self.current_index = index;
                    self.current_track = Some(self.queue[index].clone());
                    self.is_playing = true;
                    self.position_ms = 0;
                    self.last_action_desc = "changed track".into();
                }
            }
            PartyAction::AddToQueue { track, play_next } => {
                let Some(track) = track.sanitized() else { return; };
                let mut existing_idx = None;
                for (i, t) in self.queue.iter().enumerate() {
                    if t.video_id == track.video_id {
                        existing_idx = Some(i);
                        break;
                    }
                }
                if let Some(idx) = existing_idx {
                    self.queue.remove(idx);
                    if idx < self.current_index {
                        self.current_index = self.current_index.saturating_sub(1);
                    }
                }
                if self.queue.len() >= MAX_QUEUE_LEN {
                    return;
                }
                if play_next {
                    let insert_pos = (self.current_index + 1).min(self.queue.len());
                    self.queue.insert(insert_pos, track.clone());
                } else {
                    self.queue.push(track.clone());
                }
                if self.current_track.is_none() {
                    self.current_index = 0;
                    self.current_track = Some(self.queue[self.current_index].clone());
                    self.is_playing = true;
                    self.position_ms = 0;
                }
                self.last_action_desc = format!("added {} to queue", track.title);
            }
            PartyAction::RemoveFromQueue { upcoming_index } => {
                let target = self.current_index + 1 + upcoming_index;
                if target < self.queue.len() {
                    self.queue.remove(target);
                }
                self.last_action_desc = "removed track from queue".into();
            }
            PartyAction::MoveQueueNext { upcoming_index } => {
                let target = self.current_index + 1 + upcoming_index;
                if target < self.queue.len() {
                    let track = self.queue.remove(target);
                    self.queue.insert(self.current_index + 1, track);
                }
                self.last_action_desc = "moved track up in queue".into();
            }
            PartyAction::ShuffleUpcoming => {
                let start = self.current_index + 1;
                if start < self.queue.len() {
                    let mut seed = self.seq.wrapping_add(now_mono_ms) | 1;
                    for i in (start..self.queue.len()).rev() {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        let offset = (seed as usize) % (i - start + 1);
                        self.queue.swap(i, start + offset);
                    }
                }
                self.last_action_desc = "shuffled upcoming tracks".into();
            }
            PartyAction::SetRepeatMode { mode } => {
                self.repeat_mode = match mode.as_str() {
                    "all" | "one" => mode,
                    _ => "off".to_string(),
                };
                self.last_action_desc = "changed repeat mode".into();
            }
            PartyAction::ReplaceQueue { tracks, start_index, is_playing } => {
                self.queue = tracks
                    .into_iter()
                    .filter_map(PartyTrack::sanitized)
                    .take(MAX_QUEUE_LEN)
                    .collect();
                self.current_index = start_index.min(self.queue.len().saturating_sub(1));
                self.current_track = self.queue.get(self.current_index).cloned();
                self.is_playing = is_playing;
                self.position_ms = 0;
                self.last_action_desc = "replaced the queue".into();
            }
        }
        self.seq += 1;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PartyAction {
    PlayPause { is_playing: bool, position_ms: u64 },
    Seek { position_ms: u64 },
    Next,
    Prev,
    PlayTrackNow { track: PartyTrack },
    PlayQueueIndex { index: usize },
    AddToQueue { track: PartyTrack, #[serde(default)] play_next: bool },
    RemoveFromQueue { upcoming_index: usize },
    MoveQueueNext { upcoming_index: usize },
    ShuffleUpcoming,
    SetRepeatMode { mode: String },
    ReplaceQueue { tracks: Vec<PartyTrack>, #[serde(default)] start_index: usize, #[serde(default)] is_playing: bool },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PartyUiSnapshot {
    pub status: String,
    pub is_active: bool,
    pub is_host: bool,
    pub party_id: String,
    pub party_code: String,
    pub invite_link: String,
    pub host_name: String,
    pub local_name: String,
    pub participant_count: usize,
    pub participants: Vec<Participant>,
    pub rtt_ms: u64,
    pub last_action_by: String,
    pub last_action_desc: String,
    pub error_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemotePartyUpdate {
    pub has_update: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<PartyState>,
    pub target_position_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartyInvite {
    pub v: u8,
    pub party_id: String,
    pub secret: String,
    pub host_name: String,
    pub addr: EndpointAddr,
}

pub fn encode_invite_link(invite: &PartyInvite) -> Result<String, String> {
    let json = serde_json::to_string(invite).map_err(|e| e.to_string())?;
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
    std::io::Write::write_all(&mut encoder, json.as_bytes()).map_err(|e| e.to_string())?;
    let compressed = encoder.finish().map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(compressed);
    Ok(format!("auricle://party/{}", b64))
}

pub fn parse_invite_link(input: &str) -> Result<PartyInvite, String> {
    let mut token = input.trim();
    if let Some(idx) = token.find("auricle://party/") {
        token = &token[idx + 16..];
        if let Some(space_idx) = token.find(|c: char| c.is_whitespace()) {
            token = &token[..space_idx];
        }
    } else if let Some(idx) = token.find('#') {
        token = &token[idx + 1..];
    } else if let Some(idx) = token.find("?party=") {
        token = &token[idx + 7..];
        if let Some(amp_idx) = token.find('&') {
            token = &token[..amp_idx];
        }
    } else if let Some(idx) = token.find("?join=") {
        token = &token[idx + 6..];
        if let Some(amp_idx) = token.find('&') {
            token = &token[..amp_idx];
        }
    }
    if token.is_empty() || token.len() > MAX_INVITE_TOKEN_CHARS {
        return Err("Invite link length out of bounds".into());
    }
    let compressed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|e| e.to_string())?;
    let decoder = DeflateDecoder::new(&compressed[..]);
    let mut limited = std::io::Read::take(decoder, (MAX_INVITE_JSON_BYTES + 1) as u64);
    let mut json = String::new();
    std::io::Read::read_to_string(&mut limited, &mut json).map_err(|e| e.to_string())?;
    if json.len() > MAX_INVITE_JSON_BYTES {
        return Err("Invite payload exceeds maximum size".into());
    }
    serde_json::from_str(&json).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WireMsg {
    Hello { secret: String, peer_id: String, name: String, platform: String },
    Welcome { party_id: String, host_mono_ms: u64, state: PartyState },
    Reject { reason: String },
    Ping { client_mono_ms: u64 },
    Pong { client_mono_ms: u64, host_mono_ms: u64 },
    Action { actor_name: String, action: PartyAction },
    StateBroadcast { state: PartyState },
    HostLeaving,
}

async fn write_wire_msg(send_stream: &mut SendStream, msg: &WireMsg) -> Result<(), String> {
    let json = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    if json.len() > 1_048_576 {
        return Err("Message too large".into());
    }
    let len = (json.len() as u32).to_be_bytes();
    send_stream.write_all(&len).await.map_err(|e| e.to_string())?;
    send_stream.write_all(&json).await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn read_wire_msg(recv_stream: &mut RecvStream) -> Result<WireMsg, String> {
    let mut len_buf = [0u8; 4];
    recv_stream.read_exact(&mut len_buf).await.map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1_048_576 {
        return Err("Message too large".into());
    }
    let mut payload = vec![0u8; len];
    recv_stream.read_exact(&mut payload).await.map_err(|e| e.to_string())?;
    let msg = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
    Ok(msg)
}

#[derive(Clone, Debug, Default)]
struct ClockFilter {
    samples: VecDeque<(u64, i64)>,
}

impl ClockFilter {
    fn record(&mut self, rtt_ms: u64, offset_ms: i64) -> (u64, i64) {
        if self.samples.len() >= 6 {
            self.samples.pop_front();
        }
        self.samples.push_back((rtt_ms, offset_ms));
        self.samples
            .iter()
            .copied()
            .min_by_key(|(rtt, _)| *rtt)
            .unwrap_or((rtt_ms, offset_ms))
    }
}

struct SharedPartySession {
    gen: u64,
    status: String,
    is_host: bool,
    party_id: String,
    party_code: String,
    secret: String,
    invite_link: String,
    host_name: String,
    local_name: String,
    local_peer_id: String,
    error_text: String,
    rtt_ms: u64,
    clock_offset_ms: i64,
    clock_filter: ClockFilter,
    epoch_instant: Instant,
    state: PartyState,
    last_received_seq: u64,
    pending_remote_update: Option<PartyState>,
    next_conn_id: u64,
    guests: HashMap<u64, (Participant, mpsc::Sender<WireMsg>)>,
    guest_out_tx: Option<mpsc::Sender<WireMsg>>,
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl SharedPartySession {
    fn new() -> Self {
        Self {
            gen: 0,
            status: "off".to_string(),
            is_host: false,
            party_id: String::new(),
            party_code: String::new(),
            secret: String::new(),
            invite_link: String::new(),
            host_name: String::new(),
            local_name: String::new(),
            local_peer_id: gen_hex_token(8),
            error_text: String::new(),
            rtt_ms: 0,
            clock_offset_ms: 0,
            clock_filter: ClockFilter::default(),
            epoch_instant: Instant::now(),
            state: PartyState::default(),
            last_received_seq: 0,
            pending_remote_update: None,
            next_conn_id: 1,
            guests: HashMap::new(),
            guest_out_tx: None,
            shutdown_tx: None,
        }
    }

    fn is_active(&self) -> bool {
        self.status == "hosting" || self.status == "joined"
    }

    fn mono_ms(&self) -> u64 {
        self.epoch_instant.elapsed().as_millis() as u64
    }

    fn estimated_host_mono_ms(&self) -> u64 {
        if self.is_host {
            self.mono_ms()
        } else {
            (self.mono_ms() as i64 + self.clock_offset_ms).max(0) as u64
        }
    }

    fn projected_position_ms(&self) -> u64 {
        if !self.state.is_playing {
            return self.state.position_ms;
        }
        let now_h = self.estimated_host_mono_ms();
        let elapsed = now_h.saturating_sub(self.state.anchor_host_mono_ms);
        let raw = self.state.position_ms.saturating_add(elapsed);
        if let Some(ref t) = self.state.current_track {
            if t.duration_secs > 0 {
                let cap = (t.duration_secs as u64).saturating_mul(1000);
                return raw.min(cap);
            }
        }
        raw
    }

    fn rebuild_host_participants(&mut self, platform: &str) {
        let mut list = Vec::with_capacity(1 + self.guests.len());
        list.push(Participant {
            peer_id: self.local_peer_id.clone(),
            name: self.local_name.clone(),
            platform: platform.to_string(),
            is_host: true,
        });
        let mut others: Vec<Participant> = self.guests.values().map(|(p, _)| p.clone()).collect();
        others.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        list.extend(others);
        self.state.participants = list;
    }

    fn broadcast_to_guests(&self, msg: &WireMsg) {
        for (_, (_, tx)) in &self.guests {
            let _ = tx.try_send(msg.clone());
        }
    }

    fn to_snapshot(&self) -> PartyUiSnapshot {
        let active = self.is_active();
        PartyUiSnapshot {
            status: self.status.clone(),
            is_active: active,
            is_host: self.is_host,
            party_id: self.party_id.clone(),
            party_code: self.party_code.clone(),
            invite_link: self.invite_link.clone(),
            host_name: self.host_name.clone(),
            local_name: self.local_name.clone(),
            participant_count: if active { self.state.participants.len() } else { 0 },
            participants: if active { self.state.participants.clone() } else { Vec::new() },
            rtt_ms: self.rtt_ms,
            last_action_by: self.state.last_action_by.clone(),
            last_action_desc: self.state.last_action_desc.clone(),
            error_text: self.error_text.clone(),
        }
    }
}

fn fill_os_csprng(buf: &mut [u8]) -> bool {
    #[cfg(windows)]
    {
        #[link(name = "advapi32")]
        extern "system" {
            // RtlGenRandom
            fn SystemFunction036(RandomBuffer: *mut u8, RandomBufferLength: u32) -> u8;
        }
        unsafe { SystemFunction036(buf.as_mut_ptr(), buf.len() as u32) != 0 }
    }
    #[cfg(unix)]
    {
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            return std::io::Read::read_exact(&mut f, buf).is_ok();
        }
        false
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = buf;
        false
    }
}

fn gen_hex_token(bytes_len: usize) -> String {
    let mut raw = vec![0u8; bytes_len];
    if !fill_os_csprng(&mut raw) {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x517cc1b727220a95);
        let mut state = nanos
            ^ SEQ
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_mul(0x9e3779b97f4a7c15)
            ^ (std::process::id() as u64).rotate_left(17);
        for b in &mut raw {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = (state & 0xff) as u8;
        }
    }
    let mut out = String::with_capacity(bytes_len * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in raw {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn derive_party_code(party_id: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut acc: u64 = 0xcbf29ce484222325;
    for b in party_id.bytes() {
        acc ^= b as u64;
        acc = acc.wrapping_mul(0x100000001b3);
    }
    let mut code = String::with_capacity(6);
    for _ in 0..6 {
        code.push(ALPHABET[(acc as usize) % ALPHABET.len()] as char);
        acc >>= 5;
    }
    code
}

pub struct PartyManager {
    platform: &'static str,
    inner: Arc<Mutex<SharedPartySession>>,
    wakeup: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>>>,
    rt: OnceLock<Runtime>,
}

impl PartyManager {
    pub fn new(platform: &'static str) -> Self {
        Self {
            platform,
            inner: Arc::new(Mutex::new(SharedPartySession::new())),
            wakeup: Arc::new(Mutex::new(None)),
            rt: OnceLock::new(),
        }
    }

    fn runtime(&self) -> &Runtime {
        self.rt.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("PartyManager tokio runtime")
        })
    }

    fn notify_wakeup(wakeup: &Arc<Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>>>) {
        if let Ok(guard) = wakeup.lock() {
            if let Some(ref cb) = *guard {
                cb();
            }
        }
    }

    pub fn set_wakeup_callback(&self, cb: Box<dyn Fn() + Send + Sync + 'static>) {
        if let Ok(mut guard) = self.wakeup.lock() {
            *guard = Some(cb);
        }
    }

    pub fn snapshot(&self) -> PartyUiSnapshot {
        let guard = self.inner.lock().unwrap();
        guard.to_snapshot()
    }

    pub fn leave_party(&self) -> PartyUiSnapshot {
        let snap = {
            let mut s = self.inner.lock().unwrap();
            s.gen = s.gen.wrapping_add(1);
            if s.is_host && s.is_active() {
                s.broadcast_to_guests(&WireMsg::HostLeaving);
            }
            if let Some(tx) = s.shutdown_tx.take() {
                let _ = tx.send(true);
            }
            s.guests.clear();
            s.guest_out_tx = None;
            s.status = "off".to_string();
            s.is_host = false;
            s.invite_link.clear();
            s.party_id.clear();
            s.party_code.clear();
            s.error_text.clear();
            s.pending_remote_update = None;
            s.state = PartyState::default();
            s.to_snapshot()
        };
        Self::notify_wakeup(&self.wakeup);
        snap
    }

    pub fn start_party(&self, display_name: String, seed: PartySeedState) -> PartyUiSnapshot {
        let clean_name = {
            let t = display_name.trim();
            if t.is_empty() {
                if self.platform == "android" {
                    "Android Listener".to_string()
                } else {
                    "Windows Listener".to_string()
                }
            } else {
                t.to_string()
            }
        };

        let party_id = gen_hex_token(8);
        let party_code = derive_party_code(&party_id);
        let secret = gen_hex_token(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let (my_gen, initial_snap) = {
            let mut s = self.inner.lock().unwrap();
            if let Some(prev_tx) = s.shutdown_tx.take() {
                let _ = prev_tx.send(true);
            }
            s.gen = s.gen.wrapping_add(1);
            let gen = s.gen;
            s.status = "starting".to_string();
            s.is_host = true;
            s.party_id = party_id.clone();
            s.party_code = party_code.clone();
            s.secret = secret.clone();
            s.invite_link.clear();
            s.host_name = clean_name.clone();
            s.local_name = clean_name.clone();
            s.error_text.clear();
            s.rtt_ms = 0;
            s.clock_offset_ms = 0;
            s.epoch_instant = Instant::now();
            s.guests.clear();
            s.guest_out_tx = None;
            s.pending_remote_update = None;
            s.shutdown_tx = Some(shutdown_tx);

            let cur_idx = if seed.queue.is_empty() {
                0
            } else {
                seed.current_index.min(seed.queue.len() - 1)
            };
            let cur_track = seed
                .current_track
                .or_else(|| seed.queue.get(cur_idx).cloned());

            s.state = PartyState {
                seq: 1,
                party_id: party_id.clone(),
                host_name: clean_name.clone(),
                participants: vec![Participant {
                    peer_id: s.local_peer_id.clone(),
                    name: clean_name.clone(),
                    platform: self.platform.to_string(),
                    is_host: true,
                }],
                queue: seed.queue,
                current_index: cur_idx,
                current_track: cur_track,
                is_playing: seed.is_playing,
                position_ms: seed.position_ms,
                anchor_host_mono_ms: 0,
                repeat_mode: if seed.repeat_mode.is_empty() {
                    "off".to_string()
                } else {
                    seed.repeat_mode
                },
                last_action_by: clean_name.clone(),
                last_action_desc: "started the party".to_string(),
            };
            s.last_received_seq = 1;
            (gen, s.to_snapshot())
        };

        let inner_arc = Arc::clone(&self.inner);
        let wakeup_arc = Arc::clone(&self.wakeup);
        let platform_str = self.platform;

        self.runtime().spawn(async move {
            let bind_res = Endpoint::builder(presets::N0)
                .alpns(vec![ALPN.to_vec()])
                .bind()
                .await;

            let endpoint = match bind_res {
                Ok(ep) => ep,
                Err(e) => {
                    let mut should_wake = false;
                    {
                        let mut s = inner_arc.lock().unwrap();
                        if s.gen == my_gen {
                            s.status = "error".to_string();
                            s.error_text = format!("Failed to bind party P2P endpoint: {e}");
                            should_wake = true;
                        }
                    }
                    if should_wake {
                        Self::notify_wakeup(&wakeup_arc);
                    }
                    return;
                }
            };

            let _ = tokio::time::timeout(Duration::from_secs(4), endpoint.online()).await;
            let addr = endpoint.addr();
            let invite = PartyInvite {
                v: 1,
                party_id: party_id.clone(),
                secret: secret.clone(),
                host_name: clean_name.clone(),
                addr,
            };

            let link = match encode_invite_link(&invite) {
                Ok(l) => l,
                Err(e) => {
                    {
                        let mut s = inner_arc.lock().unwrap();
                        if s.gen == my_gen {
                            s.status = "error".to_string();
                            s.error_text = format!("Invite encode error: {e}");
                        }
                    }
                    Self::notify_wakeup(&wakeup_arc);
                    endpoint.close().await;
                    return;
                }
            };

            let mut active_now = false;
            {
                let mut s = inner_arc.lock().unwrap();
                if s.gen == my_gen {
                    s.invite_link = link;
                    s.status = "hosting".to_string();
                    active_now = true;
                }
            }
            if !active_now {
                endpoint.close().await;
                return;
            }
            Self::notify_wakeup(&wakeup_arc);

            let mut rx_stop = shutdown_rx.clone();
            loop {
                tokio::select! {
                    _ = rx_stop.changed() => {
                        if *rx_stop.borrow() {
                            break;
                        }
                    }
                    incoming_opt = endpoint.accept() => {
                        let Some(incoming) = incoming_opt else { break; };
                        let inner_conn = Arc::clone(&inner_arc);
                        let wakeup_conn = Arc::clone(&wakeup_arc);
                        let expected_secret = secret.clone();
                        let expected_party_id = party_id.clone();
                        let mut conn_stop = shutdown_rx.clone();
                        tokio::spawn(async move {
                            let Ok(Ok(conn)) = tokio::time::timeout(Duration::from_secs(10), incoming).await else {
                                return;
                            };
                            handle_host_guest_conn(
                                conn,
                                inner_conn,
                                wakeup_conn,
                                my_gen,
                                platform_str,
                                expected_secret,
                                expected_party_id,
                                &mut conn_stop,
                            ).await;
                        });
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(40)).await;
            endpoint.close().await;
        });

        initial_snap
    }

    pub fn join_party(&self, invite_input: String, display_name: String) -> PartyUiSnapshot {
        let invite = match parse_invite_link(&invite_input) {
            Ok(inv) => inv,
            Err(e) => {
                let mut s = self.inner.lock().unwrap();
                s.status = "error".to_string();
                s.error_text = format!("Invalid party link: {e}");
                return s.to_snapshot();
            }
        };

        let clean_name = {
            let t = display_name.trim();
            if t.is_empty() {
                if self.platform == "android" {
                    "Android Listener".to_string()
                } else {
                    "Windows Listener".to_string()
                }
            } else {
                t.to_string()
            }
        };

        let canonical_link = encode_invite_link(&invite).unwrap_or_else(|_| invite_input.trim().to_string());
        let party_code = derive_party_code(&invite.party_id);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let (my_gen, my_peer_id, initial_snap) = {
            let mut s = self.inner.lock().unwrap();
            if let Some(prev_tx) = s.shutdown_tx.take() {
                let _ = prev_tx.send(true);
            }
            s.gen = s.gen.wrapping_add(1);
            let gen = s.gen;
            let pid = s.local_peer_id.clone();
            s.status = "joining".to_string();
            s.is_host = false;
            s.party_id = invite.party_id.clone();
            s.party_code = party_code;
            s.secret = invite.secret.clone();
            s.invite_link = canonical_link;
            s.host_name = invite.host_name.clone();
            s.local_name = clean_name.clone();
            s.error_text.clear();
            s.rtt_ms = 0;
            s.clock_offset_ms = 0;
            s.clock_filter = ClockFilter::default();
            s.epoch_instant = Instant::now();
            s.guests.clear();
            s.guest_out_tx = None;
            s.pending_remote_update = None;
            s.last_received_seq = 0;
            s.shutdown_tx = Some(shutdown_tx);
            (gen, pid, s.to_snapshot())
        };

        let inner_arc = Arc::clone(&self.inner);
        let wakeup_arc = Arc::clone(&self.wakeup);
        let platform_str = self.platform;

        self.runtime().spawn(async move {
            run_guest_loop(
                invite,
                clean_name,
                my_peer_id,
                platform_str,
                my_gen,
                inner_arc,
                wakeup_arc,
                shutdown_rx,
            )
            .await;
        });

        initial_snap
    }

    pub fn poll_remote_update(&self) -> RemotePartyUpdate {
        let mut s = self.inner.lock().unwrap();
        let pending = s.pending_remote_update.take();
        match pending {
            Some(st) => {
                let target_ms = s.projected_position_ms();
                RemotePartyUpdate {
                    has_update: true,
                    state: Some(st),
                    target_position_ms: target_ms,
                }
            }
            None => RemotePartyUpdate {
                has_update: false,
                state: None,
                target_position_ms: 0,
            },
        }
    }

    pub fn send_action(&self, action: PartyAction, emit_to_self: bool) {
        let mut should_wake = false;
        {
            let mut s = self.inner.lock().unwrap();
            if !s.is_active() {
                return;
            }
            if s.is_host {
                let now_ms = s.mono_ms();
                let actor = s.local_name.clone();
                s.state.apply_action(action, actor, now_ms);
                let broadcast = WireMsg::StateBroadcast {
                    state: s.state.clone(),
                };
                s.broadcast_to_guests(&broadcast);
                if emit_to_self {
                    s.pending_remote_update = Some(s.state.clone());
                }
                should_wake = true;
            } else if let Some(ref tx) = s.guest_out_tx {
                let msg = WireMsg::Action {
                    actor_name: s.local_name.clone(),
                    action,
                };
                let _ = tx.try_send(msg);
            }
        }
        if should_wake {
            Self::notify_wakeup(&self.wakeup);
        }
    }

    pub fn target_position_ms(&self, current_video_id: &str) -> Option<u64> {
        let s = self.inner.lock().unwrap();
        if !s.is_active() {
            return None;
        }
        let track_vid = s
            .state
            .current_track
            .as_ref()
            .map(|t| t.video_id.as_str())
            .unwrap_or("");
        if track_vid.is_empty() || track_vid != current_video_id {
            return None;
        }
        Some(s.projected_position_ms())
    }
}

async fn handle_host_guest_conn(
    conn: Connection,
    inner_arc: Arc<Mutex<SharedPartySession>>,
    wakeup_arc: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>>>,
    my_gen: u64,
    host_platform: &'static str,
    expected_secret: String,
    expected_party_id: String,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let Ok(Ok((mut send_stream, mut recv_stream))) =
        tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await
    else {
        return;
    };

    let hello = match tokio::time::timeout(Duration::from_secs(10), read_wire_msg(&mut recv_stream)).await {
        Ok(Ok(msg)) => msg,
        _ => return,
    };

    let (secret, peer_id, guest_name, guest_platform) = match hello {
        WireMsg::Hello {
            secret,
            peer_id,
            name,
            platform,
        } => (
            secret,
            truncate_chars(peer_id.trim(), 64),
            truncate_chars(name.trim(), 64),
            truncate_chars(platform.trim(), 24),
        ),
        _ => return,
    };

    if !ct_eq(secret.as_bytes(), expected_secret.as_bytes()) {
        let _ = write_wire_msg(
            &mut send_stream,
            &WireMsg::Reject {
                reason: "Invalid party invite secret".to_string(),
            },
        )
        .await;
        return;
    }

    let (tx, mut rx) = mpsc::channel::<WireMsg>(64);
    let welcome_opt = {
        let mut s = inner_arc.lock().unwrap();
        if s.gen != my_gen || !s.is_host {
            None
        } else if s.guests.len() >= MAX_GUESTS {
            Some((
                0u64,
                WireMsg::Reject {
                    reason: "Party is full".to_string(),
                },
            ))
        } else {
            let conn_id = s.next_conn_id;
            s.next_conn_id = s.next_conn_id.wrapping_add(1);
            let now_ms = s.mono_ms();
            if s.state.is_playing {
                let elapsed = now_ms.saturating_sub(s.state.anchor_host_mono_ms);
                s.state.position_ms = s.state.position_ms.saturating_add(elapsed);
                s.state.anchor_host_mono_ms = now_ms;
            } else {
                s.state.anchor_host_mono_ms = now_ms;
            }
            let safe_name = if guest_name.is_empty() {
                "Listener".to_string()
            } else {
                guest_name.clone()
            };
            let p = Participant {
                peer_id: peer_id.clone(),
                name: safe_name.clone(),
                platform: guest_platform,
                is_host: false,
            };
            s.guests.insert(conn_id, (p, tx.clone()));
            s.rebuild_host_participants(host_platform);
            s.state.seq += 1;
            s.state.last_action_by = safe_name.clone();
            s.state.last_action_desc = format!("{safe_name} joined the party");
            let st = s.state.clone();
            for (cid, (_, gtx)) in &s.guests {
                if *cid != conn_id {
                    let _ = gtx.try_send(WireMsg::StateBroadcast { state: st.clone() });
                }
            }
            Some((
                conn_id,
                WireMsg::Welcome {
                    party_id: expected_party_id,
                    host_mono_ms: now_ms,
                    state: st,
                },
            ))
        }
    };

    let Some((conn_id, welcome_msg)) = welcome_opt else {
        return;
    };

    if matches!(welcome_msg, WireMsg::Reject { .. }) {
        let _ = write_wire_msg(&mut send_stream, &welcome_msg).await;
        return;
    }

    if write_wire_msg(&mut send_stream, &welcome_msg).await.is_err() {
        let mut s = inner_arc.lock().unwrap();
        if s.gen == my_gen {
            s.guests.remove(&conn_id);
            s.rebuild_host_participants(host_platform);
        }
        return;
    }
    PartyManager::notify_wakeup(&wakeup_arc);

    let mut writer_stop = shutdown_rx.clone();
    let writer_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = writer_stop.changed() => {
                    if *writer_stop.borrow() {
                        while let Ok(msg) = rx.try_recv() {
                            let _ = write_wire_msg(&mut send_stream, &msg).await;
                        }
                        break;
                    }
                }
                msg_opt = rx.recv() => {
                    let Some(msg) = msg_opt else { break; };
                    if write_wire_msg(&mut send_stream, &msg).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            read_res = tokio::time::timeout(IDLE_READ_TIMEOUT, read_wire_msg(&mut recv_stream)) => {
                let Ok(Ok(msg)) = read_res else { break; };
                match msg {
                    WireMsg::Ping { client_mono_ms } => {
                        let host_mono_ms = {
                            let s = inner_arc.lock().unwrap();
                            s.mono_ms()
                        };
                        let _ = tx.try_send(WireMsg::Pong { client_mono_ms, host_mono_ms });
                    }
                    WireMsg::Action { actor_name, action } => {
                        let mut wake = false;
                        {
                            let mut s = inner_arc.lock().unwrap();
                            if s.gen == my_gen && s.is_host {
                                let now_ms = s.mono_ms();
                                s.state.apply_action(action, actor_name, now_ms);
                                s.pending_remote_update = Some(s.state.clone());
                                let bcast = WireMsg::StateBroadcast { state: s.state.clone() };
                                s.broadcast_to_guests(&bcast);
                                wake = true;
                            }
                        }
                        if wake {
                            PartyManager::notify_wakeup(&wakeup_arc);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    writer_handle.abort();
    let mut wake_leave = false;
    {
        let mut s = inner_arc.lock().unwrap();
        if s.gen == my_gen && s.is_host && s.guests.remove(&conn_id).is_some() {
            s.rebuild_host_participants(host_platform);
            s.state.seq += 1;
            s.state.last_action_by = guest_name.clone();
            s.state.last_action_desc = format!("{guest_name} left the party");
            let bcast = WireMsg::StateBroadcast {
                state: s.state.clone(),
            };
            s.broadcast_to_guests(&bcast);
            wake_leave = true;
        }
    }
    if wake_leave {
        PartyManager::notify_wakeup(&wakeup_arc);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_guest_loop(
    invite: PartyInvite,
    local_name: String,
    local_peer_id: String,
    platform_str: &'static str,
    my_gen: u64,
    inner_arc: Arc<Mutex<SharedPartySession>>,
    wakeup_arc: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let backoffs = [
        Duration::from_millis(0),
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(4),
    ];

    let mut last_err = String::new();

    for (attempt, delay_dur) in backoffs.iter().enumerate() {
        if attempt > 0 {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() { return; }
                }
                _ = tokio::time::sleep(*delay_dur) => {}
            }
            let still_ours = {
                let s = inner_arc.lock().unwrap();
                s.gen == my_gen
            };
            if !still_ours {
                return;
            }
        }

        let endpoint = match Endpoint::builder(presets::N0).bind().await {
            Ok(ep) => ep,
            Err(e) => {
                last_err = format!("P2P bind failed: {e}");
                continue;
            }
        };

        let _ = tokio::time::timeout(Duration::from_secs(3), endpoint.online()).await;

        let conn_res = tokio::time::timeout(
            Duration::from_secs(15),
            endpoint.connect(invite.addr.clone(), ALPN),
        )
        .await;

        let conn = match conn_res {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                last_err = format!("Connect failed: {e}");
                endpoint.close().await;
                continue;
            }
            Err(_) => {
                last_err = "Connection timed out".to_string();
                endpoint.close().await;
                continue;
            }
        };

        let Ok((mut send_stream, mut recv_stream)) = conn.open_bi().await else {
            last_err = "Failed to open stream".to_string();
            endpoint.close().await;
            continue;
        };

        let hello_sent_mono = {
            let s = inner_arc.lock().unwrap();
            s.mono_ms()
        };

        let hello = WireMsg::Hello {
            secret: invite.secret.clone(),
            peer_id: local_peer_id.clone(),
            name: local_name.clone(),
            platform: platform_str.to_string(),
        };

        if let Err(e) = write_wire_msg(&mut send_stream, &hello).await {
            last_err = e;
            endpoint.close().await;
            continue;
        }

        let first_reply =
            match tokio::time::timeout(Duration::from_secs(10), read_wire_msg(&mut recv_stream)).await {
                Ok(Ok(m)) => m,
                Ok(Err(e)) => {
                    last_err = e;
                    endpoint.close().await;
                    continue;
                }
                Err(_) => {
                    last_err = "Handshake timed out".to_string();
                    endpoint.close().await;
                    continue;
                }
            };

        match first_reply {
            WireMsg::Reject { reason } => {
                {
                    let mut s = inner_arc.lock().unwrap();
                    if s.gen == my_gen {
                        s.status = "error".to_string();
                        s.error_text = reason;
                    }
                }
                PartyManager::notify_wakeup(&wakeup_arc);
                endpoint.close().await;
                return;
            }
            WireMsg::Welcome {
                host_mono_ms,
                state,
                ..
            } => {
                let state = state.sanitized();
                let (out_tx, mut out_rx) = mpsc::channel::<WireMsg>(64);
                let mut valid = false;
                {
                    let mut s = inner_arc.lock().unwrap();
                    if s.gen == my_gen {
                        let now_client = s.mono_ms();
                        let init_rtt = now_client.saturating_sub(hello_sent_mono);
                        let init_offset =
                            host_mono_ms as i64 - (hello_sent_mono + init_rtt / 2) as i64;
                        let (best_rtt, best_off) = s.clock_filter.record(init_rtt, init_offset);
                        s.rtt_ms = best_rtt;
                        s.clock_offset_ms = best_off;
                        s.status = "joined".to_string();
                        s.error_text.clear();
                        s.host_name = state.host_name.clone();
                        s.last_received_seq = state.seq;
                        s.state = state.clone();
                        s.pending_remote_update = Some(state);
                        s.guest_out_tx = Some(out_tx.clone());
                        let _ = out_tx.try_send(WireMsg::Ping {
                            client_mono_ms: now_client,
                        });
                        valid = true;
                    }
                }
                if !valid {
                    endpoint.close().await;
                    return;
                }
                PartyManager::notify_wakeup(&wakeup_arc);

                let inner_ping = Arc::clone(&inner_arc);
                let mut writer_stop = shutdown_rx.clone();
                let writer_handle = tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(Duration::from_secs(4));
                    ticker.tick().await;
                    loop {
                        tokio::select! {
                            _ = writer_stop.changed() => {
                                if *writer_stop.borrow() { break; }
                            }
                            _ = ticker.tick() => {
                                let c_ms = {
                                    let s = inner_ping.lock().unwrap();
                                    if s.gen != my_gen { break; }
                                    s.mono_ms()
                                };
                                if write_wire_msg(&mut send_stream, &WireMsg::Ping { client_mono_ms: c_ms }).await.is_err() {
                                    break;
                                }
                            }
                            msg_opt = out_rx.recv() => {
                                let Some(msg) = msg_opt else { break; };
                                if write_wire_msg(&mut send_stream, &msg).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });

                let mut host_ended = false;
                let mut user_cancelled = false;

                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                user_cancelled = true;
                                break;
                            }
                        }
                        msg_res = tokio::time::timeout(IDLE_READ_TIMEOUT, read_wire_msg(&mut recv_stream)) => {
                            let msg_res = match msg_res {
                                Ok(r) => r,
                                Err(_) => Err("Connection timed out".to_string()),
                            };
                            match msg_res {
                                Ok(WireMsg::Pong { client_mono_ms, host_mono_ms }) => {
                                    let mut s = inner_arc.lock().unwrap();
                                    if s.gen == my_gen {
                                        let now_c = s.mono_ms();
                                        let rtt = now_c.saturating_sub(client_mono_ms);
                                        let off = host_mono_ms as i64 - (client_mono_ms + rtt / 2) as i64;
                                        let (best_rtt, best_off) = s.clock_filter.record(rtt, off);
                                        s.rtt_ms = best_rtt;
                                        s.clock_offset_ms = best_off;
                                    }
                                }
                                Ok(WireMsg::StateBroadcast { state }) => {
                                    let state = state.sanitized();
                                    let mut wake = false;
                                    {
                                        let mut s = inner_arc.lock().unwrap();
                                        if s.gen == my_gen && state.seq >= s.last_received_seq {
                                            s.last_received_seq = state.seq;
                                            s.host_name = state.host_name.clone();
                                            s.state = state.clone();
                                            s.pending_remote_update = Some(state);
                                            wake = true;
                                        }
                                    }
                                    if wake {
                                        PartyManager::notify_wakeup(&wakeup_arc);
                                    }
                                }
                                Ok(WireMsg::HostLeaving) => {
                                    host_ended = true;
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    last_err = e;
                                    break;
                                }
                            }
                        }
                    }
                }

                writer_handle.abort();
                endpoint.close().await;

                if user_cancelled {
                    return;
                }
                if host_ended {
                    {
                        let mut s = inner_arc.lock().unwrap();
                        if s.gen == my_gen {
                            s.status = "off".to_string();
                            s.guest_out_tx = None;
                            s.pending_remote_update = None;
                            s.state.last_action_desc = "Host ended the party".to_string();
                        }
                    }
                    PartyManager::notify_wakeup(&wakeup_arc);
                    return;
                }
            }
            _ => {
                last_err = "Unexpected handshake message".to_string();
                endpoint.close().await;
            }
        }
    }

    {
        let mut s = inner_arc.lock().unwrap();
        if s.gen == my_gen {
            s.status = "error".to_string();
            s.guest_out_tx = None;
            s.error_text = if last_err.is_empty() {
                "Disconnected from party".to_string()
            } else {
                last_err
            };
        }
    }
    PartyManager::notify_wakeup(&wakeup_arc);
}

pub fn party_manager() -> &'static PartyManager {
    static MGR: OnceLock<PartyManager> = OnceLock::new();
    MGR.get_or_init(|| {
        PartyManager::new(if cfg!(target_os = "android") {
            "android"
        } else {
            "windows"
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track(id: &str, title: &str) -> PartyTrack {
        PartyTrack {
            video_id: id.to_string(),
            title: title.to_string(),
            artist: "Artist".to_string(),
            duration_secs: 200,
            ..Default::default()
        }
    }

    #[test]
    fn invite_link_roundtrip_and_chat_extraction() {
        let secret_key = iroh::SecretKey::from_bytes(&[7u8; 32]);
        let addr = EndpointAddr::new(secret_key.public());
        let invite = PartyInvite {
            v: 1,
            party_id: "ab12cd34ef56".to_string(),
            secret: "supersecret123".to_string(),
            host_name: "Deepak".to_string(),
            addr,
        };

        let link = encode_invite_link(&invite).expect("encode");
        assert!(link.starts_with("auricle://party/"));

        // Direct link
        let parsed1 = parse_invite_link(&link).expect("parse direct");
        assert_eq!(parsed1.party_id, invite.party_id);
        assert_eq!(parsed1.secret, invite.secret);
        assert_eq!(parsed1.host_name, "Deepak");

        // Embedded inside WhatsApp / chat message text
        let chat = format!("Hey listen with me! {link} join right now!");
        let parsed2 = parse_invite_link(&chat).expect("parse chat");
        assert_eq!(parsed2.party_id, invite.party_id);

        // Raw token and query param forms
        let raw_token = link.strip_prefix("auricle://party/").unwrap();
        let parsed3 = parse_invite_link(raw_token).expect("parse raw");
        assert_eq!(parsed3.party_id, invite.party_id);

        let web_url = format!("https://californiaalmonds.github.io/auricle/party?join={raw_token}&src=app");
        let parsed4 = parse_invite_link(&web_url).expect("parse web query");
        assert_eq!(parsed4.party_id, invite.party_id);

        assert!(parse_invite_link("not-a-valid-party-token!!!").is_err());
    }

    #[test]
    fn clock_filter_picks_minimum_rtt_offset() {
        let mut filter = ClockFilter::default();
        assert_eq!(filter.record(120, 500), (120, 500));
        assert_eq!(filter.record(18, 430), (18, 430));
        assert_eq!(filter.record(85, 490), (18, 430));
    }

    #[test]
    fn state_machine_actions_preserve_queue_and_clock() {
        let mut st = PartyState {
            seq: 1,
            party_id: "p1".into(),
            host_name: "Host".into(),
            participants: vec![],
            queue: vec![sample_track("v1", "Song 1")],
            current_index: 0,
            current_track: Some(sample_track("v1", "Song 1")),
            is_playing: true,
            position_ms: 1_000,
            anchor_host_mono_ms: 5_000,
            repeat_mode: "off".into(),
            last_action_by: String::new(),
            last_action_desc: String::new(),
        };

        // Add song to queue at t=7000 -> elapsed 2000ms added to position_ms (3000ms)
        st.apply_action(
            PartyAction::AddToQueue {
                track: sample_track("v2", "Song 2"),
                play_next: false,
            },
            "Guest".into(),
            7_000,
        );
        assert_eq!(st.position_ms, 3_000);
        assert_eq!(st.queue.len(), 2);

        // Add song with play_next = true -> inserts right after current_index (at index 1)
        st.apply_action(
            PartyAction::AddToQueue {
                track: sample_track("v3", "Song 3"),
                play_next: true,
            },
            "Guest".into(),
            7_500,
        );
        assert_eq!(st.queue[1].video_id, "v3");
        assert_eq!(st.queue[2].video_id, "v2");

        // Next -> advances to v3 at position 0
        st.apply_action(PartyAction::Next, "Host".into(), 8_000);
        assert_eq!(st.current_index, 1);
        assert_eq!(st.current_track.as_ref().unwrap().video_id, "v3");
        assert_eq!(st.position_ms, 0);

        // Seek to 10_000ms, then Prev (>3000ms) -> restarts at 0 without changing track
        st.apply_action(PartyAction::Seek { position_ms: 10_000 }, "Guest".into(), 8_100);
        st.apply_action(PartyAction::Prev, "Guest".into(), 8_200);
        assert_eq!(st.current_index, 1);
        assert_eq!(st.position_ms, 0);

        // Prev again within 3s -> steps back to index 0 (v1)
        st.apply_action(PartyAction::Prev, "Guest".into(), 8_300);
        assert_eq!(st.current_index, 0);
        assert_eq!(st.current_track.as_ref().unwrap().video_id, "v1");
    }

    #[test]
    fn security_rejects_traversal_video_ids_and_deflate_bombs() {
        assert!(is_valid_video_id("dQw4w9WgXcQ"));
        assert!(!is_valid_video_id("../../evil"));
        assert!(!is_valid_video_id("abc/def"));
        assert!(!is_valid_video_id("abc\\def"));
        assert!(!is_valid_video_id(""));

        let evil_track = PartyTrack {
            video_id: "..\\..\\Windows\\win.ini".into(),
            title: "Evil".into(),
            artist: "Attacker".into(),
            thumbnail_url: "file:///etc/passwd".into(),
            ..Default::default()
        };
        assert!(evil_track.sanitized().is_none());

        let safe_track = PartyTrack {
            video_id: "dQw4w9WgXcQ".into(),
            title: "Safe".into(),
            artist: "Rick".into(),
            thumbnail_url: "http://192.168.1.1/ssrf".into(),
            ..Default::default()
        }
        .sanitized()
        .expect("valid id");
        assert!(safe_track.thumbnail_url.starts_with("https://i.ytimg.com/vi/"));

        // Construct a highly compressible 64 KiB payload (> MAX_INVITE_JSON_BYTES = 16 KiB)
        let big = vec![b'A'; 64 * 1024];
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
        std::io::Write::write_all(&mut enc, &big).unwrap();
        let compressed = enc.finish().unwrap();
        let bomb_token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(compressed);
        let bomb_link = format!("auricle://party/{bomb_token}");
        let err = parse_invite_link(&bomb_link).unwrap_err();
        assert!(err.contains("maximum size"), "unexpected error: {err}");
    }
}
