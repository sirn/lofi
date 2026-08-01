use std::io::Read as _;

fn rss_kb() -> u64 {
    let mut s = String::new();
    std::fs::File::open("/proc/self/status").unwrap().read_to_string(&mut s).unwrap();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().unwrap();
        }
    }
    0
}

fn main() {
    let store = lofi_core::session::store::SessionStore::open().unwrap();
    let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let files = store.list_files_for_cwd(&cwd).unwrap();
    println!("start_rss={}KB files={}", rss_kb(), files.len());
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    std::thread::spawn(move || {
        let mut total = 0usize;
        for f in &files {
            if let Some(entry) = f.inspect() { total += entry.message_count; }
            let _ = f.quick_preview();
        }
        drop(files);
        println!("thread_rss={}KB total_msgs={}", rss_kb(), total);
        tx.send(total).unwrap();
    });
    let _ = rx.recv().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    println!("main_after_rx_rss={}KB", rss_kb());
}
