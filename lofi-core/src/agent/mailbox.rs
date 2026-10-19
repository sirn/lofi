//! Parent/child messaging between a session and its subagent runs.
//!
//! A child has no subagent support, so it cannot manage runs. It keeps only
//! this capability: it may write to `PARENT_ADDRESS` and read its own inbox.
//! The parent addresses live run ids. Authority comes from the capability,
//! never from the arguments, so a child cannot address a sibling.
//!
//! Every message takes one sequence number from a single counter, so the
//! parent sees one total order across all children. A retry with the same
//! key from the same sender returns the original sequence number instead of
//! a second message; the same key with different content is a conflict. A
//! full inbox rejects the send, so unread messages are never evicted.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::lock;

/// Address a child uses for its parent. The parent session has no run id.
pub(crate) const PARENT_ADDRESS: &str = "parent";

/// Cap on one message body.
const BODY_CAP: usize = 8 * 1024;
/// Cap on queued messages per recipient. A full inbox rejects the send.
const QUEUE_CAP: usize = 64;
/// Default receive wait. Below the guest exec timeout so a default wait
/// reports an empty inbox instead of dying with the exec.
const DEFAULT_RECEIVE_WAIT_MS: u64 = 60_000;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

#[derive(Default)]
struct State {
    /// One counter for the whole mailbox: total order across all senders.
    next_seq: u64,
    inboxes: HashMap<String, VecDeque<Value>>,
    /// Idempotency index: (sender, key) to (sequence, content digest).
    sent: HashMap<(String, String), (u64, u64)>,
    /// Addresses that may receive: live runs plus the parent.
    open: HashSet<String>,
    /// Addresses whose run ended, with the reason.
    closed: HashMap<String, String>,
    /// Messages a run never read before it ended. Kept out of the inbox so
    /// they are never delivered, but shown to the parent as an audit trail.
    undelivered: HashMap<String, Vec<Value>>,
    /// One wake-up per address. `notify_one` stores a permit, so a wake that
    /// lands between the queue check and the poll is not lost.
    wake: HashMap<String, Arc<Notify>>,
    /// A host wake for the parent inbox that the host has not taken yet.
    /// After a broadcast lag the host rebuilds only this wake: mail it was
    /// already told about must not start a second turn.
    parent_wake_unclaimed: bool,
}

/// Wake for a message that lands in the parent inbox. The mailbox knows
/// nothing about notices, so the registry supplies the sink.
pub(crate) type InboxSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Per-session mailbox shared by the parent exec and every child exec.
#[derive(Clone)]
pub(crate) struct Mailbox {
    state: Arc<Mutex<State>>,
    /// Append-only JSONL journal. None when the session does not persist;
    /// the mailbox then lives in memory only.
    journal: Arc<Mutex<Option<PathBuf>>>,
    /// Called when the parent inbox goes from empty to non-empty. Absent in
    /// a mailbox built outside a registry, unless a test installs one.
    sink: Option<InboxSink>,
}

impl Default for Mailbox {
    fn default() -> Self {
        let mailbox = Self {
            state: Arc::default(),
            journal: Arc::default(),
            sink: None,
        };
        // The parent is always addressable until a session clear.
        mailbox.open(PARENT_ADDRESS);
        mailbox
    }
}

/// A capability scoped to one child run: its own inbox and the right to
/// write to the parent. Cloned into the child agent; it never carries the
/// parent's registry, so it cannot form an ownership cycle.
///
/// The scope names the run, not one invocation: a resumed run reuses the id,
/// so work a timed-out invocation left behind in its sandbox still sends and
/// receives as that run. An invocation counter would sever it, but the run id
/// is the model's identity across resumes, and a stale invocation can only
/// reach its own parent and its own inbox.
#[derive(Clone)]
pub(crate) struct MailboxScope {
    pub(crate) mailbox: Mailbox,
    pub(crate) me: String,
}

