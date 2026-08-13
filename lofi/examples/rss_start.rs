use std::io::Read;
fn rss_kb() -> u64 {
    let mut s = String::new();
    std::fs::File::open("/proc/self/status").unwrap().read_to_string(&mut s).unwrap();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches(" kB").parse().unwrap_or(0);
        }
    }
    0
}
fn main() {
    println!("rss_at_main_start={}KB", rss_kb());
    let _v: Vec<u8> = vec![1; 1024];
    println!("rss_after_first_alloc={}KB", rss_kb());
}
