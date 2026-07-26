use std::path::Path;
use lofi_core::session::store;
use lofi_providers::ir::{build_request, chat::ToolSchema};
use lofi_types::{Api, Message, Model, SessionEventKind, ThinkingLevel};

fn mem() -> (u64,u64,u64,u64) {
 let s=std::fs::read_to_string("/proc/self/status").unwrap_or_default();
 let f=|n:&str| s.lines().find(|l|l.starts_with(n)).and_then(|l|l.split_whitespace().nth(1)).and_then(|x|x.parse().ok()).unwrap_or(0);
 (f("VmRSS:"),f("RssAnon:"),f("RssFile:"),f("VmHWM:"))
}
fn main(){
 let path=std::env::args().nth(1).expect("session");
 println!("start={:?}",mem());
 let (_,idx,_)=store::load_index(Path::new(&path)).unwrap();
 println!("index entries={} mem={:?}",idx.len(),mem());
 let events=store::load_indexed_path(Path::new(&path),&idx,None).unwrap();
 println!("events={} mem={:?}",events.len(),mem());
 let messages:Vec<Message>=events.iter().filter_map(|e|match &e.kind{SessionEventKind::Message(m)=>Some(m.clone()),_=>None}).collect();
 println!("messages={} heap_bytes={} mem={:?}",messages.len(),messages.iter().map(message_bytes).sum::<usize>(),mem());
 drop(events); drop(idx);
 println!("after_drop_events_index={:?}",mem());
 let model=Model{id:"gpt-5.6-sol".into(),name:"gpt-5.6-sol".into(),provider:"plexus".into(),api:Api::OpenAiResponses,reasoning:true,thinking:ThinkingLevel::High,supports_image:false,context_window:Some(200000),max_tokens:None,base_url:None,input_price:None,output_price:None,cache_read_price:None,cache_write_price:None,per_request_price:None};
 let tools=[ToolSchema{name:"exec".into(),description:lofi_code::EXEC_TOOL_DESCRIPTION.into(),input_schema:lofi_code::exec_tool_input_schema()}];
 let body=build_request(Api::OpenAiResponses,&model,&messages,&tools);
 println!("body_built mem={:?}",mem());
 let json=serde_json::to_vec(&body).unwrap();
 println!("serialized_bytes={} mem={:?}",json.len(),mem());
 drop(json); println!("after_drop_json={:?}",mem());
 drop(body); println!("after_drop_body={:?}",mem());
 drop(messages); println!("after_drop_messages={:?}",mem());
}
fn message_bytes(m:&Message)->usize{m.blocks.iter().map(|b|match b{lofi_types::ContentBlock::Text{text}|lofi_types::ContentBlock::Thinking{text,..}=>text.len(),lofi_types::ContentBlock::ToolUse{id,name,input}=>id.len()+name.len()+input.to_string().len(),lofi_types::ContentBlock::ToolResult{tool_use_id,content,..}=>tool_use_id.len()+content.len()}).sum()}
