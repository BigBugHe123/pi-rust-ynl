use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ring::rand::SecureRandom;
use serde::Serialize;
use tokio::sync::broadcast;

const MAX_BACKLOG_BYTES: usize = 128 * 1024;
const RECONNECT_MS: u64 = 120_000;

static PAGE_STREAMS: OnceLock<Mutex<HashMap<u64, DebugPageStream>>> = OnceLock::new();
static NEXT_PAGE_STREAM_ID: AtomicU64 = AtomicU64::new(1);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum TerminalEvent {
    Output {
        data: String,
        offset: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        reset: Option<bool>,
    },
    Exit {
        #[serde(rename = "exitCode")]
        exit_code: i32,
    },
    Closed,
}

struct OutputState {
    text: String,
    offset: u64,
}

struct TerminalRecord {
    cwd: PathBuf,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    output: Mutex<OutputState>,
    events: broadcast::Sender<TerminalEvent>,
    listeners: AtomicUsize,
    exited: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    cleanup_at: Mutex<Option<u64>>,
}

#[derive(Clone, Default)]
pub(super) struct TerminalManager {
    records: Arc<Mutex<HashMap<String, Arc<TerminalRecord>>>>,
}

pub(super) struct TerminalSubscription {
    pub(super) initial: TerminalEvent,
    pub(super) exited: bool,
    pub(super) exit_code: Option<i32>,
    receiver: broadcast::Receiver<TerminalEvent>,
    record: Arc<TerminalRecord>,
    manager: TerminalManager,
    id: String,
    last_offset: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DebugPageStream {
    pub(super) holder: Option<String>,
    pub(super) address: Option<String>,
    pub(super) client: Option<String>,
}

pub(super) struct PageStreamGuard(u64);

pub(super) fn register_page_stream(
    holder: Option<&str>,
    forwarded_for: Option<&str>,
    user_agent: Option<&str>,
) -> PageStreamGuard {
    let id = NEXT_PAGE_STREAM_ID.fetch_add(1, Ordering::Relaxed);
    let entry = DebugPageStream {
        holder: holder.filter(|value| is_liveness_holder(value)).map(str::to_owned),
        address: debug_stream_address(forwarded_for),
        client: debug_stream_client(user_agent),
    };
    lock(PAGE_STREAMS.get_or_init(|| Mutex::new(HashMap::new()))).insert(id, entry);
    PageStreamGuard(id)
}

pub(super) fn debug_page_streams() -> Vec<DebugPageStream> {
    lock(PAGE_STREAMS.get_or_init(|| Mutex::new(HashMap::new()))).values().cloned().collect()
}

impl Drop for PageStreamGuard {
    fn drop(&mut self) {
        lock(PAGE_STREAMS.get_or_init(|| Mutex::new(HashMap::new()))).remove(&self.0);
    }
}

fn is_liveness_holder(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn debug_stream_address(forwarded_for: Option<&str>) -> Option<String> {
    let first = forwarded_for?.split(',').next()?.trim();
    if first.is_empty()
        || first.len() > 64
        || !first.bytes().all(|byte| byte.is_ascii_alphanumeric() || b".:%[]_-".contains(&byte))
    {
        return None;
    }
    if let Some(mapped) = first.strip_prefix("::ffff:")
        && mapped.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Some(mapped.to_owned());
    }
    Some(first.to_owned())
}

fn debug_stream_client(user_agent: Option<&str>) -> Option<String> {
    let user_agent = user_agent?;
    let browser = if user_agent.contains("Edg/")
        || user_agent.contains("EdgA/")
        || user_agent.contains("EdgiOS/")
        || user_agent.contains("EdgE/")
    {
        Some("Edge")
    } else if user_agent.contains("OPR/") || user_agent.contains("Opera") {
        Some("Opera")
    } else if user_agent.contains("Firefox/") || user_agent.contains("FxiOS/") {
        Some("Firefox")
    } else if user_agent.contains("Chrome/") || user_agent.contains("CriOS/") {
        Some("Chrome")
    } else if user_agent.contains("Version/") && user_agent.contains("Safari/") {
        Some("Safari")
    } else {
        None
    };
    let system = if user_agent.contains("iPhone") || user_agent.contains("iPad") || user_agent.contains("iPod") {
        Some("iOS")
    } else if user_agent.contains("Android") {
        Some("Android")
    } else if user_agent.contains("CrOS") {
        Some("ChromeOS")
    } else if user_agent.contains("Windows") {
        Some("Windows")
    } else if user_agent.contains("Macintosh") || user_agent.contains("Mac OS X") {
        Some("macOS")
    } else if user_agent.contains("Linux") {
        Some("Linux")
    } else {
        None
    };
    match (browser, system) {
        (Some(browser), Some(system)) => Some(format!("{browser} · {system}")),
        (Some(browser), None) => Some(browser.to_owned()),
        (None, Some(system)) => Some(system.to_owned()),
        (None, None) => None,
    }
}

impl TerminalManager {
    pub(super) fn create(
        &self,
        cwd: &Path,
        cols: u16,
        rows: u16,
        requested_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let cwd = cwd.canonicalize().context("Could not resolve terminal directory")?;
        if requested_id.is_some_and(|id| {
            id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }) {
            return Err(anyhow!("Invalid terminal id"));
        }
        let id = requested_id.map(str::to_owned).unwrap_or_else(random_id);
        if let Some(existing) = lock(&self.records).get(&id).cloned() {
            if existing.cwd != cwd {
                return Err(anyhow!("Terminal belongs to a different workspace"));
            }
            return Ok(id);
        }

        let pair = native_pty_system().openpty(PtySize {
            rows: rows.clamp(2, 1000),
            cols: cols.clamp(2, 1000),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let (program, args) = shell_command();
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(&cwd);
        command.env_remove("PI_WEB_PASSWORD");
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        if std::env::var_os("LANG").is_none()
            && std::env::var_os("LC_ALL").is_none()
            && std::env::var_os("LC_CTYPE").is_none()
        {
            command.env("LANG", "C.UTF-8");
        }
        let child = pair.slave.spawn_command(command).context("Could not start terminal shell")?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (events, _) = broadcast::channel(256);
        let record = Arc::new(TerminalRecord {
            cwd,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            output: Mutex::new(OutputState { text: String::new(), offset: 0 }),
            events,
            listeners: AtomicUsize::new(0),
            exited: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            cleanup_at: Mutex::new(None),
        });
        lock(&self.records).insert(id.clone(), record.clone());
        self.spawn_reader(reader, record.clone());
        self.spawn_waiter(record.clone());
        self.schedule_cleanup(id.clone(), record);
        Ok(id)
    }

    fn spawn_reader(&self, mut reader: Box<dyn Read + Send>, record: Arc<TerminalRecord>) {
        thread::spawn(move || {
            let mut bytes = [0_u8; 8192];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let data = String::from_utf8_lossy(&bytes[..count]).into_owned();
                        let mut output = lock(&record.output);
                        output.offset += data.encode_utf16().count() as u64;
                        output.text.push_str(&data);
                        trim_backlog(&mut output.text);
                        let event = TerminalEvent::Output { data, offset: output.offset, reset: None };
                        drop(output);
                        let _ = record.events.send(event);
                    }
                }
            }
        });
    }

