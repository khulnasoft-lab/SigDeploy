mod store;

use crate::{
    auth,
    db::{self, ChannelId, MessageId, ProjectId, User, UserId},
    AppState, Result,
};
use anyhow::anyhow;
use async_tungstenite::tungstenite::{
    protocol::CloseFrame as TungsteniteCloseFrame, Message as TungsteniteMessage,
};
use axum::{
    body::Body,
    extract::{
        ws::{CloseFrame as AxumCloseFrame, Message as AxumMessage},
        ConnectInfo, WebSocketUpgrade,
    },
    headers::{Header, HeaderName},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
    Extension, Router, TypedHeader,
};
use collections::{HashMap, HashSet};
use futures::{
    channel::{mpsc, oneshot},
    future::{self, BoxFuture},
    stream::FuturesUnordered,
    FutureExt, SinkExt, StreamExt, TryStreamExt,
};
use lazy_static::lazy_static;
use prometheus::{register_int_gauge, IntGauge};
use rpc::{
    proto::{self, AnyTypedEnvelope, EntityMessage, EnvelopedMessage, RequestMessage},
    Connection, ConnectionId, Peer, Receipt, TypedEnvelope,
};
use serde::{Serialize, Serializer};
use std::{
    any::TypeId,
    future::Future,
    marker::PhantomData,
    net::SocketAddr,
    ops::{Deref, DerefMut},
    os::unix::prelude::OsStrExt,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering::SeqCst},
        Arc,
    },
    time::Duration,
};
pub use store::{Store, Worktree};
use time::OffsetDateTime;
use tokio::{
    sync::{Mutex, MutexGuard},
    time::Sleep,
};
use tower::ServiceBuilder;
use tracing::{info_span, instrument, Instrument};

lazy_static! {
    static ref METRIC_CONNECTIONS: IntGauge =
        register_int_gauge!("connections", "number of connections").unwrap();
    static ref METRIC_REGISTERED_PROJECTS: IntGauge =
        register_int_gauge!("registered_projects", "number of registered projects").unwrap();
    static ref METRIC_ACTIVE_PROJECTS: IntGauge =
        register_int_gauge!("active_projects", "number of active projects").unwrap();
    static ref METRIC_SHARED_PROJECTS: IntGauge = register_int_gauge!(
        "shared_projects",
        "number of open projects with one or more guests"
    )
    .unwrap();
}

type MessageHandler =
    Box<dyn Send + Sync + Fn(Arc<Server>, Box<dyn AnyTypedEnvelope>) -> BoxFuture<'static, ()>>;

struct Response<R> {
    server: Arc<Server>,
    receipt: Receipt<R>,
    responded: Arc<AtomicBool>,
}

impl<R: RequestMessage> Response<R> {
    fn send(self, payload: R::Response) -> Result<()> {
        self.responded.store(true, SeqCst);
        self.server.peer.respond(self.receipt, payload)?;
        Ok(())
    }
}

pub struct Server {
    peer: Arc<Peer>,
    pub(crate) store: Mutex<Store>,
    app_state: Arc<AppState>,
    handlers: HashMap<TypeId, MessageHandler>,
    notifications: Option<mpsc::UnboundedSender<()>>,
}

pub trait Executor: Send + Clone {
    type Sleep: Send + Future;
    fn spawn_detached<F: 'static + Send + Future<Output = ()>>(&self, future: F);
    fn sleep(&self, duration: Duration) -> Self::Sleep;
}

#[derive(Clone)]
pub struct RealExecutor;

const MESSAGE_COUNT_PER_PAGE: usize = 100;
const MAX_MESSAGE_LEN: usize = 1024;

pub(crate) struct StoreGuard<'a> {
    guard: MutexGuard<'a, Store>,
    _not_send: PhantomData<Rc<()>>,
}

#[derive(Serialize)]
pub struct ServerSnapshot<'a> {
    peer: &'a Peer,
    #[serde(serialize_with = "serialize_deref")]
    store: StoreGuard<'a>,
}

pub fn serialize_deref<S, T, U>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Deref<Target = U>,
    U: Serialize,
{
    Serialize::serialize(value.deref(), serializer)
}

impl Server {
    pub fn new(
        app_state: Arc<AppState>,
        notifications: Option<mpsc::UnboundedSender<()>>,
    ) -> Arc<Self> {
        let mut server = Self {
            peer: Peer::new(),
            app_state,
            store: Default::default(),
            handlers: Default::default(),
            notifications,
        };

        server
            .add_request_handler(Server::ping)
            .add_request_handler(Server::create_room)
            .add_request_handler(Server::join_room)
            .add_message_handler(Server::leave_room)
            .add_request_handler(Server::call)
            .add_request_handler(Server::cancel_call)
            .add_message_handler(Server::decline_call)
            .add_request_handler(Server::update_participant_location)
            .add_request_handler(Server::share_project)
            .add_message_handler(Server::unshare_project)
            .add_request_handler(Server::join_project)
            .add_message_handler(Server::leave_project)
            .add_message_handler(Server::update_project)
            .add_message_handler(Server::register_project_activity)
            .add_request_handler(Server::update_worktree)
            .add_message_handler(Server::update_worktree_extensions)
            .add_message_handler(Server::start_language_server)
            .add_message_handler(Server::update_language_server)
            .add_message_handler(Server::update_diagnostic_summary)
            .add_request_handler(Server::forward_project_request::<proto::GetHover>)
            .add_request_handler(Server::forward_project_request::<proto::GetDefinition>)
            .add_request_handler(Server::forward_project_request::<proto::GetTypeDefinition>)
            .add_request_handler(Server::forward_project_request::<proto::GetReferences>)
            .add_request_handler(Server::forward_project_request::<proto::SearchProject>)
            .add_request_handler(Server::forward_project_request::<proto::GetDocumentHighlights>)
            .add_request_handler(Server::forward_project_request::<proto::GetProjectSymbols>)
            .add_request_handler(Server::forward_project_request::<proto::OpenBufferForSymbol>)
            .add_request_handler(Server::forward_project_request::<proto::OpenBufferById>)
            .add_request_handler(Server::forward_project_request::<proto::OpenBufferByPath>)
            .add_request_handler(Server::forward_project_request::<proto::GetCompletions>)
            .add_request_handler(
                Server::forward_project_request::<proto::ApplyCompletionAdditionalEdits>,
            )
            .add_request_handler(Server::forward_project_request::<proto::GetCodeActions>)
            .add_request_handler(Server::forward_project_request::<proto::ApplyCodeAction>)
            .add_request_handler(Server::forward_project_request::<proto::PrepareRename>)
            .add_request_handler(Server::forward_project_request::<proto::PerformRename>)
            .add_request_handler(Server::forward_project_request::<proto::ReloadBuffers>)
            .add_request_handler(Server::forward_project_request::<proto::FormatBuffers>)
            .add_request_handler(Server::forward_project_request::<proto::CreateProjectEntry>)
            .add_request_handler(Server::forward_project_request::<proto::RenameProjectEntry>)
            .add_request_handler(Server::forward_project_request::<proto::CopyProjectEntry>)
            .add_request_handler(Server::forward_project_request::<proto::DeleteProjectEntry>)
            .add_message_handler(Server::create_buffer_for_peer)
            .add_request_handler(Server::update_buffer)
            .add_message_handler(Server::update_buffer_file)
            .add_message_handler(Server::buffer_reloaded)
            .add_message_handler(Server::buffer_saved)
            .add_request_handler(Server::save_buffer)
            .add_request_handler(Server::get_channels)
            .add_request_handler(Server::get_users)
            .add_request_handler(Server::fuzzy_search_users)
            .add_request_handler(Server::request_contact)
            .add_request_handler(Server::remove_contact)
            .add_request_handler(Server::respond_to_contact_request)
            .add_request_handler(Server::join_channel)
            .add_message_handler(Server::leave_channel)
            .add_request_handler(Server::send_channel_message)
            .add_request_handler(Server::follow)
            .add_message_handler(Server::unfollow)
            .add_message_handler(Server::update_followers)
            .add_request_handler(Server::get_channel_messages)
            .add_message_handler(Server::update_diff_base)
            .add_request_handler(Server::get_private_user_info);

        Arc::new(server)
    }