impl Mailbox {
    /// Install the wake for messages that land in the parent inbox.
    #[must_use]
    pub(crate) fn with_sink(mut self, sink: InboxSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Whether the parent inbox holds unread mail.
    #[cfg(test)]
    pub(crate) fn parent_inbox_pending(&self) -> bool {
        lock(&self.state)
            .inboxes
            .get(PARENT_ADDRESS)
            .is_some_and(|queue| !queue.is_empty())
    }

    /// Sender of the oldest message in the parent inbox. A wake names one
    /// sender, so the host needs it when a broadcast lag dropped the wake.
    pub(crate) fn parent_inbox_front(&self) -> Option<String> {
        parent_front(&lock(&self.state))
    }

    /// Whether a host wake for the parent inbox is still untaken. Only then
    /// can a broadcast lag have dropped it.
    pub(crate) fn parent_wake_unclaimed(&self) -> bool {
        lock(&self.state).parent_wake_unclaimed
    }

    /// Take the parent wake for a turn the host is about to start. False
    /// when the wake was already taken, or when the turn that ended read the
    /// inbox itself. One shot under one lock: a notice the receiver retained
    /// across the claim cannot start a second turn.
    pub(crate) fn claim_parent_wake(&self) -> bool {
        let mut st = lock(&self.state);
        let start = st.parent_wake_unclaimed
            && st
                .inboxes
                .get(PARENT_ADDRESS)
                .is_some_and(|queue| !queue.is_empty());
        st.parent_wake_unclaimed = false;
        start
    }

    /// Point the mailbox at a session journal and replay what it holds.
    pub(crate) fn attach_journal(&self, path: &Path) {
        // Cheap check first: this runs on every spawn, and the journal only
        // grows.
        if lock(&self.journal).is_some() {
            return;
        }
        // A journal that does not exist yet is an empty one: the first
        // append creates the file.
        let text = std::fs::read_to_string(path).unwrap_or_default();
        // The state lock spans the attach and the replay: a send that ran
        // between them would append a sequence the replay then queues a
        // second time under the same number.
        let mut st = lock(&self.state);
        {
            // One attach per session: a second spawn must not replay the
            // journal again on top of the pending it already produced.
            let mut journal = lock(&self.journal);
            if journal.is_some() {
                return;
            }
            *journal = Some(path.to_path_buf());
        }
        replay(&mut st, &text);
        // Replayed mail was sent before this session opened: no send wakes
        // the host for it, and the next send will not either, because it
        // lands in an inbox that is no longer empty. Wake once here, or a
        // resumed session keeps unread mail that nothing points at.
        let restored = parent_front(&st);
        if restored.is_some() {
            st.parent_wake_unclaimed = true;
        }
        drop(st);
        if let (Some(sender), Some(sink)) = (restored, &self.sink) {
            sink(&sender);
        }
    }

    /// Register a run id as addressable.
    pub(crate) fn open(&self, id: &str) {
        let mut st = lock(&self.state);
        st.closed.remove(id);
        st.open.insert(id.to_string());
        // A resume continues the same conversation, so what the previous run
        // never read is delivered to it now instead of staying audit-only.
        let queued = st.undelivered.remove(id).unwrap_or_default();
        let inbox = st.inboxes.entry(id.to_string()).or_default();
        for mut message in queued {
            if let Some(object) = message.as_object_mut() {
                object.remove("undelivered");
            }
            inbox.push_back(message);
        }
    }

    /// Close an address: queued messages stay visible to the parent as
    /// undelivered, later sends are rejected, and a blocked receiver wakes
    /// with a closed result.
    pub(crate) fn close(&self, id: &str, reason: &str) {
        let wake = {
            let mut st = lock(&self.state);
            st.open.remove(id);
            st.closed.insert(id.to_string(), reason.to_string());
            let queued: VecDeque<Value> = st
                .inboxes
                .get_mut(id)
                .map(std::mem::take)
                .unwrap_or_default();
            for mut message in queued {
                if let Some(seq) = message["seq"].as_u64() {
                    // The record belongs to this critical section: a session
                    // reset must not leave it for the next journal, where the
                    // same sequence number names a different message.
                    let _ = self.append(&json!({
                        "kind": "undelivered",
                        "to": id,
                        "seq": seq,
                        "reason": reason,
                    }));
                }
                message["undelivered"] = json!(reason);
                st.undelivered
                    .entry(id.to_string())
                    .or_default()
                    .push(message);
            }
            st.wake.get(id).cloned()
        };
        if let Some(wake) = wake {
            // notify_one wakes a single waiter; a closed address must wake
            // every receive blocked on it, or the others wait out the deadline.
            wake.notify_one();
            wake.notify_waiters();
        }
    }

    /// Close every child address. The parent stays addressable so a new
    /// session does not inherit stale conversations.
    pub(crate) fn close_children(&self, reason: &str) {
        let ids: Vec<String> = {
            let st = lock(&self.state);
            st.open
                .iter()
                .filter(|id| id.as_str() != PARENT_ADDRESS)
                .cloned()
                .collect()
        };
        for id in ids {
            self.close(&id, reason);
        }
        // The next session must inherit nothing: no inbox, no address, no
        // idempotency key, no wake handle, and no journal path (or it would
        // keep writing into the previous session's file). The journal lock
        // nests inside the state lock, as it does in append.
        let wakes: Vec<Arc<Notify>> = {
            let mut st = lock(&self.state);
            *lock(&self.journal) = None;
            let wakes = st.wake.values().cloned().collect();
            *st = State::default();
            // Reopen the parent in the same hold: a parent receive in the gap
            // would find it neither open nor closed and report a false end.
            st.open.insert(PARENT_ADDRESS.to_string());
            wakes
        };
        // Wake every discarded waiter after the reset: a child then finds its
        // address neither open nor closed, and a parent finds the wake handle
        // it was given replaced. Either way it reports the end.
        for wake in wakes {
            wake.notify_waiters();
        }
    }

    /// Send one message. manage is set for the parent, the only caller
    /// allowed to address runs by id.
    pub(crate) fn send(&self, me: &str, manage: bool, args: &Value) -> Value {
        let SendArgs {
            to,
            body,
            reply_to,
            key,
        } = match send_args(args) {
            Ok(send) => send,
            Err(body) => return body,
        };
        let digest = digest(&to, reply_to, &body);
        let mut st = lock(&self.state);
        // A retry must resolve to the first answer even when the recipient has
        // ended in between: the message was already accepted.
        if let Some(key) = &key {
            if let Some((seq, previous)) = st.sent.get(&(me.to_string(), key.clone())).copied() {
                if previous == digest {
                    return json!({ "ok": true, "seq": seq, "duplicate": true });
                }
                return json!({
                    "ok": false,
                    "status": "conflict",
                    "error": "key was already used with a different message",
                });
            }
        }
        if manage {
            if to == PARENT_ADDRESS {
                return forbidden("the parent cannot address itself");
            }
            if let Some(reason) = st.closed.get(&to) {
                return json!({
                    "ok": false,
                    "status": "recipient_ended",
                    "error": format!("recipient ended: {reason}"),
                });
            }
            if !st.open.contains(&to) {
                return json!({
                    "ok": false,
                    "status": "unknown_recipient",
                    "error": "unknown subagent id",
                });
            }
        } else {
            if to != PARENT_ADDRESS {
                return forbidden("a subagent may address only parent");
            }
            // A run that already ended must not write into the parent inbox:
            // its lingering sandbox work can still be running after a settle
            // or a session switch.
            if !st.open.contains(me) {
                return json!({
                    "ok": false,
                    "status": "sender_ended",
                    "error": "this run has ended; the message was not sent",
                });
            }
        }
        if st
            .inboxes
            .get(&to)
            .is_some_and(|queue| queue.len() >= QUEUE_CAP)
        {
            return json!({
                "ok": false,
                "status": "full",
                "error": format!("recipient inbox holds {QUEUE_CAP} unread messages"),
            });
        }
        let seq = st.next_seq;
        // The append happens under the state lock: a message must not become
        // visible unless the durable record exists.
        if let Err(e) = self.append(&json!({
            "kind": "send",
            "seq": seq,
            "from": me,
            "to": to,
            "reply_to": reply_to,
            "body": body,
            "key": key,
        })) {
            return json!({ "ok": false, "status": "persist_failed", "error": e });
        }
        st.next_seq += 1;
        // Only a drain of the parent inbox arms the next wake, so a
        // supervisor turn is woken once per batch, not once per message.
        let wake_host = to == PARENT_ADDRESS && fills_empty_inbox(&st, &to);
        if wake_host {
            st.parent_wake_unclaimed = true;
        }
        st.inboxes.entry(to.clone()).or_default().push_back(json!({
            "seq": seq,
            "from": me,
            "to": to,
            "reply_to": reply_to,
            "body": body,
        }));
        if let Some(key) = key {
            st.sent.insert((me.to_string(), key), (seq, digest));
        }
        let wake = st
            .wake
            .entry(to.clone())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone();
        drop(st);
        self.wake_after_send(me, wake_host, &wake);
        json!({ "ok": true, "seq": seq, "to": to })
    }

    /// Wake a receiver blocked in `receive`. When the message filled an
    /// empty parent inbox, wake the host too: a blocked receive may drain the
    /// inbox first, so the host re-checks before it starts a turn.
    fn wake_after_send(&self, me: &str, wake_host: bool, wake: &Arc<Notify>) {
        wake.notify_one();
        if wake_host {
            if let Some(sink) = &self.sink {
                sink(me);
            }
        }
    }

    /// Take up to limit messages from the caller's inbox, waiting at most
    /// `wait_ms` for the first one.
    pub(crate) async fn receive(&self, me: &str, args: &Value) -> Value {
        let limit = match u64_arg(args, "limit") {
            Ok(limit) => limit
                .unwrap_or(DEFAULT_LIMIT as u64)
                .clamp(1, MAX_LIMIT as u64) as usize,
            Err(body) => return body,
        };
        let wait = match u64_arg(args, "wait_ms") {
            Ok(ms) => Duration::from_millis(ms.unwrap_or(0).min(DEFAULT_RECEIVE_WAIT_MS)),
            Err(body) => return body,
        };
        // The wake handle must be the one the senders signal, so register it
        // here rather than building one that nobody else can see.
        let wake = {
            let mut st = lock(&self.state);
            st.wake
                .entry(me.to_string())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        let deadline = tokio::time::sleep(wait);
        tokio::pin!(deadline);
        loop {
            // Register before the check, and enable the registration: an
            // un-polled Notified has not subscribed yet, so a wake that lands
            // between the check and the select would be missed.
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (messages, ended) = {
                let mut st = lock(&self.state);
                // An address that is neither open nor closed was discarded
                // with the session that owned it, and so was the wake handle
                // this call waits on: report the end instead of sleeping on a
                // handle that no sender can reach.
                let ended = !st.open.contains(me)
                    || !st.wake.get(me).is_some_and(|now| Arc::ptr_eq(now, &wake));
                let mut out = Vec::new();
                if let Some(queue) = st.inboxes.get_mut(me) {
                    while out.len() < limit {
                        match queue.pop_front() {
                            Some(message) => out.push(message),
                            None => break,
                        }
                    }
                }
                if !out.is_empty() {
                    // The record belongs to the same critical section as the
                    // pop: a session switch must not move it into a journal
                    // that never held the send.
                    let seqs: Vec<u64> = out
                        .iter()
                        .filter_map(|message| message["seq"].as_u64())
                        .collect();
                    let _ = self.append(&json!({ "kind": "receive", "by": me, "seqs": seqs }));
                }
                (out, ended)
            };
            if !messages.is_empty() {
                return json!({ "ok": true, "messages": messages });
            }
            if ended {
                return json!({ "ok": true, "messages": [], "closed": true });
            }
            if wait.is_zero() {
                return json!({ "ok": true, "messages": [], "empty": true });
            }
            tokio::select! {
                biased;
                () = &mut deadline => {
                    return json!({ "ok": true, "messages": [], "empty": true, "timed_out": true });
                }
                () = notified => {}
            }
        }
    }

    /// Pending messages: for the caller's inbox, or - when the caller is the
    /// parent - for one named run. Without `agent_id` the parent gets counts.
    pub(crate) fn list(&self, me: &str, manage: bool, args: &Value) -> Value {
        let agent_id = match opt_str(args, "agent_id") {
            Ok(value) => value,
            Err(body) => return body,
        };
        let st = lock(&self.state);
        match agent_id {
            Some(id) => {
                if !manage && id != me {
                    return forbidden("a subagent may list only its own mailbox");
                }
                let mut messages: Vec<Value> = st.undelivered.get(&id).cloned().unwrap_or_default();
                if let Some(queue) = st.inboxes.get(&id) {
                    messages.extend(queue.iter().cloned());
                }
                json!({ "ok": true, "agent_id": id, "messages": messages })
            }
            None if manage => {
                let mut inboxes: Vec<Value> = st
                    .inboxes
                    .iter()
                    .map(|(id, queue)| json!({ "agent_id": id, "pending": queue.len() }))
                    .collect();
                inboxes.sort_by(|a, b| a["agent_id"].as_str().cmp(&b["agent_id"].as_str()));
                let messages: Vec<Value> = st
                    .inboxes
                    .get(PARENT_ADDRESS)
                    .map(|queue| queue.iter().cloned().collect())
                    .unwrap_or_default();
                json!({ "ok": true, "messages": messages, "inboxes": inboxes })
            }
            None => {
                let messages: Vec<Value> = st
                    .inboxes
                    .get(me)
                    .map(|queue| queue.iter().cloned().collect())
                    .unwrap_or_default();
                json!({ "ok": true, "agent_id": me, "messages": messages })
            }
        }
    }

    /// Append one journal line. An absent journal means memory only.
    fn append(&self, line: &Value) -> std::result::Result<(), String> {
        // Hold the lock across the whole line: writeln can issue more than
        // one write, and two writers must not interleave fragments of
        // different records in an append-only journal.
        let journal = lock(&self.journal);
        let Some(path) = journal.as_ref() else {
            return Ok(());
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            // Message bodies are private: the journal must not be readable
            // by other users, like every other state file.
            .mode(0o600)
            .open(path)
            .map_err(|e| e.to_string())?;
        writeln!(file, "{line}").map_err(|e| e.to_string())
    }
}

/// Sender of the oldest message in the parent inbox.
fn parent_front(st: &State) -> Option<String> {
    st.inboxes
        .get(PARENT_ADDRESS)
        .and_then(|queue| queue.front())
        .and_then(|message| message["from"].as_str())
        .map(str::to_string)
}

/// Whether one send takes the recipient's inbox from empty to non-empty.
fn fills_empty_inbox(st: &State, to: &str) -> bool {
    st.inboxes.get(to).is_none_or(VecDeque::is_empty)
}

/// Replay a journal into pending state: sends minus receives and
/// undelivered marks, in sequence order.
fn replay(st: &mut State, text: &str) {
    let mut sends: Vec<Value> = Vec::new();
    let mut settled: HashSet<u64> = HashSet::new();
    // Sequence to reason, for messages a run never read.
    let mut undelivered: HashMap<u64, String> = HashMap::new();
    let mut next_seq = 0;
    for line in text.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["kind"].as_str() {
            Some("send") => {
                if let Some(seq) = event["seq"].as_u64() {
                    next_seq = next_seq.max(seq + 1);
                }
                sends.push(event);
            }
            Some("receive") => {
                for seq in event["seqs"].as_array().into_iter().flatten() {
                    if let Some(seq) = seq.as_u64() {
                        settled.insert(seq);
                    }
                }
            }
            Some("undelivered") => {
                if let Some(seq) = event["seq"].as_u64() {
                    let reason = event["reason"].as_str().unwrap_or("ended");
                    undelivered.insert(seq, reason.to_string());
                }
            }
            _ => {}
        }
    }
    st.next_seq = st.next_seq.max(next_seq);
    for send in sends {
        let Some(seq) = send["seq"].as_u64() else {
            continue;
        };
        let to = send["to"].as_str().unwrap_or_default().to_string();
        // A keyed send rebuilds the idempotency index, or a retry after a
        // restart would send the message a second time.
        if let Some(key) = send["key"].as_str() {
            let from = send["from"].as_str().unwrap_or_default().to_string();
            let body = send["body"].as_str().unwrap_or_default();
            let reply_to = send["reply_to"].as_u64();
            st.sent
                .insert((from, key.to_string()), (seq, digest(&to, reply_to, body)));
        }
        // A receive wins over an undelivered mark: a message marked
        // undelivered, redelivered by a resume and then read must not come
        // back as undelivered after a restart.
        if settled.contains(&seq) {
            continue;
        }
        if let Some(reason) = undelivered.get(&seq) {
            st.undelivered.entry(to).or_default().push(json!({
                "seq": seq,
                "from": send["from"],
                "to": send["to"],
                "reply_to": send["reply_to"],
                "body": send["body"],
                "undelivered": reason,
            }));
            continue;
        }
        st.inboxes.entry(to).or_default().push_back(json!({
            "seq": seq,
            "from": send["from"],
            "to": send["to"],
            "reply_to": send["reply_to"],
            "body": send["body"],
        }));
    }
}

struct SendArgs {
    to: String,
    body: String,
    reply_to: Option<u64>,
    key: Option<String>,
}

/// Parse and bound-check the send arguments before the state lock is taken.
fn send_args(args: &Value) -> std::result::Result<SendArgs, Value> {
    let to = match opt_str(args, "to") {
        Ok(Some(to)) => to,
        Ok(None) => return Err(invalid("to is required and must be a non-empty string")),
        Err(body) => return Err(body),
    };
    let body = match opt_str(args, "body") {
        Ok(Some(body)) => body,
        Ok(None) => return Err(invalid("body is required and must be a non-empty string")),
        Err(bad) => return Err(bad),
    };
    let reply_to = u64_arg(args, "reply_to")?;
    let key = opt_str(args, "key")?;
    if body.len() > BODY_CAP {
        return Err(json!({
            "ok": false,
            "status": "too_large",
            "error": format!("body exceeds {BODY_CAP} bytes"),
        }));
    }
    Ok(SendArgs {
        to,
        body,
        reply_to,
        key,
    })
}

fn digest(to: &str, reply_to: Option<u64>, body: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    to.hash(&mut hasher);
    reply_to.hash(&mut hasher);
    body.hash(&mut hasher);
    hasher.finish()
}

fn invalid(message: &str) -> Value {
    json!({ "ok": false, "status": "failed", "error": message })
}

fn forbidden(message: &str) -> Value {
    json!({ "ok": false, "status": "forbidden", "error": message })
}

/// Absent, null and empty string all mean not given; any other non-string is
/// an argument error.
fn opt_str(args: &Value, name: &str) -> std::result::Result<Option<String>, Value> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(invalid(&format!("{name} must be a string"))),
    }
}