    fn spawn_waiter(&self, record: Arc<TerminalRecord>) {
        let manager = self.clone();
        let id = lock(&self.records).iter().find_map(|(id, current)| Arc::ptr_eq(current, &record).then(|| id.clone()));
        let Some(id) = id else { return };
        thread::spawn(move || {
            loop {
                let status = lock(&record.child).try_wait();
                match status {
                    Ok(Some(status)) => {
                        let exit_code = status.exit_code() as i32;
                        record.exited.store(true, Ordering::Release);
                        *lock(&record.exit_code) = Some(exit_code);
                        let _ = record.events.send(TerminalEvent::Exit { exit_code });
                        manager.schedule_cleanup(id, record);
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(100)),
                    Err(_) => {
                        record.exited.store(true, Ordering::Release);
                        *lock(&record.exit_code) = Some(1);
                        let _ = record.events.send(TerminalEvent::Exit { exit_code: 1 });
                        manager.schedule_cleanup(id, record);
                        break;
                    }
                }
            }
        });
    }

    fn schedule_cleanup(&self, id: String, record: Arc<TerminalRecord>) {
        let due = now_ms() + RECONNECT_MS;
        *lock(&record.cleanup_at) = Some(due);
        let manager = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(RECONNECT_MS)).await;
            if record.listeners.load(Ordering::Acquire) != 0 {
                *lock(&record.cleanup_at) = None;
                return;
            }
            if lock(&record.cleanup_at).is_none_or(|due| due > now_ms()) {
                return;
            }
            let removed = {
                let mut records = lock(&manager.records);
                if records.get(&id).is_some_and(|current| Arc::ptr_eq(current, &record)) {
                    records.remove(&id);
                    true
                } else {
                    false
                }
            };
            if removed {
                *lock(&record.cleanup_at) = None;
                if !record.exited.load(Ordering::Acquire) {
                    let _ = lock(&record.child).kill();
                }
                let _ = record.events.send(TerminalEvent::Closed);
            }
        });
    }

    pub(super) fn cwd(&self, id: &str) -> Option<PathBuf> {
        lock(&self.records).get(id).map(|record| record.cwd.clone())
    }

    pub(super) fn write(&self, id: &str, data: &str) -> bool {
        let Some(record) = lock(&self.records).get(id).cloned() else { return false };
        if record.exited.load(Ordering::Acquire) {
            return false;
        }
        lock(&record.writer).write_all(data.as_bytes()).is_ok()
    }

    pub(super) fn resize(&self, id: &str, cols: u16, rows: u16) -> bool {
        let Some(record) = lock(&self.records).get(id).cloned() else { return false };
        if record.exited.load(Ordering::Acquire) {
            return false;
        }
        lock(&record.master)
            .resize(PtySize { rows: rows.clamp(2, 1000), cols: cols.clamp(2, 1000), pixel_width: 0, pixel_height: 0 })
            .is_ok()
    }

    pub(super) fn kill(&self, id: &str) {
        let Some(record) = lock(&self.records).remove(id) else { return };
        *lock(&record.cleanup_at) = None;
        if !record.exited.load(Ordering::Acquire) {
            let _ = lock(&record.child).kill();
        }
        let _ = record.events.send(TerminalEvent::Closed);
    }

    pub(super) fn shutdown(&self) {
        let ids = lock(&self.records).keys().cloned().collect::<Vec<_>>();
        for id in ids {
            self.kill(&id);
        }
    }

    pub(super) fn subscribe(&self, id: &str, after: Option<u64>) -> Option<TerminalSubscription> {
        let record = lock(&self.records).get(id).cloned()?;
        let receiver = record.events.subscribe();
        record.listeners.fetch_add(1, Ordering::AcqRel);
        *lock(&record.cleanup_at) = None;
        let output = lock(&record.output);
        let start = output.offset.saturating_sub(output.text.encode_utf16().count() as u64);
        let reset = after.is_none_or(|cursor| cursor < start || cursor > output.offset);
        let data = if reset {
            output.text.clone()
        } else {
            after.map(|cursor| slice_utf16(&output.text, cursor.saturating_sub(start) as usize)).unwrap_or_default()
        };
        let initial = TerminalEvent::Output { data, offset: output.offset, reset: Some(reset) };
        let exited = record.exited.load(Ordering::Acquire);
        let exit_code = *lock(&record.exit_code);
        Some(TerminalSubscription {
            initial,
            exited,
            exit_code,
            receiver,
            record: record.clone(),
            manager: self.clone(),
            id: id.to_owned(),
            last_offset: output.offset,
        })
    }

    pub(super) fn stats(&self) -> serde_json::Value {
        let records = lock(&self.records);
        let mut count = 0;
        let mut listeners = 0;
        let mut detached = 0;
        let mut exited = 0;
        let mut next_cleanup_at: Option<u64> = None;
        for record in records.values() {
            count += 1;
            let active = record.listeners.load(Ordering::Acquire);
            listeners += active;
            if active == 0 {
                detached += 1;
            }
            if record.exited.load(Ordering::Acquire) {
                exited += 1;
            }
            if let Some(due) = *lock(&record.cleanup_at) {
                next_cleanup_at = Some(next_cleanup_at.map_or(due, |current| current.min(due)));
            }
        }
        serde_json::json!({"count":count,"listeners":listeners,"detached":detached,"exited":exited,"nextCleanupAt":next_cleanup_at})
    }
}

