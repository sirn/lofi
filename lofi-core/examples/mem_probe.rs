use lofi_core::session::store::SessionCursor;
fn rss()->u64{std::fs::read_to_string("/proc/self/statm").unwrap_or_default().split_whitespace().nth(1).and_then(|v|v.parse::<u64>().ok()).unwrap_or(0)}
fn main(){ let p=std::env::args().nth(1).unwrap(); let c=SessionCursor::open(p.into()).unwrap(); let s=c.snapshot().unwrap(); std::hint::black_box(&s); eprintln!("MB={}", rss()*4/1024); }