    fn add_message_handler<F, Fut, M>(&mut self, handler: F) -> &mut Self
    where
        F: 'static + Send + Sync + Fn(Arc<Self>, TypedEnvelope<M>) -> Fut,
        Fut: 'static + Send + Future<Output = Result<()>>,
        M: EnvelopedMessage,
    {
        let prev_handler = self.handlers.insert(
            TypeId::of::<M>(),
            Box::new(move |server, envelope| {
                let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                let span = info_span!(
                    "handle message",
                    payload_type = envelope.payload_type_name()
                );
                span.in_scope(|| {
                    tracing::info!(
                        payload_type = envelope.payload_type_name(),
                        "message received"
                    );
                });
                let future = (handler)(server, *envelope);
                async move {
                    if let Err(error) = future.await {
                        tracing::error!(%error, "error handling message");
                    }
                }
                .instrument(span)
                .boxed()
            }),
        );
        if prev_handler.is_some() {
            panic!("registered a handler for the same message twice");
        }
        self
    }

    /// Handle a request while holding a lock to the store. This is useful when we're registering
    /// a connection but we want to respond on the connection before anybody else can send on it.
    fn add_request_handler<F, Fut, M>(&mut self, handler: F) -> &mut Self
    where
        F: 'static + Send + Sync + Fn(Arc<Self>, TypedEnvelope<M>, Response<M>) -> Fut,
        Fut: Send + Future<Output = Result<()>>,
        M: RequestMessage,
    {
        let handler = Arc::new(handler);
        self.add_message_handler(move |server, envelope| {
            let receipt = envelope.receipt();
            let handler = handler.clone();
            async move {
                let responded = Arc::new(AtomicBool::default());
                let response = Response {
                    server: server.clone(),
                    responded: responded.clone(),
                    receipt: envelope.receipt(),
                };
                match (handler)(server.clone(), envelope, response).await {
                    Ok(()) => {
                        if responded.load(std::sync::atomic::Ordering::SeqCst) {
                            Ok(())
                        } else {
                            Err(anyhow!("handler did not send a response"))?
                        }
                    }
                    Err(error) => {
                        server.peer.respond_with_error(
                            receipt,
                            proto::Error {
                                message: error.to_string(),
                            },
                        )?;
                        Err(error)
                    }
                }
            }
        })
    }