impl TerminalSubscription {
    pub(super) async fn recv(&mut self) -> TerminalEvent {
        loop {
            match self.receiver.recv().await {
                Ok(event @ TerminalEvent::Output { offset, .. }) => {
                    if offset <= self.last_offset {
                        continue;
                    }
                    self.last_offset = offset;
                    return event;
                }
                Ok(event) => return event,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let output = lock(&self.record.output);
                    self.last_offset = output.offset;
                    return TerminalEvent::Output {
                        data: output.text.clone(),
                        offset: output.offset,
                        reset: Some(true),
                    };
                }
                Err(broadcast::error::RecvError::Closed) => return TerminalEvent::Closed,
            }
        }
    }
}

impl Drop for TerminalSubscription {
    fn drop(&mut self) {
        self.record.listeners.fetch_sub(1, Ordering::AcqRel);
        if self.record.listeners.load(Ordering::Acquire) == 0 {
            self.manager.schedule_cleanup(self.id.clone(), self.record.clone());
        }
    }
}

fn shell_command() -> (PathBuf, Vec<String>) {
    #[cfg(windows)]
    {
        if let Ok(config) = pi_coding_agent::tools::powershell_shell_config() {
            return (config.shell, config.args);
        }
        return (
            std::env::var_os("ComSpec").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("cmd.exe")),
            Vec::new(),
        );
    }
    #[cfg(not(windows))]
    {
        (std::env::var_os("SHELL").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/bin/sh")), vec!["-l".into()])
    }
}

fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    ring::rand::SystemRandom::new().fill(&mut bytes).expect("system RNG unavailable");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn trim_backlog(text: &mut String) {
    if text.len() <= MAX_BACKLOG_BYTES {
        return;
    }
    let mut trim_to = text.len() - MAX_BACKLOG_BYTES;
    while !text.is_char_boundary(trim_to) {
        trim_to += 1;
    }
    text.drain(..trim_to);
}

fn slice_utf16(text: &str, offset: usize) -> String {
    let mut units = 0;
    let mut byte = 0;
    for character in text.chars() {
        if units >= offset {
            break;
        }
        units += character.len_utf16();
        byte += character.len_utf8();
    }
    text[byte..].to_owned()
}