fn u64_arg(args: &Value, name: &str) -> std::result::Result<Option<u64>, Value> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(number) => Ok(Some(number)),
            None => Err(invalid(&format!("{name} must be a non-negative integer"))),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn mailbox() -> Mailbox {
        let mailbox = Mailbox::default();
        mailbox.open("child-a");
        mailbox.open("child-b");
        mailbox
    }

    /// A mailbox whose parent-inbox wakes are recorded instead of published.
    fn woken() -> (Mailbox, Arc<Mutex<Vec<String>>>) {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink: InboxSink = {
            let seen = seen.clone();
            Arc::new(move |sender: &str| lock(&seen).push(sender.to_string()))
        };
        let mailbox = mailbox().with_sink(sink);
        (mailbox, seen)
    }

    #[test]
    fn a_filled_inbox_wakes_the_host_once() {
        let (mailbox, seen) = woken();
        let first = mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "one" }),
        );
        let second = mailbox.send(
            "child-b",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "two" }),
        );
        assert_eq!(first["ok"], true, "{first}");
        assert_eq!(second["ok"], true, "{second}");
        assert_eq!(*lock(&seen), vec!["child-a".to_string()]);
    }

    #[test]
    fn a_replayed_inbox_wakes_the_host_once() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("mailbox-test.jsonl");
        let (mailbox, seen) = woken();
        mailbox.attach_journal(&journal);
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "one" }),
        );
        assert_eq!(lock(&seen).len(), 1);
        // A restart reads the same journal into a fresh mailbox: nothing
        // sends, so the replay itself must raise the wake.
        let (restored, seen) = woken();
        restored.attach_journal(&journal);
        assert_eq!(*lock(&seen), vec!["child-a".to_string()]);
        assert!(restored.parent_inbox_pending());
        // A second attach is not a second replay.
        restored.attach_journal(&journal);
        assert_eq!(lock(&seen).len(), 1);
    }

    #[test]
    fn a_list_does_not_arm_another_wake() {
        let (mailbox, seen) = woken();
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "one" }),
        );
        let listed = mailbox.list(PARENT_ADDRESS, true, &json!({}));
        assert_eq!(listed["messages"].as_array().map(Vec::len), Some(1));
        mailbox.send(
            "child-b",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "two" }),
        );
        assert_eq!(lock(&seen).len(), 1);
        assert!(mailbox.parent_inbox_pending());
    }

    #[test]
    fn a_claim_takes_the_wake_once() {
        let (mailbox, seen) = woken();
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "one" }),
        );
        // The notice may still be in flight while the host claims: the claim
        // decides, so the second one finds the wake spent.
        assert!(mailbox.claim_parent_wake());
        assert!(!mailbox.claim_parent_wake());
        assert!(mailbox.parent_inbox_pending());
        // Only a drain arms the next wake.
        mailbox.send(
            "child-b",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "two" }),
        );
        assert_eq!(lock(&seen).len(), 1);
    }

    #[tokio::test]
    async fn a_drained_wake_does_not_start_a_turn() {
        let (mailbox, seen) = woken();
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "one" }),
        );
        // The turn the wake started read the mail itself: nothing is left for
        // the turn the host was about to start.
        mailbox.receive(PARENT_ADDRESS, &json!({})).await;
        assert!(!mailbox.claim_parent_wake());
        mailbox.send(
            "child-b",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "two" }),
        );
        assert!(mailbox.claim_parent_wake());
        assert_eq!(lock(&seen).len(), 2);
    }

    #[tokio::test]
    async fn a_drained_inbox_wakes_the_host_again() {
        let (mailbox, seen) = woken();
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "one" }),
        );
        let read = mailbox.receive(PARENT_ADDRESS, &json!({})).await;
        assert_eq!(read["messages"].as_array().map(Vec::len), Some(1));
        assert!(!mailbox.parent_inbox_pending());
        mailbox.send(
            "child-b",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "two" }),
        );
        assert_eq!(
            *lock(&seen),
            vec!["child-a".to_string(), "child-b".to_string()]
        );
    }

    #[test]
    fn a_message_to_a_child_inbox_does_not_wake_the_host() {
        let (mailbox, seen) = woken();
        let out = mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "hi" }),
        );
        assert_eq!(out["ok"], true);
        assert!(lock(&seen).is_empty());
    }

    #[test]
    fn the_parent_inbox_reports_pending_mail() {
        let mailbox = mailbox();
        assert!(!mailbox.parent_inbox_pending());
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "hi" }),
        );
        assert!(mailbox.parent_inbox_pending());
    }

    #[test]
    fn a_child_addresses_only_the_parent() {
        let mailbox = mailbox();
        let out = mailbox.send("child-a", false, &json!({ "to": "child-b", "body": "hi" }));
        assert_eq!(out["status"], "forbidden");
    }

    #[test]
    fn the_parent_addresses_runs_by_id_only() {
        let mailbox = mailbox();
        let unknown = mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "nobody", "body": "hi" }),
        );
        assert_eq!(unknown["status"], "unknown_recipient");
        let itself = mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": PARENT_ADDRESS, "body": "hi" }),
        );
        assert_eq!(itself["status"], "forbidden");
    }

    #[test]
    fn a_retry_with_the_same_key_returns_the_first_sequence() {
        let mailbox = mailbox();
        let args = json!({ "to": PARENT_ADDRESS, "body": "hi", "key": "k1" });
        let first = mailbox.send("child-a", false, &args);
        let second = mailbox.send("child-a", false, &args);
        assert_eq!(first["seq"], second["seq"]);
        assert_eq!(second["duplicate"], true);
    }

    #[test]
    fn the_same_key_with_a_different_body_conflicts() {
        let mailbox = mailbox();
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "hi", "key": "k1" }),
        );
        let out = mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "other", "key": "k1" }),
        );
        assert_eq!(out["status"], "conflict");
    }

    #[test]
    fn a_full_inbox_rejects_the_send_instead_of_evicting() {
        let mailbox = mailbox();
        for index in 0..QUEUE_CAP {
            let out = mailbox.send(
                "child-a",
                false,
                &json!({ "to": PARENT_ADDRESS, "body": index.to_string() }),
            );
            assert_eq!(out["ok"], true);
        }
        let out = mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "overflow" }),
        );
        assert_eq!(out["status"], "full");
        let listed = mailbox.list(PARENT_ADDRESS, true, &json!({}));
        assert_eq!(listed["messages"].as_array().map(Vec::len), Some(QUEUE_CAP));
    }

    #[test]
    fn a_body_over_the_cap_is_rejected() {
        let mailbox = mailbox();
        let body = "x".repeat(BODY_CAP + 1);
        let out = mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": body }),
        );
        assert_eq!(out["status"], "too_large");
    }

    #[test]
    fn closing_an_address_rejects_later_sends_and_keeps_the_audit() {
        let mailbox = mailbox();
        mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "task" }),
        );
        mailbox.close("child-a", "cancelled");
        let out = mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "more" }),
        );
        assert_eq!(out["status"], "recipient_ended");
        let listed = mailbox.list(PARENT_ADDRESS, true, &json!({ "agent_id": "child-a" }));
        assert_eq!(listed["messages"][0]["undelivered"], "cancelled");
    }

    #[tokio::test]
    async fn an_undelivered_message_survives_a_receive() {
        let mailbox = mailbox();
        mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "task" }),
        );
        mailbox.close("child-a", "cancelled");
        // A receive must not deliver or forget the message.
        let read = mailbox.receive("child-a", &json!({})).await;
        assert_eq!(read["closed"], true);
        assert!(read["messages"].as_array().unwrap().is_empty());
        let listed = mailbox.list(PARENT_ADDRESS, true, &json!({ "agent_id": "child-a" }));
        assert_eq!(listed["messages"][0]["body"], "task");
        assert_eq!(listed["messages"][0]["undelivered"], "cancelled");
    }

    #[tokio::test]
    async fn a_resume_delivers_what_the_previous_run_never_read() {
        let mailbox = mailbox();
        mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "task" }),
        );
        mailbox.close("child-a", "cancelled");
        mailbox.open("child-a");
        let read = mailbox.receive("child-a", &json!({})).await;
        let messages = read["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["body"], "task");
        assert!(messages[0].get("undelivered").is_none());
    }

    #[test]
    fn a_child_cannot_send_after_its_run_ends() {
        let mailbox = mailbox();
        mailbox.close("child-a", "completed");
        let out = mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "late" }),
        );
        assert_eq!(out["status"], "sender_ended", "{out}");
        let listed = mailbox.list(PARENT_ADDRESS, true, &json!({}));
        assert!(
            listed["messages"].as_array().unwrap().is_empty(),
            "{listed}"
        );
    }

    #[test]
    fn a_session_clear_leaves_no_trace_for_the_next_session() {
        let mailbox = mailbox();
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "old news", "key": "k1" }),
        );
        mailbox.close_children("session cleared");
        // No parent inbox, and the old idempotency key is gone with it.
        let listed = mailbox.list(PARENT_ADDRESS, true, &json!({}));
        assert!(
            listed["messages"].as_array().unwrap().is_empty(),
            "{listed}"
        );
        mailbox.open("child-b");
        let retried = mailbox.send(
            "child-b",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "old news", "key": "k1" }),
        );
        assert_eq!(retried["ok"], true, "{retried}");
        assert!(retried.get("duplicate").is_none(), "{retried}");
    }

    #[test]
    fn a_session_clear_detaches_the_old_journal() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.jsonl");
        let fresh = dir.path().join("new.jsonl");
        let mailbox = mailbox();
        mailbox.attach_journal(&old);
        mailbox.close_children("session cleared");
        // A new session must attach its own journal, not be told one is attached.
        mailbox.attach_journal(&fresh);
        mailbox.open("child-a");
        let sent = mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "new session" }),
        );
        assert_eq!(sent["ok"], true, "{sent}");
        let new_lines = std::fs::read_to_string(&fresh).unwrap();
        assert!(new_lines.contains("new session"), "{new_lines}");
        let old_lines = std::fs::read_to_string(&old).unwrap_or_default();
        assert!(!old_lines.contains("new session"), "{old_lines}");
    }

    #[tokio::test]
    async fn reply_to_is_the_numeric_sequence_of_the_message_answered() {
        let mailbox = mailbox();
        let first = mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "question" }),
        );
        let seq = first["seq"].as_u64().unwrap();
        let answer = mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "answer", "reply_to": seq }),
        );
        assert_eq!(answer["ok"], true, "{answer}");
        let read = mailbox.receive("child-a", &json!({})).await;
        let messages = read["messages"].as_array().unwrap();
        assert_eq!(messages[0]["reply_to"], json!(seq), "{read}");
    }

    #[test]
    fn a_keyed_send_survives_a_restart_for_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mailbox.jsonl");
        let mailbox = mailbox();
        mailbox.attach_journal(&path);
        let first = mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "task", "key": "k1" }),
        );
        assert_eq!(first["ok"], true, "{first}");

        // A fresh store over the same journal: the retry is still a duplicate.
        let restarted = Mailbox::default();
        restarted.open("child-a");
        restarted.attach_journal(&path);
        let retried = restarted.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "task", "key": "k1" }),
        );
        assert_eq!(retried["duplicate"], json!(true), "{retried}");
        assert_eq!(retried["seq"], first["seq"], "{retried}");
        let conflicted = restarted.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "other", "key": "k1" }),
        );
        assert_eq!(conflicted["status"], "conflict", "{conflicted}");
    }

    #[tokio::test]
    async fn a_read_message_is_not_restored_as_undelivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mailbox.jsonl");
        let mailbox = mailbox();
        mailbox.attach_journal(&path);
        mailbox.send(
            PARENT_ADDRESS,
            true,
            &json!({ "to": "child-a", "body": "task" }),
        );
        mailbox.close("child-a", "completed");
        // A resume delivers it and the child reads it.
        mailbox.open("child-a");
        let read = mailbox.receive("child-a", &json!({})).await;
        assert_eq!(read["messages"].as_array().unwrap().len(), 1, "{read}");

        // After a restart the message is gone: it was delivered and read.
        let restarted = Mailbox::default();
        restarted.attach_journal(&path);
        let listed = restarted.list(PARENT_ADDRESS, true, &json!({ "agent_id": "child-a" }));
        assert!(
            listed["messages"].as_array().unwrap().is_empty(),
            "{listed}"
        );
    }

    #[test]
    fn the_parent_sees_one_order_across_children() {
        let mailbox = mailbox();
        let a = mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "a1" }),
        );
        let b = mailbox.send(
            "child-b",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "b1" }),
        );
        assert!(a["seq"].as_u64() < b["seq"].as_u64());
    }

    #[tokio::test]
    async fn receive_returns_pending_messages_oldest_first() {
        let mailbox = mailbox();
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "a1" }),
        );
        mailbox.send(
            "child-b",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "b1" }),
        );
        let out = mailbox.receive(PARENT_ADDRESS, &json!({})).await;
        assert_eq!(out["messages"].as_array().map(Vec::len), Some(2));
        assert_eq!(out["messages"][0]["body"], "a1");
        assert_eq!(out["messages"][1]["body"], "b1");
    }

    #[tokio::test]
    async fn receive_without_a_wait_reports_an_empty_inbox() {
        let mailbox = mailbox();
        let out = mailbox.receive(PARENT_ADDRESS, &json!({})).await;
        assert_eq!(out["empty"], true);
    }

    #[tokio::test]
    async fn receive_waits_for_a_late_message() {
        let mailbox = mailbox();
        let sender = mailbox.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            sender.send(
                "child-a",
                false,
                &json!({ "to": PARENT_ADDRESS, "body": "late" }),
            );
        });
        let out = mailbox
            .receive(PARENT_ADDRESS, &json!({ "wait_ms": 5_000 }))
            .await;
        assert_eq!(out["messages"][0]["body"], "late");
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn receive_reports_a_closed_inbox() {
        let mailbox = mailbox();
        mailbox.close("child-a", "completed");
        let out = mailbox
            .receive("child-a", &json!({ "wait_ms": 5_000 }))
            .await;
        assert_eq!(out["closed"], true);
    }

    #[tokio::test]
    async fn a_journal_replays_only_undelivered_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mailbox.jsonl");
        let mailbox = Mailbox::default();
        mailbox.attach_journal(&path);
        mailbox.open("child-a");
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "first" }),
        );
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "second" }),
        );
        let taken = mailbox
            .receive(PARENT_ADDRESS, &json!({ "limit": 1 }))
            .await;
        assert_eq!(taken["messages"][0]["body"], "first");

        let restored = Mailbox::default();
        restored.attach_journal(&path);
        let pending = restored.receive(PARENT_ADDRESS, &json!({})).await;
        assert_eq!(pending["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(pending["messages"][0]["body"], "second");
    }

    #[tokio::test]
    async fn a_session_clear_wakes_a_blocked_receive() {
        let mailbox = mailbox();
        let waiting = {
            let mailbox = mailbox.clone();
            tokio::spawn(async move {
                mailbox
                    .receive("child-a", &json!({ "wait_ms": 60_000 }))
                    .await
            })
        };
        // Let the receive reach its blocked state before the clear.
        tokio::task::yield_now().await;
        mailbox.close_children("session cleared");
        let out = waiting.await.unwrap();
        // The address is gone, so the wait ends now instead of at the
        // deadline.
        assert_eq!(out["closed"], json!(true), "{out}");
    }

    #[test]
    fn the_journal_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mailbox.jsonl");
        let mailbox = mailbox();
        mailbox.attach_journal(&path);
        mailbox.send(
            "child-a",
            false,
            &json!({ "to": PARENT_ADDRESS, "body": "secret" }),
        );
        let mode = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&path).unwrap());
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[tokio::test]
    async fn a_receive_after_the_session_is_discarded_reports_the_end() {
        let mailbox = mailbox();
        mailbox.close_children("session cleared");
        let out = mailbox
            .receive("child-a", &json!({ "wait_ms": 60_000 }))
            .await;
        assert_eq!(out["closed"], json!(true), "{out}");
    }
}
