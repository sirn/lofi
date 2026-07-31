#![allow(unsafe_code)]
fn rss_kb() -> (f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let mut rss = 0.0; let mut hwm = 0.0;
    for line in status.lines() {
        if let Some(r) = line.strip_prefix("VmRSS:") { rss = r.trim().trim_end_matches(" kB").parse().unwrap(); }
        if let Some(r) = line.strip_prefix("VmHWM:") { hwm = r.trim().trim_end_matches(" kB").parse().unwrap(); }
    }
    (rss / 1024.0, hwm / 1024.0)
}
fn heap_kb() -> f64 {
    // parse [heap] Rss from smaps
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
    let mut in_heap = false; let mut rss = 0.0;
    for line in smaps.lines() {
        if line.contains("[heap]") { in_heap = true; }
        else if in_heap && line.ends_with(" 0 ") { in_heap = false; }
        if in_heap && line.starts_with("Rss:") {
            rss = line[4..].trim().trim_end_matches(" kB").parse().unwrap();
        }
    }
    rss / 1024.0
}


fn main() {
    let (r0, _) = rss_kb();
    println!("start:            rss={:.1} heap={:.1} MB", r0, heap_kb());

    // Simulate materializing 320 turns of component trees: many nested
    // String/Vec/Rc allocations of varying small sizes.
    let mut keep: Vec<Vec<String>> = Vec::new();
    for t in 0..320 {
        let mut turn: Vec<String> = Vec::new();
        for b in 0..40 {
            turn.push(format!("turn {} block {}: {}", t, b, "some rendered text ".repeat(20)));
        }
        keep.push(turn);
    }
    let (r1, _) = rss_kb();
    println!("after alloc:      rss={:.1} heap={:.1} MB", r1, heap_kb());

    // Free the bulk (like dropping the transient component trees after measuring)
    drop(keep);
    let (r2, _) = rss_kb();
    println!("after drop:       rss={:.1} heap={:.1} MB  <- stays high (malloc retains)", r2, heap_kb());

    let (r3, _) = rss_kb();
    println!("after trim:       rss={:.1} heap={:.1} MB  <- reclaimed?", r3, heap_kb());
}
