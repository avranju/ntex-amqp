use std::collections::{BTreeMap, HashMap, VecDeque};

use bytes::Bytes;
use futures::unsync::oneshot;
use futures::{future, Future};
use slab::Slab;
use string::{self, TryFrom};
use uuid::Uuid;

use amqp::framing::AmqpFrame;
use amqp::protocol::{
    Accepted, Attach, DeliveryNumber, DeliveryState, Disposition, Flow, Frame, Handle, Outcome,
    ReceiverSettleMode, Role, SenderSettleMode, Target, TerminusDurability, TerminusExpiryPolicy,
    Transfer,
};

use crate::cell::Cell;
use crate::connection::{ChannelId, ConnectionController};
use crate::errors::AmqpTransportError;
use crate::link::{SenderLink, SenderLinkInner};
use crate::message::Message;
use crate::DeliveryPromise;

#[derive(Clone)]
pub struct Session {
    inner: Cell<SessionInner>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.inner.get_mut().drop_session()
    }
}

impl Session {
    pub(crate) fn new(inner: Cell<SessionInner>) -> Session {
        Session { inner }
    }

    pub fn close() -> impl Future<Item = (), Error = AmqpTransportError> {
        future::ok(())
    }

    pub fn open_sender_link(
        &mut self,
        address: String,
        name: String,
    ) -> impl Future<Item = SenderLink, Error = AmqpTransportError> {
        let inner = self.inner.clone();
        self.inner.get_mut().open_sender_link(address, name, inner)
    }
}

enum LinkState {
    Opening(oneshot::Sender<SenderLink>, Cell<SessionInner>),
    Established(Cell<SenderLinkInner>),
    Closing(Cell<SenderLinkInner>),
    None,
}

impl LinkState {
    fn is_opening(&self) -> bool {
        match self {
            LinkState::Opening(_, _) => true,
            _ => false,
        }
    }
}

pub(crate) struct SessionInner {
    id: ChannelId,
    connection: ConnectionController,
    remote_channel_id: u16,
    next_outgoing_id: DeliveryNumber,
    outgoing_window: u32,
    next_incoming_id: DeliveryNumber,
    incoming_window: u32,
    unsettled_deliveries: BTreeMap<DeliveryNumber, DeliveryPromise>,
    links: Slab<LinkState>,
    pending_links: HashMap<string::String<Bytes>, usize>,
    pending_transfers: VecDeque<PendingTransfer>,
}

struct PendingTransfer {
    link_handle: Handle,
    message: Message,
    promise: DeliveryPromise,
}

impl SessionInner {
    pub fn new(
        id: ChannelId,
        connection: ConnectionController,
        remote_channel_id: u16,
        outgoing_window: u32,
        next_incoming_id: DeliveryNumber,
        incoming_window: u32,
    ) -> SessionInner {
        SessionInner {
            id,
            connection,
            remote_channel_id,
            next_outgoing_id: 1,
            outgoing_window: outgoing_window,
            next_incoming_id,
            incoming_window,
            unsettled_deliveries: BTreeMap::new(),
            links: Slab::new(),
            pending_links: HashMap::new(),
            pending_transfers: VecDeque::new(),
        }
    }

    fn drop_session(&mut self) {
        self.connection.drop_session_copy(self.id);
    }

    pub fn handle_frame(&mut self, frame: AmqpFrame, self_rc: Cell<SessionInner>) {
        match *frame.performative() {
            Frame::Attach(ref attach) => self.complete_link_creation(attach),
            Frame::Disposition(ref disp) => self.settle_deliveries(disp),
            Frame::Flow(ref flow) => self.apply_flow(flow),
            Frame::Detach(_) => println!("unexpected frame: {:#?}", frame),
            // todo: handle Detach, End
            _ => {
                // todo: handle unexpected frames
            }
        }
    }

    fn complete_link_creation(&mut self, attach: &Attach) {
        let name = attach.name();
        if let Some(index) = self.pending_links.remove(name) {
            match self.links.get_mut(index) {
                Some(item) => {
                    if item.is_opening() {
                        trace!("sender link opened: {:?}", name);
                        let local_sender = std::mem::replace(item, LinkState::None);

                        if let LinkState::Opening(tx, self_rc) = local_sender {
                            let link = Cell::new(SenderLinkInner::new(self_rc, attach.handle()));
                            *item = LinkState::Established(link.clone());
                            let _ = tx.send(SenderLink::new(link));
                        }
                    }
                }
                _ => {
                    // TODO: error in proto, have to close connection
                }
            }
        } else {
            // todo: rogue attach right now - do nothing. in future will indicate incoming attach
        }
    }

    fn settle_deliveries(&mut self, disposition: &Disposition) {
        assert!(disposition.settled()); // we can only work with settled for now
        let from = disposition.first;
        let to = disposition.last.unwrap_or(from);
        let actionable = self
            .unsettled_deliveries
            .range(from..to + 1)
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        let outcome: Outcome;
        match disposition.state().map(|s| s.clone()) {
            Some(DeliveryState::Received(_v)) => {
                return;
            } // todo: apply more thinking
            Some(DeliveryState::Accepted(v)) => outcome = Outcome::Accepted(v.clone()),
            Some(DeliveryState::Rejected(v)) => outcome = Outcome::Rejected(v.clone()),
            Some(DeliveryState::Released(v)) => outcome = Outcome::Released(v.clone()),
            Some(DeliveryState::Modified(v)) => outcome = Outcome::Modified(v.clone()),
            None => outcome = Outcome::Accepted(Accepted {}),
        }
        // let state = disposition.state().map(|s| s.clone()).unwrap_or(DeliveryState::Accepted(Accepted {})).clone(); // todo: honor Source.default_outcome()
        for k in actionable {
            self.unsettled_deliveries
                .remove(&k)
                .unwrap()
                .send(Ok(outcome.clone()));
        }
    }

