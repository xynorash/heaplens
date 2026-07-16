use heaplens_protocol::{AllocEvent, EventKind, FrameDecoder, GraphMessage, encode_events};
use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::msg::GraphMsg;
use heaplens_daemon::resolver::Resolver;
use tokio::sync::mpsc;

fn make_alloc_event(ptr: u64, size: u64, ts: u64, stack: &[u64]) -> AllocEvent {
    let mut s = [0u64; 16];
    let len = stack.len().min(16);
    s[..len].copy_from_slice(&stack[..len]);
    AllocEvent::new(EventKind::Alloc, ptr, 0, size, 8, ts, s, len as u8)
}

/// Encode events through FrameDecoder round-trip → GraphMsg::Events
fn events_to_msg(events: &[AllocEvent]) -> GraphMsg {
    let bytes = encode_events(events);
    let mut decoder = FrameDecoder::new();
    decoder.push(&bytes);
    match decoder.next().expect("decoder must yield a frame") {
        heaplens_protocol::Frame::Events(evs) => GraphMsg::Events(evs),
        _ => panic!("expected Events frame"),
    }
}

#[tokio::test]
async fn loopback_alloc_and_tick_produces_diff() {
    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();

    let ev1 = make_alloc_event(0x1000, 64, 100, &[0xAAAA]);
    let ev2 = make_alloc_event(0x2000, 32, 200, &[0xBBBB, 0xAAAA]);
    tx.send(events_to_msg(&[ev1, ev2])).unwrap();
    tx.send(GraphMsg::Tick).unwrap();
    drop(tx);

    let mut graph = OwnershipGraph::new();
    let mut resolver = Resolver::new();
    resolver.insert(0xAAAA, "owner_site".to_owned(), false);
    resolver.insert(0xBBBB, "leaf_site".to_owned(), false);

    while let Some(msg) = rx.recv().await {
        match msg {
            GraphMsg::Events(events) => {
                for ev in &events {
                    match ev.kind {
                        0 => graph.on_alloc(ev, &resolver),
                        1 => graph.on_dealloc(ev.ptr, ev.ts_nanos),
                        2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size, &resolver),
                        _ => {}
                    }
                }
            }
            GraphMsg::Tick => {
                let diff = graph.drain_diff(&resolver);
                match diff {
                    GraphMessage::Diff { add, update, remove, .. } => {
                        assert_eq!(add.len(), 2, "both allocs should be in add");
                        assert!(update.is_empty(), "no updates expected");
                        assert!(remove.is_empty(), "no removes expected");

                        let owner = add.iter().find(|n| n.ptr == 0x1000).unwrap();
                        let child = add.iter().find(|n| n.ptr == 0x2000).unwrap();

                        // φ inference: owner's edges_out contains child's id
                        assert!(owner.edges.contains(&child.id),
                            "owner should have child in edges");
                    }
                    GraphMessage::Snapshot { .. } => panic!("expected Diff, got Snapshot"),
                }
            }
            GraphMsg::Symbols(_) => {}
        }
    }
}

#[tokio::test]
async fn loopback_dealloc_produces_remove() {
    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();

    let ev = make_alloc_event(0x1000, 64, 100, &[0xAAAA]);
    tx.send(events_to_msg(&[ev])).unwrap();
    tx.send(GraphMsg::Tick).unwrap();

    // Now dealloc (EventKind::Dealloc)
    let dealloc_ev = AllocEvent::new(EventKind::Dealloc, 0x1000, 0, 0, 0, 200, [0u64; 16], 0);
    tx.send(events_to_msg(&[dealloc_ev])).unwrap();
    tx.send(GraphMsg::Tick).unwrap();
    drop(tx);

    let mut graph = OwnershipGraph::new();
    let resolver = Resolver::new();
    let mut tick_count = 0u32;

    while let Some(msg) = rx.recv().await {
        match msg {
            GraphMsg::Events(events) => {
                for ev in &events {
                    match ev.kind {
                        0 => graph.on_alloc(ev, &resolver),
                        1 => graph.on_dealloc(ev.ptr, ev.ts_nanos),
                        2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size, &resolver),
                        _ => {}
                    }
                }
            }
            GraphMsg::Tick => {
                tick_count += 1;
                let diff = graph.drain_diff(&resolver);
                if let GraphMessage::Diff { add, remove, .. } = diff {
                    if tick_count == 1 {
                        assert_eq!(add.len(), 1, "first tick: alloc in add");
                    } else if tick_count == 2 {
                        assert_eq!(remove.len(), 1, "second tick: dealloc in remove");
                    }
                }
            }
            GraphMsg::Symbols(_) => {}
        }
    }

    assert_eq!(tick_count, 2, "should have processed two ticks");
}
