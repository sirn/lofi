
use crossterm::event::{Event, EventStream, EnableMouseCapture, EnableBracketedPaste};
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use crossterm::execute;
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _ = enable_raw_mode();
    let backend = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).unwrap();
    let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste);
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mut evs = EventStream::new();
            let mut tick = tokio::time::interval(Duration::from_millis(60));
            let mut input = String::new();
            let mut pending: Option<UnboundedReceiver<String>> = None;
            eprintln!("probe6 ready");
            loop {
                term.draw(|f| {
                    let para = ratatui::widgets::Paragraph::new(input.as_str());
                    f.render_widget(para, f.area());
                }).unwrap();
                tokio::select! {
                    maybe_ev = evs.next() => {
                        match maybe_ev {
                            Some(Ok(Event::Key(k))) => {
                                eprintln!("KEY code={:?} kind={:?} mods={:?}", k.code, k.kind, k.modifiers);
                                if let crossterm::event::KeyCode::Char(c) = k.code { input.push(c); }
                            }
                            Some(Ok(e)) => eprintln!("EVT {:?}", e),
                            Some(Err(e)) => eprintln!("ERR {:?}", e),
                            None => { eprintln!("stream ended"); break; }
                        }
                    }
                    notice = async {
                        match pending.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        eprintln!("NOTICE {:?}", notice);
                    }
                    _ = tick.tick() => {}
                }
            }
        })
        .await;
}
