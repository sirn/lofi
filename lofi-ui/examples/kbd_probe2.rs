
use crossterm::event::{Event, EventStream, EnableMouseCapture, EnableBracketedPaste};
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use crossterm::execute;
use futures::StreamExt;
use std::io;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _ = enable_raw_mode();
    let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste);
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mut evs = EventStream::new();
            eprintln!("probe3 ready");
            while let Some(ev) = evs.next().await {
                match ev {
                    Ok(Event::Key(k)) => eprintln!("KEY code={:?} kind={:?} mods={:?}", k.code, k.kind, k.modifiers),
                    Ok(e) => eprintln!("EVT {:?}", e),
                    Err(e) => eprintln!("ERR {:?}", e),
                }
            }
            eprintln!("stream ended");
        })
        .await;
}
