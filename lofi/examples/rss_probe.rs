// Compare baseline RSS between allocators.
// Build once with mimalloc as-is, then once with the global_allocator
// lines commented out, and diff VmRSS.

use std::io::Read;

fn rss_kb() -> u64 {
    let mut s = String::new();
    std::fs::File::open("/proc/self/status")
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .parse()
                .unwrap_or(0);
        }
    }
    0
}

fn main() {
    let at_start = rss_kb();
    // Allocate ~50 MB in 1 MB chunks, touch each page, then drop.
    let mut bufs: Vec<Vec<u8>> = Vec::new();
    for _ in 0..50 {
        let mut v = vec![0u8; 1_000_000];
        for p in v.chunks_mut(4096) {
            p[0] = 1;
        }
        bufs.push(v);
    }
    let allocated = rss_kb();
    drop(bufs);
    // Force some churn to encourage free/return.
    for _ in 0..10 {
        let v: Vec<u8> = vec![0; 10_000_000];
        std::hint::black_box(&v);
        drop(v);
    }
    let after_drop = rss_kb();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let after_sleep = rss_kb();
    println!(
        "start={}KB  allocated={}KB  after_drop={}KB  after_500ms={}KB",
        at_start, allocated, after_drop, after_sleep
    );
}
