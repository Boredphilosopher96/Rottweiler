#![allow(dead_code)]
use std::{collections::BTreeMap, sync::{Arc, Mutex}, time::Duration};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
const MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS:usize=32;
trait SecretRedactor:Send+Sync {fn redact(&self,s:&str)->String;}
struct Identity; impl SecretRedactor for Identity {fn redact(&self,s:&str)->String{s.to_owned()}}
#[derive(Debug)] enum ToolError {Output(String)}
struct ToolOutputChunk {content:String,stream:usize}
enum PendingEvent {ToolOutput {turn:u64,id:String,stream:String,chunk:String}}
enum TurnSignal {ToolOutput {event:PendingEvent,_permit:OwnedSemaphorePermit}}
struct OrderedOutputState {
    current: usize,
    buffered: BTreeMap<usize, Vec<BoundedOutputChunk>>,
}

struct BoundedOutputChunk {
    id: String,
    chunk: ToolOutputChunk,
    permit: OwnedSemaphorePermit,
}

struct OrderedOutputCoordinator {
    turn: u64,
    signals: mpsc::UnboundedSender<TurnSignal>,
    state: Mutex<OrderedOutputState>,
    permits: Arc<Semaphore>,
    redactor: Arc<dyn SecretRedactor>,
}

impl OrderedOutputCoordinator {
    fn new(
        turn: u64,
        signals: mpsc::UnboundedSender<TurnSignal>,
        redactor: Arc<dyn SecretRedactor>,
    ) -> Self {
        Self {
            turn,
            signals,
            state: Mutex::new(OrderedOutputState {
                current: 0,
                buffered: BTreeMap::new(),
            }),
            permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS)),
            redactor,
        }
    }

    async fn emit(
        &self,
        index: usize,
        id: &str,
        mut chunk: ToolOutputChunk,
    ) -> Result<(), ToolError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| ToolError::Output("tool output channel is closed".to_owned()))?;
        chunk.content = self.redactor.redact(&chunk.content);
        let bounded = BoundedOutputChunk {
            id: id.to_owned(),
            chunk,
            permit,
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index == state.current {
            drop(state);
            self.send_chunk(bounded);
        } else {
            state.buffered.entry(index).or_default().push(bounded);
        }
        Ok(())
    }

    fn advance(&self, next: usize) {
        let buffered = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.current = next;
            state.buffered.remove(&next).unwrap_or_default()
        };
        for chunk in buffered {
            self.send_chunk(chunk);
        }
    }

    fn send_chunk(&self, bounded: BoundedOutputChunk) {
        let _ = self.signals.send(TurnSignal::ToolOutput {
            event: PendingEvent::ToolOutput {
                turn: self.turn,
                id: bounded.id,
                stream: format!("{:?}", bounded.chunk.stream).to_ascii_lowercase(),
                chunk: bounded.chunk.content,
            },
            _permit: bounded.permit,
        });
    }
}


#[tokio::main(flavor="current_thread")]
async fn main(){
 let (tx,mut rx)=mpsc::unbounded_channel();
 let c=OrderedOutputCoordinator::new(1,tx,Arc::new(Identity));
 for _ in 0..32 {c.emit(1,"later",ToolOutputChunk{content:"x".into(),stream:0}).await.unwrap();}
 assert_eq!(c.permits.available_permits(),0);
 assert!(rx.try_recv().is_err());
 let first=tokio::time::timeout(Duration::from_millis(100),c.emit(0,"first",ToolOutputChunk{content:"y".into(),stream:0})).await;
 assert!(first.is_err(),"current tool should be blocked by later buffers");
 println!("REPRODUCED: 32 later-tool chunks exhaust global permits; first tool output times out; actor queue empty, so no consumer can release permits.");
}
