use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use crate::messages::*;
use actix::prelude::*;
use actix_interop::{critical_section, FutureInterop, with_ctx};
use crate::player::MpcPlayer;
use crate::signer::Signer;
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::state_machine::sign::{OfflineStage, Error as OfflineStageError, CompletedOfflineStage, OfflineProtocolMessage, SignManual, PartialSignature};
use round_based::{Msg, StateMachine};
use crate::transport::join_computation;
use anyhow::{Context as AnyhowContext, Error, Result};
use curv::arithmetic::Converter;
use curv::BigInt;
use curv::elliptic::curves::Secp256k1;
use serde_json::ser::State;
use futures::{future, Sink};
use futures_util::SinkExt;
use surf::Url;
use futures_util::TryStreamExt;
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::party_i::SignatureRecid;
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::state_machine::keygen::{Keygen, Error as KeygenError, ProtocolMessage as KeygenProtocolMessage, LocalKey};
use crate::group::{MpcGroup, PublicKeyGroup};
use thiserror::Error;
use crate::coordinator::GroupError::WrongPublicKey;
use serde::{Serialize, Deserialize};

#[derive(Debug, Error)]
enum GroupError {
    #[error("Public key doesn't belong to the group.")]
    WrongPublicKey,
    #[error("Some of the public keys don't belong to the group.")]
    WrongPublicKeys,
}

