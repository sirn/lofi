use crossterm::event::{Event, EventStream};
use crossterm::terminal::enable_raw_mode;
use futures::StreamExt;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _ = enable_raw_mode();
    let mut evs = EventStream::new();
    eprintln!("probe ready");
    while let Some(ev) = evs.next().await {
        match ev {
            Ok(Event::Key(k)) => eprintln!("KEY code={:?} kind={:?} mods={:?}", k.code, k.kind, k.modifiers),
            Ok(e) => eprintln!("EVT {:?}", e),
            Err(e) => eprintln!("ERR {:?}", e),
        }
    }
    eprintln!("stream ended");
}