    fn apply_flow(&mut self, flow: &Flow) {
        self.outgoing_window =
            flow.next_incoming_id().unwrap_or(0) + flow.incoming_window() - self.next_outgoing_id;
        trace!(
            "session received credit. window: {}, pending: {}",
            self.outgoing_window,
            self.pending_transfers.len()
        );
        while let Some(t) = self.pending_transfers.pop_front() {
            self.send_transfer_conn(t.link_handle, t.message, t.promise);
            if self.outgoing_window == 0 {
                break;
            }
        }
        if let Some(link) = flow.handle().and_then(|h| self.links.get_mut(h as usize)) {
            match link {
                LinkState::Established(ref mut link) => {
                    link.get_mut().apply_flow(flow);
                }
                _ => (),
            }
        } else if flow.echo() {
            self.send_flow();
        }
    }

    fn send_flow(&mut self) {
        let flow = Flow {
            next_incoming_id: Some(self.next_incoming_id), // todo: derive from begin/flow
            incoming_window: self.incoming_window,
            next_outgoing_id: self.next_outgoing_id,
            outgoing_window: self.outgoing_window,
            handle: None,
            delivery_count: None,
            link_credit: None,
            available: None,
            drain: false,
            echo: false,
            properties: None,
        };
        self.post_frame_conn(Frame::Flow(flow), Bytes::new());
    }

    fn post_frame(&mut self, frame: Frame, payload: Bytes) {
        self.post_frame_conn(frame, payload);
    }

    fn post_frame_conn(&mut self, frame: Frame, payload: Bytes) {
        self.connection
            .post_frame(AmqpFrame::new(self.remote_channel_id, frame, payload));
    }

    pub fn open_sender_link(
        &mut self,
        address: String,
        name: String,
        self_rc: Cell<SessionInner>,
    ) -> impl Future<Item = SenderLink, Error = AmqpTransportError> {
        let (tx, rx) = oneshot::channel();

        let entry = self.links.vacant_entry();
        let token = entry.key();
        entry.insert(LinkState::Opening(tx, self_rc));

        let name = string::String::try_from(Bytes::from(name)).unwrap();
        let address = string::String::try_from(Bytes::from(address)).unwrap();

        let target = Target {
            address: Some(address),
            durable: TerminusDurability::None,
            expiry_policy: TerminusExpiryPolicy::SessionEnd,
            timeout: 0,
            dynamic: false,
            dynamic_node_properties: None,
            capabilities: None,
        };
        let attach = Attach {
            name: name.clone(),
            handle: token as Handle,
            role: Role::Sender,
            snd_settle_mode: SenderSettleMode::Mixed,
            rcv_settle_mode: ReceiverSettleMode::First,
            source: None,
            target: Some(target),
            unsettled: None,
            incomplete_unsettled: false,
            initial_delivery_count: None,
            max_message_size: None,
            offered_capabilities: None,
            desired_capabilities: None,
            properties: None,
        };
        self.pending_links.insert(name, token);
        self.post_frame(Frame::Attach(attach), Bytes::new());
        rx.map_err(|_e| AmqpTransportError::Disconnected)
    }

    pub fn send_transfer(
        &mut self,
        link_handle: Handle,
        message: Message,
        promise: DeliveryPromise,
    ) {
        // todo: DRY
        if self.outgoing_window == 0 {
            // todo: queue up instead
            self.pending_transfers.push_back(PendingTransfer {
                link_handle,
                message,
                promise,
            });
            return;
        }
        let (frame, body) = self.prepare_transfer(link_handle, message, promise);
        self.post_frame(frame, body);
    }

    pub fn send_transfer_conn(
        &mut self,
        link_handle: Handle,
        message: Message,
        promise: DeliveryPromise,
    ) {
        // todo: DRY
        if self.outgoing_window == 0 {
            // todo: queue up instead
            self.pending_transfers.push_back(PendingTransfer {
                link_handle,
                message,
                promise,
            });
            return;
        }
        let (frame, body) = self.prepare_transfer(link_handle, message, promise);
        self.post_frame_conn(frame, body);
    }

    pub fn prepare_transfer(
        &mut self,
        link_handle: Handle,
        message: Message,
        promise: DeliveryPromise,
    ) -> (Frame, Bytes) {
        self.outgoing_window -= 1;
        let delivery_id = self.next_outgoing_id;
        self.next_outgoing_id += 1;
        let delivery_tag = Bytes::from(&Uuid::new_v4().as_bytes()[..]);
        let transfer = Transfer {
            handle: link_handle,
            delivery_id: Some(delivery_id),
            delivery_tag: Some(delivery_tag.clone()),
            message_format: message.message_format,
            settled: Some(false),
            more: false,
            rcv_settle_mode: None,
            state: None,
            resume: false,
            aborted: false,
            batchable: false,
        };
        self.unsettled_deliveries.insert(delivery_id, promise);
        (Frame::Transfer(transfer), message.serialize())
    }
}