#[derive(Debug, Error)]
enum DataError {
    #[error("Couldn't find the local share for {0}.")]
    LocalShareNotFound(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredLocalShare {
    public_keys: Vec<String>,
    own_public_key: String,
    share: LocalKey<Secp256k1>,
}

pub struct Coordinator {
    db : sled::Db,
    groups: HashMap<String, PublicKeyGroup>,
    keygen_runners: HashMap<String, Addr<MpcPlayer<KeygenRequest, Keygen, <Keygen as StateMachine>::Err, <Keygen as StateMachine>::Output>>>,
    offline_state_runners: HashMap<String, Addr<MpcPlayer<EnrichedSignRequest, OfflineStage, <OfflineStage as StateMachine>::Err, <OfflineStage as StateMachine>::Output>>>,
    signers: HashMap<String, Addr<Signer<EnrichedSignRequest>>>,
    sink: Option<Pin<Box<dyn Sink<Envelope, Error=anyhow::Error>>>>,
}

impl Coordinator {
    pub fn new<Si, St>(db:sled::Db, stream: St, sink: Si) -> Addr<Self>
        where
            St: Stream<Item=Result<SignedEnvelope<String>>> + 'static,
            Si: Sink<Envelope, Error=anyhow::Error> + 'static,
    {
        let stream = stream.and_then(|msg| async move {
            Ok(IncomingEnvelope {
                room: msg.room,
                message: msg.message,
                sender_public_key: msg.sender_public_key,
            })
        });
        let sink: Box<dyn Sink<Envelope, Error=anyhow::Error>> = Box::new(sink);

        Self::create(|ctx| {
            ctx.add_stream(stream);
            Self {
                db,
                groups: HashMap::new(),
                keygen_runners: HashMap::new(),
                offline_state_runners: HashMap::new(),
                signers: HashMap::new(),
                sink: Some(sink.into()),
            }
        })
    }

    fn valid_sender(&mut self, msg: IncomingEnvelope) -> Result<()> {
        let room = msg.room;
        let group = self.groups.get(&room).context("Can't found group.")?;
        group.get_index(&msg.sender_public_key).map_or(Err(WrongPublicKey), |_|Ok(())).context("Validate sender.")?;
        Ok(())
    }

    fn handle_incoming_keygen(&mut self, msg: IncomingEnvelope, ctx: &mut Context<Self>) -> Result<()> {
        let room = msg.room;
        let addr = self.keygen_runners.get(&room).context("Can't found mpc player.")?;
        let msg = serde_json::from_str::<Msg<KeygenProtocolMessage>>(&msg.message).context("deserialize message")?;
        addr.do_send(IncomingMessage {
            room: room.clone(),
            message: msg,
        });
        Ok(())
    }

    fn handle_incoming_offline(&mut self, msg: IncomingEnvelope, ctx: &mut Context<Self>) -> Result<()> {
        let room = msg.room;
        let addr = self.offline_state_runners.get(&room).context("Not found mpc player.")?;
        let msg = serde_json::from_str::<Msg<OfflineProtocolMessage>>(&msg.message).context("deserialize message")?;
        addr.do_send(IncomingMessage {
            room: room.clone(),
            message: msg,
        });

        Ok(())
    }

    fn handle_incoming_sign(&mut self, msg: IncomingEnvelope, ctx: &mut Context<Self>) -> Result<()> {
        let room = msg.room;
        let addr = self.signers.get(&room).context("Not found signer.")?;
        let msg = serde_json::from_str::<Msg<PartialSignature>>(&msg.message).context("deserialize message")?;
        addr.do_send(IncomingMessage {
            room: room.clone(),
            message: msg,
        });
        Ok(())
    }

    fn handle_incoming(&mut self, msg: IncomingEnvelope, ctx: &mut Context<Self>) {
        match self.valid_sender(msg.clone()) {
            Ok(()) => {
                let h1= self.handle_incoming_offline(msg.clone(), ctx);
                let h2 = self.handle_incoming_sign(msg.clone(), ctx);
                let h3 = self.handle_incoming_keygen(msg.clone(), ctx);
                if h1.or(h2).or(h3).is_err() {
                    ctx.run_later(Duration::from_secs(1), move|a, _ctx| {
                        _ctx.notify(RetryEnvelope {
                            room: msg.room.clone(),
                            message: msg.message.clone(),
                            sender_public_key: msg.sender_public_key.clone()
                        });
                    });
                }
            }
            Err(_)=>{
                // Do nothing
            }
        }

    }

    fn send_one(envelope: OutgoingEnvelope) -> impl Future<Output=()> {
        async move {
            critical_section::<Self, _>(async {
                let mut sink = with_ctx(|actor: &mut Self, _| actor.sink.take())
                    .expect("Sink to be present");

                // Send the request
                sink.send(Envelope {
                    room: envelope.room,
                    message: envelope.message,
                }).await;

                // Put the sink back, and if the send was successful,
                // record the in-flight request.
                with_ctx(|actor: &mut Self, _| {
                    actor.sink = Some(sink);
                });
            })
                .await;
        }
    }
    fn save_local_share(&mut self, local_share: StoredLocalShare) -> Result<()> {
        let out = serde_json::to_vec_pretty(&local_share).context("serialize local share")?;
        let sum_pk_bytes = local_share.share.public_key().to_bytes(true);
        let sum_pk = hex::encode(sum_pk_bytes.deref());
        let ov: Option<&[u8]> = None;
        let nv: Option<&[u8]> = Some(out.as_slice()); // TODO: Encrypt payload
        self.db.compare_and_swap(
            sum_pk.as_bytes(),      // key
            ov, // old value, None for not present
            nv, // new value, None for delete
        ).context("Save to db.")?;
        Ok(())
    }
    fn retrieve_local_share(&mut self, public_key: String) -> Result<StoredLocalShare> {
        let local_share = self.db.get(public_key.as_bytes())?
            .ok_or_else(||DataError::LocalShareNotFound(public_key.clone()))
            .context("Retrieving local share.")?;
        let local_share = serde_json::from_slice::<StoredLocalShare>(local_share.as_ref())
            .context("Decode local share.")?;
        Ok(local_share)
    }
}

impl Actor for Coordinator {
    type Context = Context<Self>;
}

impl Handler<KeygenRequest> for Coordinator {
    type Result = Result<()>;

    fn handle(&mut self, req: KeygenRequest, ctx: &mut Context<Self>) -> Self::Result {
        log::info!("Received request {:?}", req);
        let KeygenRequest{public_keys, t, own_public_key} = req.clone();
        let group = PublicKeyGroup::new(public_keys, t , own_public_key);
        let group_id = group.get_group_id();

        let state = Keygen::new(group.get_i(), group.get_t(), group.get_n()).context("Create state machine")?;
        let player = MpcPlayer::new(
            req.clone(),
            group_id.clone(),
            group.get_i(),
            state,
            ctx.address().recipient(),
            ctx.address().recipient(),
            ctx.address().recipient(),
        ).start();
        self.groups.insert(group_id.clone(), group);
        self.keygen_runners.insert(group_id.clone(), player);
        Ok(())
    }
}

impl Handler<SignRequest> for Coordinator {
    type Result = Result<()>;

    fn handle(&mut self, req: SignRequest, ctx: &mut Context<Self>) -> Self::Result {
        log::info!("Received request {:?}", req);
        let local_share = self.retrieve_local_share(req.public_key.clone()).context("Retrieve local share.")?;
        let group = PublicKeyGroup::new(
            local_share.public_keys,
            local_share.share.t,
            req.own_public_key.clone(),
        );
        // let public_keys = local_share.public_keys;
        // public_keys.iter().position(|&r| *r == k)
        let indices:Vec<Option<usize>> = req.participant_public_keys.clone().into_iter().map(
            |k| group.get_index(&k)
        ).collect();
        let (indices, errors): (Vec<Option<usize>>, Vec<_>) = indices.into_iter().partition(Option::is_some);

        let s_l:Vec<u16> = if errors.len() == 0 {
            Ok(indices.into_iter().map(|o| o.expect("Index") as u16).collect())
        } else {
            Err(GroupError::WrongPublicKeys)
        }.context("Find index of participants")?;
        let req = EnrichedSignRequest {
            inner: req,
            group_id: group.get_group_id(),
            i: group.get_i(),
            s_l: s_l.clone(),
        };
        let state = OfflineStage::new(group.get_i(), s_l, local_share.share).context("Create state machine")?;
        let player = MpcPlayer::new(
            req.clone(),
            group.get_group_id(),
            group.get_i(),
            state,
            ctx.address().recipient(),
            ctx.address().recipient(),
            ctx.address().recipient(),
        ).start();
        self.offline_state_runners.insert(group.get_group_id(), player);
        Ok(())
    }
}

impl Handler<ProtocolError<KeygenError>> for Coordinator {
    type Result = ();

    fn handle(&mut self, msg: ProtocolError<KeygenError>, _: &mut Context<Self>) {
        log::info!("Error {:?}", msg.error);
    }
}

impl Handler<ProtocolError<OfflineStageError>> for Coordinator {
    type Result = ();

    fn handle(&mut self, msg: ProtocolError<OfflineStageError>, _: &mut Context<Self>) {
        log::info!("Error {:?}", msg.error);
    }
}

impl Handler<OutgoingEnvelope> for Coordinator
{
    type Result = ();

    fn handle(&mut self, msg: OutgoingEnvelope, ctx: &mut Context<Self>) {
        ctx.spawn(Self::send_one(msg).interop_actor(self));
    }
}

impl Handler<ProtocolOutput<KeygenRequest, LocalKey<Secp256k1>>> for Coordinator
{
    type Result = ();

    fn handle(&mut self, msg: ProtocolOutput<KeygenRequest, LocalKey<Secp256k1>>, ctx: &mut Context<Self>) {
        let sum_pk_bytes = msg.output.public_key().to_bytes(true);
        let sum_pk = hex::encode(sum_pk_bytes.deref());
        log::info!("Public key is {:?}", sum_pk);
        let share = msg.output.clone();
        let saved = self.save_local_share(StoredLocalShare{
            public_keys: msg.input.public_keys,
            own_public_key: msg.input.own_public_key,
            share: msg.output
        });
        match saved {
            Ok(())=> {
                log::debug!("Saved local share: {:?}", share);
            }
            Err(e) => {log::error!("Failed to save local share: {}", e);}
        }
    }

}

impl Handler<ProtocolOutput<EnrichedSignRequest, CompletedOfflineStage>> for Coordinator
{
    type Result = ();

    fn handle(&mut self, msg: ProtocolOutput<EnrichedSignRequest, CompletedOfflineStage>, ctx: &mut Context<Self>) {
        log::info!("result {:?}", msg.output.public_key());
        let do_it = ||-> Result<()>{

            let message = BigInt::from_bytes(msg.input.inner.message.as_bytes());
            let completed_offline_stage = msg.output;
            let input = msg.input.clone();
            let signer = Signer::new(
                input.clone(),
                input.group_id,
                input.i,
                input.s_l.len()-1,
                message,
                completed_offline_stage,
                ctx.address().recipient(),
                ctx.address().recipient(),
            ).start();
            self.signers.insert(msg.input.group_id.to_owned(), signer);
            Ok(())
        };
        do_it();
    }

}

impl Handler<ProtocolOutput<EnrichedSignRequest, SignatureRecid>> for Coordinator
{
    type Result = ();

    fn handle(&mut self, msg: ProtocolOutput<EnrichedSignRequest, SignatureRecid>, ctx: &mut Context<Self>) {
        serde_json::to_string(&msg).context("serialize signature").map(
            |serialized|log::info!("Sign request done {:?}", serialized)
        );
    }

}

impl StreamHandler<Result<IncomingEnvelope>> for Coordinator
{
    fn handle(&mut self, msg: Result<IncomingEnvelope>, ctx: &mut Context<Self>) {
        match msg.context("Invalid IncomingEnvlope") {
            Ok(msg) => { self.handle_incoming(msg, ctx); }
            Err(_) => {}
        }
    }
}

impl Handler<RetryEnvelope> for Coordinator
{
    type Result = ();

    fn handle(&mut self, msg: RetryEnvelope, ctx: &mut Context<Self>) {
        self.handle_incoming(IncomingEnvelope {
            room: msg.room,
            message: msg.message,
            sender_public_key: msg.sender_public_key,
        }, ctx);
    }
}
