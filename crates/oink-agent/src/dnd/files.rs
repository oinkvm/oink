//! Drag file generations: pause closes old native readers permanently.
use oink_clipboard::sync::files::BulkLink;
use oink_foundation::content::DeliveryId;
use std::sync::{Arc, Mutex};
type Receipts =
    std::collections::HashMap<oink_foundation::content::DeliveryId, (Option<[u8; 32]>, tokio::time::Instant)>;
/// Shared by the platform glue and bulk server; pause retires a whole generation.
pub(crate) struct DndFiles {
    pub(crate) native: Arc<oink_clipboard::content::Registry>,
    pub(crate) input: Mutex<Option<Arc<crate::threads::input::InputThread>>>,
    receipts: tokio::sync::Mutex<Receipts>,
}

impl DndFiles {
    pub(crate) async fn authorize(
        &self,
        peer: oink_foundation::DeviceId,
        offer: oink_foundation::content::OfferId,
        demand: oink_clipboard::model::Demand,
        diagnostic: oink_foundation::drag_trace::Context,
    ) -> bool {
        use oink_clipboard::model::Demand;
        let (holder, commit) = match demand {
            Demand::Drop(holder) => (holder, true),
            Demand::Hover(holder) => (holder, false),
            Demand::Clipboard => return false,
        };
        let Some(input) = self.input.lock().unwrap().clone() else {
            return false;
        };
        let queue = oink_foundation::drag_trace::Trace::new(
            oink_foundation::drag_trace::stage::INPUT_QUEUE,
            oink_foundation::drag_trace::Context {
                offer: offer.clip.seq,
                aux: u32::from(commit),
                ..diagnostic
            },
        );
        let (reply, ready) = tokio::sync::oneshot::channel();
        tracing::info!(target: "oink_file_diagnostics", file_stage = 40_u8,
            file_offer = offer.clip.seq, file_generation = offer.generation,
            "drag file source authorization requested");
        input.send(crate::threads::input::InputCommand::DndContentDemand {
            peer,
            offer,
            holder,
            commit,
            diagnostic: queue.context(),
            reply,
        });
        let accepted = ready.await == Ok(true);
        queue.result(if accepted { 0 } else { -1 });
        tracing::info!(target: "oink_file_diagnostics", file_stage = 41_u8,
            file_offer = offer.clip.seq, file_generation = offer.generation, success = accepted,
            "drag file source authorization returned");
        if !accepted {
            return false;
        }
        if commit && let Some(stalled) = self.native.obligation(offer) {
            tokio::spawn(async move {
                if stalled.await {
                    let (reply, _) = tokio::sync::oneshot::channel();
                    input.send(crate::threads::input::InputCommand::DndDeliveryReceipt {
                        peer,
                        delivery: DeliveryId {
                            offer,
                            receiver: peer,
                            sequence: holder.epoch,
                        },
                        success: false,
                        reply,
                    });
                }
            });
        }
        true
    }
    /// A fulfilled promise returning to its own source uses the same bounded
    /// delivery/receipt pipeline, without inventing a network self-connection.
    pub(super) fn local(
        self: &Arc<Self>,
        peer: oink_foundation::DeviceId,
        offer: oink_foundation::content::OfferId,
    ) -> Arc<dyn BulkLink> {
        Arc::new(LocalLink {
            files: self.clone(),
            peer,
            offer,
        })
    }
    pub(super) fn new() -> Self {
        Self {
            native: Arc::default(),
            input: Mutex::new(None),
            receipts: Default::default(),
        }
    }
    pub(crate) async fn receipt(
        &self,
        peer: oink_foundation::DeviceId,
        delivery: oink_foundation::content::DeliveryId,
        receipt: Option<[u8; 32]>,
    ) -> bool {
        if delivery.receiver != peer {
            return false;
        }
        let mut receipts = self.receipts.lock().await;
        let now = tokio::time::Instant::now();
        receipts
            .retain(|_, (_, at)| now.duration_since(*at).as_millis() < oink_clipboard::model::NO_PROGRESS_MS as u128);
        if let Some((old, _)) = receipts.get(&delivery) {
            return *old == receipt;
        }
        let Some(input) = self.input.lock().unwrap().clone() else {
            return false;
        };
        let (reply, ready) = tokio::sync::oneshot::channel();
        input.send(crate::threads::input::InputCommand::DndDeliveryReceipt {
            peer,
            delivery,
            success: receipt.is_some(),
            reply,
        });
        if ready.await != Ok(true) {
            return false;
        }
        if receipts.len() >= 128
            && let Some(oldest) = receipts.iter().min_by_key(|(_, (_, at))| *at).map(|(id, _)| *id)
        {
            receipts.remove(&oldest);
        }
        receipts.insert(delivery, (receipt, now));
        true
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        if paused {
            self.native.cancel_all();
        }
    }
}

struct LocalLink {
    files: Arc<DndFiles>,
    peer: oink_foundation::DeviceId,
    offer: oink_foundation::content::OfferId,
}
use oink_clipboard::model::ContentMsg;
use oink_clipboard::sync::files::{BoxFuture, BulkChannel};
impl BulkLink for LocalLink {
    fn bind(self: Arc<Self>, _: oink_foundation::DeviceId, _: oink_foundation::content::ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn open(&self, peer: oink_foundation::DeviceId) -> BoxFuture<'_, std::io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async move {
            if peer != self.peer {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            let (a, ar) = tokio::sync::mpsc::channel(2);
            let (b, br) = tokio::sync::mpsc::channel(2);
            let mut server = LocalChannel {
                out: Some(b),
                input: ar,
            };
            let files = self.files.clone();
            let offer = self.offer;
            tokio::spawn(async move {
                let Ok(Some(request)) = server.recv().await else {
                    return;
                };
                let terminal = match request {
                    ContentMsg::TargetStored { delivery, receipt } => Some((delivery, Some(receipt))),
                    ContentMsg::Failed { delivery, .. } => Some((delivery, None)),
                    _ => None,
                };
                if let Some((delivery, receipt)) = terminal {
                    if delivery.offer == offer && files.receipt(peer, delivery, receipt).await {
                        let _ = server.send(&ContentMsg::TerminalAck { delivery, receipt }).await;
                    }
                } else if let Some((requested, demand)) = request.request()
                    && requested == offer
                    && files
                        .authorize(peer, offer, demand, oink_foundation::drag_trace::Context::default())
                        .await
                {
                    let _ = files.native.serve(peer, request, &mut server).await;
                }
                server.finish();
            });
            Ok(Box::new(LocalChannel {
                out: Some(a),
                input: br,
            }) as Box<dyn BulkChannel>)
        })
    }
}
struct LocalChannel {
    out: Option<tokio::sync::mpsc::Sender<ContentMsg>>,
    input: tokio::sync::mpsc::Receiver<ContentMsg>,
}
impl BulkChannel for LocalChannel {
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            self.out
                .as_ref()
                .ok_or(std::io::ErrorKind::BrokenPipe)?
                .send(message.clone())
                .await
                .map_err(|_| std::io::ErrorKind::BrokenPipe.into())
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, std::io::Result<Option<ContentMsg>>> {
        Box::pin(async { Ok(self.input.recv().await) })
    }
    fn finish(&mut self) {
        self.out = None;
    }
}
