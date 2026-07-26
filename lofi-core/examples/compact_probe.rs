use lofi_core::{compact, session::store, CompactOptions};
use lofi_types::{Role, SessionEventKind};
use std::{collections::HashMap, path::Path};

fn main() {
    let arg = std::env::args().nth(1).expect("session path");
    let path = Path::new(&arg);
    let (_, index, size) = store::load_index(path).unwrap();
    let by_id: HashMap<&str, usize> = index
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();
    let mut active = Vec::new();
    let mut cur = index.len().checked_sub(1);
    while let Some(i) = cur {
        active.push(i);
        cur = index[i]
            .parent_id
            .as_deref()
            .and_then(|id| by_id.get(id).copied());
    }
    active.reverse();
    let mut counts = HashMap::new();
    for &i in &active {
        *counts
            .entry(format!("{:?}", index[i].kind))
            .or_insert(0usize) += 1;
    }
    println!(
        "size={size} index={} active={} leaf={:?} counts={counts:?}",
        index.len(),
        active.len(),
        active.last().map(|&i| (&index[i].id, index[i].kind))
    );
    let events = store::load_compaction_path(path, &index, None).unwrap();
    println!("compaction_path={}", events.len());
    let mut msgs = 0;
    let mut roles = HashMap::new();
    let mut comps = 0;
    for e in &events {
        match &e.kind {
            SessionEventKind::Message(m) => {
                msgs += 1;
                *roles.entry(format!("{:?}", m.role)).or_insert(0usize) += 1;
            }
            SessionEventKind::Compaction {
                first_kept_entry_id,
                checkpointed_tail,
                ..
            } => {
                comps += 1;
                println!(
                    "marker id={} boundary={} checkpointed={}",
                    e.id, first_kept_entry_id, checkpointed_tail
                );
            }
            _ => {}
        }
    }
    println!("messages={msgs} roles={roles:?} comps={comps}");
    let opts = CompactOptions::default();
    match compact(&events, &opts) {
        Some(c) => println!(
            "COMPACT OK summarized={} kept={} boundary={:?} summary_bytes={}",
            c.summarized_count,
            c.kept_count,
            c.first_kept_event_id,
            c.summary.len()
        ),
        None => println!("COMPACT NONE"),
    }
    for e in events
        .iter()
        .filter(|e| matches!(e.kind, SessionEventKind::Message(_)))
        .take(8)
    {
        if let SessionEventKind::Message(m) = &e.kind {
            println!("msg {} {:?} blocks={}", e.id, m.role, m.blocks.len());
        }
    }
    for e in events
        .iter()
        .filter(|e| matches!(e.kind, SessionEventKind::Message(_)))
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if let SessionEventKind::Message(m) = &e.kind {
            println!("tail {} {:?} blocks={}", e.id, m.role, m.blocks.len());
        }
    }
}