    /// Start a long lived task that records which users are active in which projects.
    pub fn start_recording_project_activity<E: 'static + Executor>(
        self: &Arc<Self>,
        interval: Duration,
        executor: E,
    ) {
        executor.spawn_detached({
            let this = Arc::downgrade(self);
            let executor = executor.clone();
            async move {
                let mut period_start = OffsetDateTime::now_utc();
                let mut active_projects = Vec::<(UserId, ProjectId)>::new();
                loop {
                    let sleep = executor.sleep(interval);
                    sleep.await;
                    let this = if let Some(this) = this.upgrade() {
                        this
                    } else {
                        break;
                    };

                    active_projects.clear();
                    active_projects.extend(this.store().await.projects().flat_map(
                        |(project_id, project)| {
                            project.guests.values().chain([&project.host]).filter_map(
                                |collaborator| {
                                    if !collaborator.admin
                                        && collaborator
                                            .last_activity
                                            .map_or(false, |activity| activity > period_start)
                                    {
                                        Some((collaborator.user_id, *project_id))
                                    } else {
                                        None
                                    }
                                },
                            )
                        },
                    ));

                    let period_end = OffsetDateTime::now_utc();
                    this.app_state
                        .db
                        .record_user_activity(period_start..period_end, &active_projects)
                        .await
                        .trace_err();
                    period_start = period_end;
                }
            }
        });
    }

    pub fn handle_connection<E: Executor>(
        self: &Arc<Self>,
        connection: Connection,
        address: String,
        user: User,
        mut send_connection_id: Option<oneshot::Sender<ConnectionId>>,
        executor: E,
    ) -> impl Future<Output = Result<()>> {
        let mut this = self.clone();
        let user_id = user.id;
        let login = user.github_login;
        let span = info_span!("handle connection", %user_id, %login, %address);
        async move {
            let (connection_id, handle_io, mut incoming_rx) = this
                .peer
                .add_connection(connection, {
                    let executor = executor.clone();
                    move |duration| {
                        let timer = executor.sleep(duration);
                        async move {
                            timer.await;
                        }
                    }
                });

            tracing::info!(%user_id, %login, %connection_id, %address, "connection opened");
            this.peer.send(connection_id, proto::Hello { peer_id: connection_id.0 })?;
            tracing::info!(%user_id, %login, %connection_id, %address, "sent hello message");

            if let Some(send_connection_id) = send_connection_id.take() {
                let _ = send_connection_id.send(connection_id);
            }

            if !user.connected_once {
                this.peer.send(connection_id, proto::ShowContacts {})?;
                this.app_state.db.set_user_connected_once(user_id, true).await?;
            }

            let (contacts, invite_code) = future::try_join(
                this.app_state.db.get_contacts(user_id),
                this.app_state.db.get_invite_code_for_user(user_id)
            ).await?;

            {
                let mut store = this.store().await;
                let incoming_call = store.add_connection(connection_id, user_id, user.admin);
                if let Some(incoming_call) = incoming_call {
                    this.peer.send(connection_id, incoming_call)?;
                }

                this.peer.send(connection_id, store.build_initial_contacts_update(contacts))?;

                if let Some((code, count)) = invite_code {
                    this.peer.send(connection_id, proto::UpdateInviteInfo {
                        url: format!("{}{}", this.app_state.config.invite_link_prefix, code),
                        count,
                    })?;
                }
            }
            this.update_user_contacts(user_id).await?;

            let handle_io = handle_io.fuse();
            futures::pin_mut!(handle_io);

            // Handlers for foreground messages are pushed into the following `FuturesUnordered`.
            // This prevents deadlocks when e.g., client A performs a request to client B and
            // client B performs a request to client A. If both clients stop processing further
            // messages until their respective request completes, they won't have a chance to
            // respond to the other client's request and cause a deadlock.
            //
            // This arrangement ensures we will attempt to process earlier messages first, but fall
            // back to processing messages arrived later in the spirit of making progress.
            let mut foreground_message_handlers = FuturesUnordered::new();
            loop {
                let next_message = incoming_rx.next().fuse();
                futures::pin_mut!(next_message);
                futures::select_biased! {
                    result = handle_io => {
                        if let Err(error) = result {
                            tracing::error!(?error, %user_id, %login, %connection_id, %address, "error handling I/O");
                        }
                        break;
                    }
                    _ = foreground_message_handlers.next() => {}
                    message = next_message => {
                        if let Some(message) = message {
                            let type_name = message.payload_type_name();
                            let span = tracing::info_span!("receive message", %user_id, %login, %connection_id, %address, type_name);
                            let span_enter = span.enter();
                            if let Some(handler) = this.handlers.get(&message.payload_type_id()) {
                                let notifications = this.notifications.clone();
                                let is_background = message.is_background();
                                let handle_message = (handler)(this.clone(), message);

                                drop(span_enter);
                                let handle_message = async move {
                                    handle_message.await;
                                    if let Some(mut notifications) = notifications {
                                        let _ = notifications.send(()).await;
                                    }
                                }.instrument(span);

                                if is_background {
                                    executor.spawn_detached(handle_message);
                                } else {
                                    foreground_message_handlers.push(handle_message);
                                }
                            } else {
                                tracing::error!(%user_id, %login, %connection_id, %address, "no message handler");
                            }
                        } else {
                            tracing::info!(%user_id, %login, %connection_id, %address, "connection closed");
                            break;
                        }
                    }
                }
            }

            drop(foreground_message_handlers);
            tracing::info!(%user_id, %login, %connection_id, %address, "signing out");
            if let Err(error) = this.sign_out(connection_id).await {
                tracing::error!(%user_id, %login, %connection_id, %address, ?error, "error signing out");
            }

            Ok(())
        }.instrument(span)
    }

    #[instrument(skip(self), err)]
    async fn sign_out(self: &mut Arc<Self>, connection_id: ConnectionId) -> Result<()> {
        self.peer.disconnect(connection_id);

        let mut projects_to_unshare = Vec::new();
        let mut contacts_to_update = HashSet::default();
        let mut room_left = None;
        {
            let mut store = self.store().await;

            #[cfg(test)]
            let removed_connection = store.remove_connection(connection_id).unwrap();
            #[cfg(not(test))]
            let removed_connection = store.remove_connection(connection_id)?;

            for project in removed_connection.hosted_projects {
                projects_to_unshare.push(project.id);
                broadcast(connection_id, project.guests.keys().copied(), |conn_id| {
                    self.peer.send(
                        conn_id,
                        proto::UnshareProject {
                            project_id: project.id.to_proto(),
                        },
                    )
                });
            }

            for project in removed_connection.guest_projects {
                broadcast(connection_id, project.connection_ids, |conn_id| {
                    self.peer.send(
                        conn_id,
                        proto::RemoveProjectCollaborator {
                            project_id: project.id.to_proto(),
                            peer_id: connection_id.0,
                        },
                    )
                });
            }

            if let Some(room) = removed_connection.room {
                self.room_updated(&room);
                room_left = Some(self.room_left(&room, connection_id));
            }

            contacts_to_update.insert(removed_connection.user_id);
            for connection_id in removed_connection.canceled_call_connection_ids {
                self.peer
                    .send(connection_id, proto::CallCanceled {})
                    .trace_err();
                contacts_to_update.extend(store.user_id_for_connection(connection_id).ok());
            }
        };

        if let Some(room_left) = room_left {
            room_left.await.trace_err();
        }

        for user_id in contacts_to_update {
            self.update_user_contacts(user_id).await.trace_err();
        }

        for project_id in projects_to_unshare {
            self.app_state
                .db
                .unregister_project(project_id)
                .await
                .trace_err();
        }

        Ok(())
    }

    pub async fn invite_code_redeemed(
        self: &Arc<Self>,
        inviter_id: UserId,
        invitee_id: UserId,
    ) -> Result<()> {
        if let Some(user) = self.app_state.db.get_user_by_id(inviter_id).await? {
            if let Some(code) = &user.invite_code {
                let store = self.store().await;
                let invitee_contact = store.contact_for_user(invitee_id, true);
                for connection_id in store.connection_ids_for_user(inviter_id) {
                    self.peer.send(
                        connection_id,
                        proto::UpdateContacts {
                            contacts: vec![invitee_contact.clone()],
                            ..Default::default()
                        },
                    )?;
                    self.peer.send(
                        connection_id,
                        proto::UpdateInviteInfo {
                            url: format!("{}{}", self.app_state.config.invite_link_prefix, &code),
                            count: user.invite_count as u32,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    pub async fn invite_count_updated(self: &Arc<Self>, user_id: UserId) -> Result<()> {
        if let Some(user) = self.app_state.db.get_user_by_id(user_id).await? {
            if let Some(invite_code) = &user.invite_code {
                let store = self.store().await;
                for connection_id in store.connection_ids_for_user(user_id) {
                    self.peer.send(
                        connection_id,
                        proto::UpdateInviteInfo {
                            url: format!(
                                "{}{}",
                                self.app_state.config.invite_link_prefix, invite_code
                            ),
                            count: user.invite_count as u32,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    async fn ping(
        self: Arc<Server>,
        _: TypedEnvelope<proto::Ping>,
        response: Response<proto::Ping>,
    ) -> Result<()> {
        response.send(proto::Ack {})?;
        Ok(())
    }

    async fn create_room(
        self: Arc<Server>,
        request: TypedEnvelope<proto::CreateRoom>,
        response: Response<proto::CreateRoom>,
    ) -> Result<()> {
        let user_id;
        let room;
        {
            let mut store = self.store().await;
            user_id = store.user_id_for_connection(request.sender_id)?;
            room = store.create_room(request.sender_id)?.clone();
        }

        let live_kit_connection_info =
            if let Some(live_kit) = self.app_state.live_kit_client.as_ref() {
                if let Some(_) = live_kit
                    .create_room(room.live_kit_room.clone())
                    .await
                    .trace_err()
                {
                    if let Some(token) = live_kit
                        .room_token(&room.live_kit_room, &request.sender_id.to_string())
                        .trace_err()
                    {
                        Some(proto::LiveKitConnectionInfo {
                            server_url: live_kit.url().into(),
                            token,
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

        response.send(proto::CreateRoomResponse {
            room: Some(room),
            live_kit_connection_info,
        })?;
        self.update_user_contacts(user_id).await?;
        Ok(())
    }

    async fn join_room(
        self: Arc<Server>,
        request: TypedEnvelope<proto::JoinRoom>,
        response: Response<proto::JoinRoom>,
    ) -> Result<()> {
        let user_id;
        {
            let mut store = self.store().await;
            user_id = store.user_id_for_connection(request.sender_id)?;
            let (room, recipient_connection_ids) =
                store.join_room(request.payload.id, request.sender_id)?;
            for recipient_id in recipient_connection_ids {
                self.peer
                    .send(recipient_id, proto::CallCanceled {})
                    .trace_err();
            }

            let live_kit_connection_info =
                if let Some(live_kit) = self.app_state.live_kit_client.as_ref() {
                    if let Some(token) = live_kit
                        .room_token(&room.live_kit_room, &request.sender_id.to_string())
                        .trace_err()
                    {
                        Some(proto::LiveKitConnectionInfo {
                            server_url: live_kit.url().into(),
                            token,
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };

            response.send(proto::JoinRoomResponse {
                room: Some(room.clone()),
                live_kit_connection_info,
            })?;
            self.room_updated(room);
        }
        self.update_user_contacts(user_id).await?;
        Ok(())
    }

    async fn leave_room(self: Arc<Server>, message: TypedEnvelope<proto::LeaveRoom>) -> Result<()> {
        let mut contacts_to_update = HashSet::default();
        let room_left;
        {
            let mut store = self.store().await;
            let user_id = store.user_id_for_connection(message.sender_id)?;
            let left_room = store.leave_room(message.payload.id, message.sender_id)?;
            contacts_to_update.insert(user_id);

            for project in left_room.unshared_projects {
                for connection_id in project.connection_ids() {
                    self.peer.send(
                        connection_id,
                        proto::UnshareProject {
                            project_id: project.id.to_proto(),
                        },
                    )?;
                }
            }

            for project in left_room.left_projects {
                if project.remove_collaborator {
                    for connection_id in project.connection_ids {
                        self.peer.send(
                            connection_id,
                            proto::RemoveProjectCollaborator {
                                project_id: project.id.to_proto(),
                                peer_id: message.sender_id.0,
                            },
                        )?;
                    }

                    self.peer.send(
                        message.sender_id,
                        proto::UnshareProject {
                            project_id: project.id.to_proto(),
                        },
                    )?;
                }
            }

            self.room_updated(&left_room.room);
            room_left = self.room_left(&left_room.room, message.sender_id);

            for connection_id in left_room.canceled_call_connection_ids {
                self.peer
                    .send(connection_id, proto::CallCanceled {})
                    .trace_err();
                contacts_to_update.extend(store.user_id_for_connection(connection_id).ok());
            }
        }

        room_left.await.trace_err();
        for user_id in contacts_to_update {
            self.update_user_contacts(user_id).await?;
        }

        Ok(())
    }

    async fn call(
        self: Arc<Server>,
        request: TypedEnvelope<proto::Call>,
        response: Response<proto::Call>,
    ) -> Result<()> {
        let caller_user_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let recipient_user_id = UserId::from_proto(request.payload.recipient_user_id);
        let initial_project_id = request
            .payload
            .initial_project_id
            .map(ProjectId::from_proto);
        if !self
            .app_state
            .db
            .has_contact(caller_user_id, recipient_user_id)
            .await?
        {
            return Err(anyhow!("cannot call a user who isn't a contact"))?;
        }

        let room_id = request.payload.room_id;
        let mut calls = {
            let mut store = self.store().await;
            let (room, recipient_connection_ids, incoming_call) = store.call(
                room_id,
                recipient_user_id,
                initial_project_id,
                request.sender_id,
            )?;
            self.room_updated(room);
            recipient_connection_ids
                .into_iter()
                .map(|recipient_connection_id| {
                    self.peer
                        .request(recipient_connection_id, incoming_call.clone())
                })
                .collect::<FuturesUnordered<_>>()
        };
        self.update_user_contacts(recipient_user_id).await?;

        while let Some(call_response) = calls.next().await {
            match call_response.as_ref() {
                Ok(_) => {
                    response.send(proto::Ack {})?;
                    return Ok(());
                }
                Err(_) => {
                    call_response.trace_err();
                }
            }
        }

        {
            let mut store = self.store().await;
            let room = store.call_failed(room_id, recipient_user_id)?;
            self.room_updated(&room);
        }
        self.update_user_contacts(recipient_user_id).await?;

        Err(anyhow!("failed to ring call recipient"))?
    }

    async fn cancel_call(
        self: Arc<Server>,
        request: TypedEnvelope<proto::CancelCall>,
        response: Response<proto::CancelCall>,
    ) -> Result<()> {
        let recipient_user_id = UserId::from_proto(request.payload.recipient_user_id);
        {
            let mut store = self.store().await;
            let (room, recipient_connection_ids) = store.cancel_call(
                request.payload.room_id,
                recipient_user_id,
                request.sender_id,
            )?;
            for recipient_id in recipient_connection_ids {
                self.peer
                    .send(recipient_id, proto::CallCanceled {})
                    .trace_err();
            }
            self.room_updated(room);
            response.send(proto::Ack {})?;
        }
        self.update_user_contacts(recipient_user_id).await?;
        Ok(())
    }

    async fn decline_call(
        self: Arc<Server>,
        message: TypedEnvelope<proto::DeclineCall>,
    ) -> Result<()> {
        let recipient_user_id;
        {
            let mut store = self.store().await;
            recipient_user_id = store.user_id_for_connection(message.sender_id)?;
            let (room, recipient_connection_ids) =
                store.decline_call(message.payload.room_id, message.sender_id)?;
            for recipient_id in recipient_connection_ids {
                self.peer
                    .send(recipient_id, proto::CallCanceled {})
                    .trace_err();
            }
            self.room_updated(room);
        }
        self.update_user_contacts(recipient_user_id).await?;
        Ok(())
    }

    async fn update_participant_location(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateParticipantLocation>,
        response: Response<proto::UpdateParticipantLocation>,
    ) -> Result<()> {
        let room_id = request.payload.room_id;
        let location = request
            .payload
            .location
            .ok_or_else(|| anyhow!("invalid location"))?;
        let mut store = self.store().await;
        let room = store.update_participant_location(room_id, location, request.sender_id)?;
        self.room_updated(room);
        response.send(proto::Ack {})?;
        Ok(())
    }

    fn room_updated(&self, room: &proto::Room) {
        for participant in &room.participants {
            self.peer
                .send(
                    ConnectionId(participant.peer_id),
                    proto::RoomUpdated {
                        room: Some(room.clone()),
                    },
                )
                .trace_err();
        }
    }

    fn room_left(
        &self,
        room: &proto::Room,
        connection_id: ConnectionId,
    ) -> impl Future<Output = Result<()>> {
        let client = self.app_state.live_kit_client.clone();
        let room_name = room.live_kit_room.clone();
        let participant_count = room.participants.len();
        async move {
            if let Some(client) = client {
                client
                    .remove_participant(room_name.clone(), connection_id.to_string())
                    .await?;

                if participant_count == 0 {
                    client.delete_room(room_name).await?;
                }
            }

            Ok(())
        }
    }

    async fn share_project(
        self: Arc<Server>,
        request: TypedEnvelope<proto::ShareProject>,
        response: Response<proto::ShareProject>,
    ) -> Result<()> {
        let user_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let project_id = self.app_state.db.register_project(user_id).await?;
        let mut store = self.store().await;
        let room = store.share_project(
            request.payload.room_id,
            project_id,
            request.payload.worktrees,
            request.sender_id,
        )?;
        response.send(proto::ShareProjectResponse {
            project_id: project_id.to_proto(),
        })?;
        self.room_updated(room);

        Ok(())
    }

    async fn unshare_project(
        self: Arc<Server>,
        message: TypedEnvelope<proto::UnshareProject>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(message.payload.project_id);
        let mut store = self.store().await;
        let (room, project) = store.unshare_project(project_id, message.sender_id)?;
        broadcast(
            message.sender_id,
            project.guest_connection_ids(),
            |conn_id| self.peer.send(conn_id, message.payload.clone()),
        );
        self.room_updated(room);

        Ok(())
    }

    async fn update_user_contacts(self: &Arc<Server>, user_id: UserId) -> Result<()> {
        let contacts = self.app_state.db.get_contacts(user_id).await?;
        let store = self.store().await;
        let updated_contact = store.contact_for_user(user_id, false);
        for contact in contacts {
            if let db::Contact::Accepted {
                user_id: contact_user_id,
                ..
            } = contact
            {
                for contact_conn_id in store.connection_ids_for_user(contact_user_id) {
                    self.peer
                        .send(
                            contact_conn_id,
                            proto::UpdateContacts {
                                contacts: vec![updated_contact.clone()],
                                remove_contacts: Default::default(),
                                incoming_requests: Default::default(),
                                remove_incoming_requests: Default::default(),
                                outgoing_requests: Default::default(),
                                remove_outgoing_requests: Default::default(),
                            },
                        )
                        .trace_err();
                }
            }
        }
        Ok(())
    }

    async fn join_project(
        self: Arc<Server>,
        request: TypedEnvelope<proto::JoinProject>,
        response: Response<proto::JoinProject>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);

        let host_user_id;
        let guest_user_id;
        let host_connection_id;
        {
            let state = self.store().await;
            let project = state.project(project_id)?;
            host_user_id = project.host.user_id;
            host_connection_id = project.host_connection_id;
            guest_user_id = state.user_id_for_connection(request.sender_id)?;
        };

        tracing::info!(%project_id, %host_user_id, %host_connection_id, "join project");

        let mut store = self.store().await;
        let (project, replica_id) = store.join_project(request.sender_id, project_id)?;
        let peer_count = project.guests.len();
        let mut collaborators = Vec::with_capacity(peer_count);
        collaborators.push(proto::Collaborator {
            peer_id: project.host_connection_id.0,
            replica_id: 0,
            user_id: project.host.user_id.to_proto(),
        });
        let worktrees = project
            .worktrees
            .iter()
            .map(|(id, worktree)| proto::WorktreeMetadata {
                id: *id,
                root_name: worktree.root_name.clone(),
                visible: worktree.visible,
                abs_path: worktree.abs_path.as_os_str().as_bytes().to_vec(),
            })
            .collect::<Vec<_>>();

        // Add all guests other than the requesting user's own connections as collaborators
        for (guest_conn_id, guest) in &project.guests {
            if request.sender_id != *guest_conn_id {
                collaborators.push(proto::Collaborator {
                    peer_id: guest_conn_id.0,
                    replica_id: guest.replica_id as u32,
                    user_id: guest.user_id.to_proto(),
                });
            }
        }

        for conn_id in project.connection_ids() {
            if conn_id != request.sender_id {
                self.peer.send(
                    conn_id,
                    proto::AddProjectCollaborator {
                        project_id: project_id.to_proto(),
                        collaborator: Some(proto::Collaborator {
                            peer_id: request.sender_id.0,
                            replica_id: replica_id as u32,
                            user_id: guest_user_id.to_proto(),
                        }),
                    },
                )?;
            }
        }

        // First, we send the metadata associated with each worktree.
        response.send(proto::JoinProjectResponse {
            worktrees: worktrees.clone(),
            replica_id: replica_id as u32,
            collaborators: collaborators.clone(),
            language_servers: project.language_servers.clone(),
        })?;

        for (worktree_id, worktree) in &project.worktrees {
            #[cfg(any(test, feature = "test-support"))]
            const MAX_CHUNK_SIZE: usize = 2;
            #[cfg(not(any(test, feature = "test-support")))]
            const MAX_CHUNK_SIZE: usize = 256;

            // Stream this worktree's entries.
            let message = proto::UpdateWorktree {
                project_id: project_id.to_proto(),
                worktree_id: *worktree_id,
                abs_path: worktree.abs_path.as_os_str().as_bytes().to_vec(),
                root_name: worktree.root_name.clone(),
                updated_entries: worktree.entries.values().cloned().collect(),
                removed_entries: Default::default(),
                scan_id: worktree.scan_id,
                is_last_update: worktree.is_complete,
            };
            for update in proto::split_worktree_update(message, MAX_CHUNK_SIZE) {
                self.peer.send(request.sender_id, update.clone())?;
            }

            // Stream this worktree's diagnostics.
            for summary in worktree.diagnostic_summaries.values() {
                self.peer.send(
                    request.sender_id,
                    proto::UpdateDiagnosticSummary {
                        project_id: project_id.to_proto(),
                        worktree_id: *worktree_id,
                        summary: Some(summary.clone()),
                    },
                )?;
            }
        }

        for language_server in &project.language_servers {
            self.peer.send(
                request.sender_id,
                proto::UpdateLanguageServer {
                    project_id: project_id.to_proto(),
                    language_server_id: language_server.id,
                    variant: Some(
                        proto::update_language_server::Variant::DiskBasedDiagnosticsUpdated(
                            proto::LspDiskBasedDiagnosticsUpdated {},
                        ),
                    ),
                },
            )?;
        }

        Ok(())
    }

    async fn leave_project(
        self: Arc<Server>,
        request: TypedEnvelope<proto::LeaveProject>,
    ) -> Result<()> {
        let sender_id = request.sender_id;
        let project_id = ProjectId::from_proto(request.payload.project_id);
        let project;
        {
            let mut store = self.store().await;
            project = store.leave_project(project_id, sender_id)?;
            tracing::info!(
                %project_id,
                host_user_id = %project.host_user_id,
                host_connection_id = %project.host_connection_id,
                "leave project"
            );

            if project.remove_collaborator {
                broadcast(sender_id, project.connection_ids, |conn_id| {
                    self.peer.send(
                        conn_id,
                        proto::RemoveProjectCollaborator {
                            project_id: project_id.to_proto(),
                            peer_id: sender_id.0,
                        },
                    )
                });
            }
        }

        Ok(())
    }

    async fn update_project(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateProject>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);
        {
            let mut state = self.store().await;
            let guest_connection_ids = state
                .read_project(project_id, request.sender_id)?
                .guest_connection_ids();
            let room =
                state.update_project(project_id, &request.payload.worktrees, request.sender_id)?;
            broadcast(request.sender_id, guest_connection_ids, |connection_id| {
                self.peer
                    .forward_send(request.sender_id, connection_id, request.payload.clone())
            });
            self.room_updated(room);
        };

        Ok(())
    }

    async fn register_project_activity(
        self: Arc<Server>,
        request: TypedEnvelope<proto::RegisterProjectActivity>,
    ) -> Result<()> {
        self.store().await.register_project_activity(
            ProjectId::from_proto(request.payload.project_id),
            request.sender_id,
        )?;
        Ok(())
    }

    async fn update_worktree(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateWorktree>,
        response: Response<proto::UpdateWorktree>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);
        let worktree_id = request.payload.worktree_id;
        let connection_ids = self.store().await.update_worktree(
            request.sender_id,
            project_id,
            worktree_id,
            &request.payload.root_name,
            &request.payload.removed_entries,
            &request.payload.updated_entries,
            request.payload.scan_id,
            request.payload.is_last_update,
        )?;

        broadcast(request.sender_id, connection_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        response.send(proto::Ack {})?;
        Ok(())
    }

    async fn update_worktree_extensions(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateWorktreeExtensions>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);
        let worktree_id = request.payload.worktree_id;
        let extensions = request
            .payload
            .extensions
            .into_iter()
            .zip(request.payload.counts)
            .collect();
        self.app_state
            .db
            .update_worktree_extensions(project_id, worktree_id, extensions)
            .await?;
        Ok(())
    }

    async fn update_diagnostic_summary(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateDiagnosticSummary>,
    ) -> Result<()> {
        let summary = request
            .payload
            .summary
            .clone()
            .ok_or_else(|| anyhow!("invalid summary"))?;
        let receiver_ids = self.store().await.update_diagnostic_summary(
            ProjectId::from_proto(request.payload.project_id),
            request.payload.worktree_id,
            request.sender_id,
            summary,
        )?;

        broadcast(request.sender_id, receiver_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        Ok(())
    }

    async fn start_language_server(
        self: Arc<Server>,
        request: TypedEnvelope<proto::StartLanguageServer>,
    ) -> Result<()> {
        let receiver_ids = self.store().await.start_language_server(
            ProjectId::from_proto(request.payload.project_id),
            request.sender_id,
            request
                .payload
                .server
                .clone()
                .ok_or_else(|| anyhow!("invalid language server"))?,
        )?;
        broadcast(request.sender_id, receiver_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        Ok(())
    }

    async fn update_language_server(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateLanguageServer>,
    ) -> Result<()> {
        let receiver_ids = self.store().await.project_connection_ids(
            ProjectId::from_proto(request.payload.project_id),
            request.sender_id,
        )?;
        broadcast(request.sender_id, receiver_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        Ok(())
    }

    async fn forward_project_request<T>(
        self: Arc<Server>,
        request: TypedEnvelope<T>,
        response: Response<T>,
    ) -> Result<()>
    where
        T: EntityMessage + RequestMessage,
    {
        let project_id = ProjectId::from_proto(request.payload.remote_entity_id());
        let host_connection_id = self
            .store()
            .await
            .read_project(project_id, request.sender_id)?
            .host_connection_id;
        let payload = self
            .peer
            .forward_request(request.sender_id, host_connection_id, request.payload)
            .await?;

        // Ensure project still exists by the time we get the response from the host.
        self.store()
            .await
            .read_project(project_id, request.sender_id)?;

        response.send(payload)?;
        Ok(())
    }

    async fn save_buffer(
        self: Arc<Server>,
        request: TypedEnvelope<proto::SaveBuffer>,
        response: Response<proto::SaveBuffer>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);
        let host = self
            .store()
            .await
            .read_project(project_id, request.sender_id)?
            .host_connection_id;
        let response_payload = self
            .peer
            .forward_request(request.sender_id, host, request.payload.clone())
            .await?;

        let mut guests = self
            .store()
            .await
            .read_project(project_id, request.sender_id)?
            .connection_ids();
        guests.retain(|guest_connection_id| *guest_connection_id != request.sender_id);
        broadcast(host, guests, |conn_id| {
            self.peer
                .forward_send(host, conn_id, response_payload.clone())
        });
        response.send(response_payload)?;
        Ok(())
    }

    async fn create_buffer_for_peer(
        self: Arc<Server>,
        request: TypedEnvelope<proto::CreateBufferForPeer>,
    ) -> Result<()> {
        self.peer.forward_send(
            request.sender_id,
            ConnectionId(request.payload.peer_id),
            request.payload,
        )?;
        Ok(())
    }

    async fn update_buffer(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateBuffer>,
        response: Response<proto::UpdateBuffer>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);
        let receiver_ids = {
            let mut store = self.store().await;
            store.register_project_activity(project_id, request.sender_id)?;
            store.project_connection_ids(project_id, request.sender_id)?
        };

        broadcast(request.sender_id, receiver_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        response.send(proto::Ack {})?;
        Ok(())
    }

    async fn update_buffer_file(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateBufferFile>,
    ) -> Result<()> {
        let receiver_ids = self.store().await.project_connection_ids(
            ProjectId::from_proto(request.payload.project_id),
            request.sender_id,
        )?;
        broadcast(request.sender_id, receiver_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        Ok(())
    }

    async fn buffer_reloaded(
        self: Arc<Server>,
        request: TypedEnvelope<proto::BufferReloaded>,
    ) -> Result<()> {
        let receiver_ids = self.store().await.project_connection_ids(
            ProjectId::from_proto(request.payload.project_id),
            request.sender_id,
        )?;
        broadcast(request.sender_id, receiver_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        Ok(())
    }

    async fn buffer_saved(
        self: Arc<Server>,
        request: TypedEnvelope<proto::BufferSaved>,
    ) -> Result<()> {
        let receiver_ids = self.store().await.project_connection_ids(
            ProjectId::from_proto(request.payload.project_id),
            request.sender_id,
        )?;
        broadcast(request.sender_id, receiver_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        Ok(())
    }

    async fn follow(
        self: Arc<Self>,
        request: TypedEnvelope<proto::Follow>,
        response: Response<proto::Follow>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);
        let leader_id = ConnectionId(request.payload.leader_id);
        let follower_id = request.sender_id;
        {
            let mut store = self.store().await;
            if !store
                .project_connection_ids(project_id, follower_id)?
                .contains(&leader_id)
            {
                Err(anyhow!("no such peer"))?;
            }

            store.register_project_activity(project_id, follower_id)?;
        }

        let mut response_payload = self
            .peer
            .forward_request(request.sender_id, leader_id, request.payload)
            .await?;
        response_payload
            .views
            .retain(|view| view.leader_id != Some(follower_id.0));
        response.send(response_payload)?;
        Ok(())
    }

    async fn unfollow(self: Arc<Self>, request: TypedEnvelope<proto::Unfollow>) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);
        let leader_id = ConnectionId(request.payload.leader_id);
        let mut store = self.store().await;
        if !store
            .project_connection_ids(project_id, request.sender_id)?
            .contains(&leader_id)
        {
            Err(anyhow!("no such peer"))?;
        }
        store.register_project_activity(project_id, request.sender_id)?;
        self.peer
            .forward_send(request.sender_id, leader_id, request.payload)?;
        Ok(())
    }

    async fn update_followers(
        self: Arc<Self>,
        request: TypedEnvelope<proto::UpdateFollowers>,
    ) -> Result<()> {
        let project_id = ProjectId::from_proto(request.payload.project_id);
        let mut store = self.store().await;
        store.register_project_activity(project_id, request.sender_id)?;
        let connection_ids = store.project_connection_ids(project_id, request.sender_id)?;
        let leader_id = request
            .payload
            .variant
            .as_ref()
            .and_then(|variant| match variant {
                proto::update_followers::Variant::CreateView(payload) => payload.leader_id,
                proto::update_followers::Variant::UpdateView(payload) => payload.leader_id,
                proto::update_followers::Variant::UpdateActiveView(payload) => payload.leader_id,
            });
        for follower_id in &request.payload.follower_ids {
            let follower_id = ConnectionId(*follower_id);
            if connection_ids.contains(&follower_id) && Some(follower_id.0) != leader_id {
                self.peer
                    .forward_send(request.sender_id, follower_id, request.payload.clone())?;
            }
        }
        Ok(())
    }

    async fn get_channels(
        self: Arc<Server>,
        request: TypedEnvelope<proto::GetChannels>,
        response: Response<proto::GetChannels>,
    ) -> Result<()> {
        let user_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let channels = self.app_state.db.get_accessible_channels(user_id).await?;
        response.send(proto::GetChannelsResponse {
            channels: channels
                .into_iter()
                .map(|chan| proto::Channel {
                    id: chan.id.to_proto(),
                    name: chan.name,
                })
                .collect(),
        })?;
        Ok(())
    }

    async fn get_users(
        self: Arc<Server>,
        request: TypedEnvelope<proto::GetUsers>,
        response: Response<proto::GetUsers>,
    ) -> Result<()> {
        let user_ids = request
            .payload
            .user_ids
            .into_iter()
            .map(UserId::from_proto)
            .collect();
        let users = self
            .app_state
            .db
            .get_users_by_ids(user_ids)
            .await?
            .into_iter()
            .map(|user| proto::User {
                id: user.id.to_proto(),
                avatar_url: format!("https://github.com/{}.png?size=128", user.github_login),
                github_login: user.github_login,
            })
            .collect();
        response.send(proto::UsersResponse { users })?;
        Ok(())
    }

    async fn fuzzy_search_users(
        self: Arc<Server>,
        request: TypedEnvelope<proto::FuzzySearchUsers>,
        response: Response<proto::FuzzySearchUsers>,
    ) -> Result<()> {
        let user_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let query = request.payload.query;
        let db = &self.app_state.db;
        let users = match query.len() {
            0 => vec![],
            1 | 2 => db
                .get_user_by_github_account(&query, None)
                .await?
                .into_iter()
                .collect(),
            _ => db.fuzzy_search_users(&query, 10).await?,
        };
        let users = users
            .into_iter()
            .filter(|user| user.id != user_id)
            .map(|user| proto::User {
                id: user.id.to_proto(),
                avatar_url: format!("https://github.com/{}.png?size=128", user.github_login),
                github_login: user.github_login,
            })
            .collect();
        response.send(proto::UsersResponse { users })?;
        Ok(())
    }

    async fn request_contact(
        self: Arc<Server>,
        request: TypedEnvelope<proto::RequestContact>,
        response: Response<proto::RequestContact>,
    ) -> Result<()> {
        let requester_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let responder_id = UserId::from_proto(request.payload.responder_id);
        if requester_id == responder_id {
            return Err(anyhow!("cannot add yourself as a contact"))?;
        }

        self.app_state
            .db
            .send_contact_request(requester_id, responder_id)
            .await?;

        // Update outgoing contact requests of requester
        let mut update = proto::UpdateContacts::default();
        update.outgoing_requests.push(responder_id.to_proto());
        for connection_id in self.store().await.connection_ids_for_user(requester_id) {
            self.peer.send(connection_id, update.clone())?;
        }

        // Update incoming contact requests of responder
        let mut update = proto::UpdateContacts::default();
        update
            .incoming_requests
            .push(proto::IncomingContactRequest {
                requester_id: requester_id.to_proto(),
                should_notify: true,
            });
        for connection_id in self.store().await.connection_ids_for_user(responder_id) {
            self.peer.send(connection_id, update.clone())?;
        }

        response.send(proto::Ack {})?;
        Ok(())
    }

    async fn respond_to_contact_request(
        self: Arc<Server>,
        request: TypedEnvelope<proto::RespondToContactRequest>,
        response: Response<proto::RespondToContactRequest>,
    ) -> Result<()> {
        let responder_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let requester_id = UserId::from_proto(request.payload.requester_id);
        if request.payload.response == proto::ContactRequestResponse::Dismiss as i32 {
            self.app_state
                .db
                .dismiss_contact_notification(responder_id, requester_id)
                .await?;
        } else {
            let accept = request.payload.response == proto::ContactRequestResponse::Accept as i32;
            self.app_state
                .db
                .respond_to_contact_request(responder_id, requester_id, accept)
                .await?;

            let store = self.store().await;
            // Update responder with new contact
            let mut update = proto::UpdateContacts::default();
            if accept {
                update
                    .contacts
                    .push(store.contact_for_user(requester_id, false));
            }
            update
                .remove_incoming_requests
                .push(requester_id.to_proto());
            for connection_id in store.connection_ids_for_user(responder_id) {
                self.peer.send(connection_id, update.clone())?;
            }

            // Update requester with new contact
            let mut update = proto::UpdateContacts::default();
            if accept {
                update
                    .contacts
                    .push(store.contact_for_user(responder_id, true));
            }
            update
                .remove_outgoing_requests
                .push(responder_id.to_proto());
            for connection_id in store.connection_ids_for_user(requester_id) {
                self.peer.send(connection_id, update.clone())?;
            }
        }

        response.send(proto::Ack {})?;
        Ok(())
    }

    async fn remove_contact(
        self: Arc<Server>,
        request: TypedEnvelope<proto::RemoveContact>,
        response: Response<proto::RemoveContact>,
    ) -> Result<()> {
        let requester_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let responder_id = UserId::from_proto(request.payload.user_id);
        self.app_state
            .db
            .remove_contact(requester_id, responder_id)
            .await?;

        // Update outgoing contact requests of requester
        let mut update = proto::UpdateContacts::default();
        update
            .remove_outgoing_requests
            .push(responder_id.to_proto());
        for connection_id in self.store().await.connection_ids_for_user(requester_id) {
            self.peer.send(connection_id, update.clone())?;
        }

        // Update incoming contact requests of responder
        let mut update = proto::UpdateContacts::default();
        update
            .remove_incoming_requests
            .push(requester_id.to_proto());
        for connection_id in self.store().await.connection_ids_for_user(responder_id) {
            self.peer.send(connection_id, update.clone())?;
        }

        response.send(proto::Ack {})?;
        Ok(())
    }

    async fn join_channel(
        self: Arc<Self>,
        request: TypedEnvelope<proto::JoinChannel>,
        response: Response<proto::JoinChannel>,
    ) -> Result<()> {
        let user_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let channel_id = ChannelId::from_proto(request.payload.channel_id);
        if !self
            .app_state
            .db
            .can_user_access_channel(user_id, channel_id)
            .await?
        {
            Err(anyhow!("access denied"))?;
        }

        self.store()
            .await
            .join_channel(request.sender_id, channel_id);
        let messages = self
            .app_state
            .db
            .get_channel_messages(channel_id, MESSAGE_COUNT_PER_PAGE, None)
            .await?
            .into_iter()
            .map(|msg| proto::ChannelMessage {
                id: msg.id.to_proto(),
                body: msg.body,
                timestamp: msg.sent_at.unix_timestamp() as u64,
                sender_id: msg.sender_id.to_proto(),
                nonce: Some(msg.nonce.as_u128().into()),
            })
            .collect::<Vec<_>>();
        response.send(proto::JoinChannelResponse {
            done: messages.len() < MESSAGE_COUNT_PER_PAGE,
            messages,
        })?;
        Ok(())
    }

    async fn leave_channel(
        self: Arc<Self>,
        request: TypedEnvelope<proto::LeaveChannel>,
    ) -> Result<()> {
        let user_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let channel_id = ChannelId::from_proto(request.payload.channel_id);
        if !self
            .app_state
            .db
            .can_user_access_channel(user_id, channel_id)
            .await?
        {
            Err(anyhow!("access denied"))?;
        }

        self.store()
            .await
            .leave_channel(request.sender_id, channel_id);

        Ok(())
    }

    async fn send_channel_message(
        self: Arc<Self>,
        request: TypedEnvelope<proto::SendChannelMessage>,
        response: Response<proto::SendChannelMessage>,
    ) -> Result<()> {
        let channel_id = ChannelId::from_proto(request.payload.channel_id);
        let user_id;
        let connection_ids;
        {
            let state = self.store().await;
            user_id = state.user_id_for_connection(request.sender_id)?;
            connection_ids = state.channel_connection_ids(channel_id)?;
        }

        // Validate the message body.
        let body = request.payload.body.trim().to_string();
        if body.len() > MAX_MESSAGE_LEN {
            return Err(anyhow!("message is too long"))?;
        }
        if body.is_empty() {
            return Err(anyhow!("message can't be blank"))?;
        }

        let timestamp = OffsetDateTime::now_utc();
        let nonce = request
            .payload
            .nonce
            .ok_or_else(|| anyhow!("nonce can't be blank"))?;

        let message_id = self
            .app_state
            .db
            .create_channel_message(channel_id, user_id, &body, timestamp, nonce.clone().into())
            .await?
            .to_proto();
        let message = proto::ChannelMessage {
            sender_id: user_id.to_proto(),
            id: message_id,
            body,
            timestamp: timestamp.unix_timestamp() as u64,
            nonce: Some(nonce),
        };
        broadcast(request.sender_id, connection_ids, |conn_id| {
            self.peer.send(
                conn_id,
                proto::ChannelMessageSent {
                    channel_id: channel_id.to_proto(),
                    message: Some(message.clone()),
                },
            )
        });
        response.send(proto::SendChannelMessageResponse {
            message: Some(message),
        })?;
        Ok(())
    }

    async fn get_channel_messages(
        self: Arc<Self>,
        request: TypedEnvelope<proto::GetChannelMessages>,
        response: Response<proto::GetChannelMessages>,
    ) -> Result<()> {
        let user_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let channel_id = ChannelId::from_proto(request.payload.channel_id);
        if !self
            .app_state
            .db
            .can_user_access_channel(user_id, channel_id)
            .await?
        {
            Err(anyhow!("access denied"))?;
        }

        let messages = self
            .app_state
            .db
            .get_channel_messages(
                channel_id,
                MESSAGE_COUNT_PER_PAGE,
                Some(MessageId::from_proto(request.payload.before_message_id)),
            )
            .await?
            .into_iter()
            .map(|msg| proto::ChannelMessage {
                id: msg.id.to_proto(),
                body: msg.body,
                timestamp: msg.sent_at.unix_timestamp() as u64,
                sender_id: msg.sender_id.to_proto(),
                nonce: Some(msg.nonce.as_u128().into()),
            })
            .collect::<Vec<_>>();
        response.send(proto::GetChannelMessagesResponse {
            done: messages.len() < MESSAGE_COUNT_PER_PAGE,
            messages,
        })?;
        Ok(())
    }

    async fn update_diff_base(
        self: Arc<Server>,
        request: TypedEnvelope<proto::UpdateDiffBase>,
    ) -> Result<()> {
        let receiver_ids = self.store().await.project_connection_ids(
            ProjectId::from_proto(request.payload.project_id),
            request.sender_id,
        )?;
        broadcast(request.sender_id, receiver_ids, |connection_id| {
            self.peer
                .forward_send(request.sender_id, connection_id, request.payload.clone())
        });
        Ok(())
    }

    async fn get_private_user_info(
        self: Arc<Self>,
        request: TypedEnvelope<proto::GetPrivateUserInfo>,
        response: Response<proto::GetPrivateUserInfo>,
    ) -> Result<()> {
        let user_id = self
            .store()
            .await
            .user_id_for_connection(request.sender_id)?;
        let metrics_id = self.app_state.db.get_user_metrics_id(user_id).await?;
        let user = self
            .app_state
            .db
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| anyhow!("user not found"))?;
        response.send(proto::GetPrivateUserInfoResponse {
            metrics_id,
            staff: user.admin,
        })?;
        Ok(())
    }

    pub(crate) async fn store(&self) -> StoreGuard<'_> {
        #[cfg(test)]
        tokio::task::yield_now().await;
        let guard = self.store.lock().await;
        #[cfg(test)]
        tokio::task::yield_now().await;
        StoreGuard {
            guard,
            _not_send: PhantomData,
        }
    }

    pub async fn snapshot<'a>(self: &'a Arc<Self>) -> ServerSnapshot<'a> {
        ServerSnapshot {
            store: self.store().await,
            peer: &self.peer,
        }
    }
}

impl<'a> Deref for StoreGuard<'a> {
    type Target = Store;

    fn deref(&self) -> &Self::Target {
        &*self.guard
    }
}

impl<'a> DerefMut for StoreGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.guard
    }
}

impl<'a> Drop for StoreGuard<'a> {
    fn drop(&mut self) {
        #[cfg(test)]
        self.check_invariants();
    }
}

impl Executor for RealExecutor {
    type Sleep = Sleep;

    fn spawn_detached<F: 'static + Send + Future<Output = ()>>(&self, future: F) {
        tokio::task::spawn(future);
    }

    fn sleep(&self, duration: Duration) -> Self::Sleep {
        tokio::time::sleep(duration)
    }
}

fn broadcast<F>(
    sender_id: ConnectionId,
    receiver_ids: impl IntoIterator<Item = ConnectionId>,
    mut f: F,
) where
    F: FnMut(ConnectionId) -> anyhow::Result<()>,
{
    for receiver_id in receiver_ids {
        if receiver_id != sender_id {
            f(receiver_id).trace_err();
        }
    }
}

lazy_static! {
    static ref ZED_PROTOCOL_VERSION: HeaderName = HeaderName::from_static("x-zed-protocol-version");
}

pub struct ProtocolVersion(u32);

impl Header for ProtocolVersion {
    fn name() -> &'static HeaderName {
        &ZED_PROTOCOL_VERSION
    }

    fn decode<'i, I>(values: &mut I) -> Result<Self, axum::headers::Error>
    where
        Self: Sized,
        I: Iterator<Item = &'i axum::http::HeaderValue>,
    {
        let version = values
            .next()
            .ok_or_else(axum::headers::Error::invalid)?
            .to_str()
            .map_err(|_| axum::headers::Error::invalid())?
            .parse()
            .map_err(|_| axum::headers::Error::invalid())?;
        Ok(Self(version))
    }

    fn encode<E: Extend<axum::http::HeaderValue>>(&self, values: &mut E) {
        values.extend([self.0.to_string().parse().unwrap()]);
    }
}

pub fn routes(server: Arc<Server>) -> Router<Body> {
    Router::new()
        .route("/rpc", get(handle_websocket_request))
        .layer(
            ServiceBuilder::new()
                .layer(Extension(server.app_state.clone()))
                .layer(middleware::from_fn(auth::validate_header)),
        )
        .route("/metrics", get(handle_metrics))
        .layer(Extension(server))
}

pub async fn handle_websocket_request(
    TypedHeader(ProtocolVersion(protocol_version)): TypedHeader<ProtocolVersion>,
    ConnectInfo(socket_address): ConnectInfo<SocketAddr>,
    Extension(server): Extension<Arc<Server>>,
    Extension(user): Extension<User>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    if protocol_version != rpc::PROTOCOL_VERSION {
        return (
            StatusCode::UPGRADE_REQUIRED,
            "client must be upgraded".to_string(),
        )
            .into_response();
    }
    let socket_address = socket_address.to_string();
    ws.on_upgrade(move |socket| {
        use util::ResultExt;
        let socket = socket
            .map_ok(to_tungstenite_message)
            .err_into()
            .with(|message| async move { Ok(to_axum_message(message)) });
        let connection = Connection::new(Box::pin(socket));
        async move {
            server
                .handle_connection(connection, socket_address, user, None, RealExecutor)
                .await
                .log_err();
        }
    })
}

pub async fn handle_metrics(Extension(server): Extension<Arc<Server>>) -> axum::response::Response {
    // We call `store_mut` here for its side effects of updating metrics.
    let metrics = server.store().await.metrics();
    METRIC_CONNECTIONS.set(metrics.connections as _);
    METRIC_REGISTERED_PROJECTS.set(metrics.registered_projects as _);
    METRIC_ACTIVE_PROJECTS.set(metrics.active_projects as _);
    METRIC_SHARED_PROJECTS.set(metrics.shared_projects as _);

    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    match encoder.encode_to_string(&metric_families) {
        Ok(string) => (StatusCode::OK, string).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to encode metrics {:?}", error),
        )
            .into_response(),
    }
}

fn to_axum_message(message: TungsteniteMessage) -> AxumMessage {
    match message {
        TungsteniteMessage::Text(payload) => AxumMessage::Text(payload),
        TungsteniteMessage::Binary(payload) => AxumMessage::Binary(payload),
        TungsteniteMessage::Ping(payload) => AxumMessage::Ping(payload),
        TungsteniteMessage::Pong(payload) => AxumMessage::Pong(payload),
        TungsteniteMessage::Close(frame) => AxumMessage::Close(frame.map(|frame| AxumCloseFrame {
            code: frame.code.into(),
            reason: frame.reason,
        })),
    }
}

fn to_tungstenite_message(message: AxumMessage) -> TungsteniteMessage {
    match message {
        AxumMessage::Text(payload) => TungsteniteMessage::Text(payload),
        AxumMessage::Binary(payload) => TungsteniteMessage::Binary(payload),
        AxumMessage::Ping(payload) => TungsteniteMessage::Ping(payload),
        AxumMessage::Pong(payload) => TungsteniteMessage::Pong(payload),
        AxumMessage::Close(frame) => {
            TungsteniteMessage::Close(frame.map(|frame| TungsteniteCloseFrame {
                code: frame.code.into(),
                reason: frame.reason,
            }))
        }
    }
}

pub trait ResultExt {
    type Ok;

    fn trace_err(self) -> Option<Self::Ok>;
}

impl<T, E> ResultExt for Result<T, E>
where
    E: std::fmt::Debug,
{
    type Ok = T;

    fn trace_err(self) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::error!("{:?}", error);
                None
            }
        }
    }
}
