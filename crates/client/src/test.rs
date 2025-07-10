use crate::{
    http::{self, HttpClient, Request, Response},
    Client, Connection, Credentials, EstablishConnectionError, UserStore,
};
use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, stream::BoxStream, Future, StreamExt};
use gpui::{executor, ModelHandle, TestAppContext};
use parking_lot::Mutex;
use rpc::{
    proto::{self, GetPrivateUserInfo, GetPrivateUserInfoResponse},
    ConnectionId, Peer, Receipt, TypedEnvelope,
};
use std::{fmt, rc::Rc, sync::Arc};

pub struct FakeServer {
    peer: Arc<Peer>,
    state: Arc<Mutex<FakeServerState>>,
    user_id: u64,
    executor: Rc<executor::Foreground>,
}

#[derive(Default)]
struct FakeServerState {
    incoming: Option<BoxStream<'static, Box<dyn proto::AnyTypedEnvelope>>>,
    connection_id: Option<ConnectionId>,
    forbid_connections: bool,
    auth_count: usize,
    access_token: usize,
}

impl FakeServer {
    pub async fn for_client(
        client_user_id: u64,
        client: &Arc<Client>,
        cx: &TestAppContext,
    ) -> Self {
        let server = Self {
            peer: Peer::new(),
            state: Default::default(),
            user_id: client_user_id,
            executor: cx.foreground(),
        };

        client
            .override_authenticate({
                let state = Arc::downgrade(&server.state);
                move |cx| {
                    let state = state.clone();
                    cx.spawn(move |_| async move {
                        let state = state.upgrade().ok_or_else(|| anyhow!("server dropped"))?;
                        let mut state = state.lock();
                        state.auth_count += 1;
                        let access_token = state.access_token.to_string();
                        Ok(Credentials {
                            user_id: client_user_id,
                            access_token,
                        })
                    })
                }
            })
            .override_establish_connection({
                let peer = Arc::downgrade(&server.peer);
                let state = Arc::downgrade(&server.state);
                move |credentials, cx| {
                    let peer = peer.clone();
                    let state = state.clone();
                    let credentials = credentials.clone();
                    cx.spawn(move |cx| async move {
                        let state = state.upgrade().ok_or_else(|| anyhow!("server dropped"))?;
                        let peer = peer.upgrade().ok_or_else(|| anyhow!("server dropped"))?;
                        if state.lock().forbid_connections {
                            Err(EstablishConnectionError::Other(anyhow!(
                                "server is forbidding connections"
                            )))?
                        }

                        assert_eq!(credentials.user_id, client_user_id);

                        if credentials.access_token != state.lock().access_token.to_string() {
                            Err(EstablishConnectionError::Unauthorized)?
                        }

                        let (client_conn, server_conn, _) = Connection::in_memory(cx.background());
                        let (connection_id, io, incoming) =
                            peer.add_test_connection(server_conn, cx.background());
                        cx.background().spawn(io).detach();
                        {
                            let mut state = state.lock();
                            state.connection_id = Some(connection_id);
                            state.incoming = Some(incoming);
                        }
                        peer.send(
                            connection_id,
                            proto::Hello {
                                peer_id: connection_id.0,
                            },
                        )
                        .unwrap();

                        Ok(client_conn)
                    })
                }
            });

        client
            .authenticate_and_connect(false, &cx.to_async())
            .await
            .unwrap();

        server
    }

    pub fn disconnect(&self) {
        if self.state.lock().connection_id.is_some() {
            self.peer.disconnect(self.connection_id());
            let mut state = self.state.lock();
            state.connection_id.take();
            state.incoming.take();
        }
    }

    pub fn auth_count(&self) -> usize {
        self.state.lock().auth_count
    }

    pub fn roll_access_token(&self) {
        self.state.lock().access_token += 1;
    }

    pub fn forbid_connections(&self) {
        self.state.lock().forbid_connections = true;
    }

    pub fn allow_connections(&self) {
        self.state.lock().forbid_connections = false;
    }

    pub fn send<T: proto::EnvelopedMessage>(&self, message: T) {
        self.peer.send(self.connection_id(), message).unwrap();
    }

    #[allow(clippy::await_holding_lock)]
    pub async fn receive<M: proto::EnvelopedMessage>(&self) -> Result<TypedEnvelope<M>> {
        self.executor.start_waiting();

        loop {
            let message = self
                .state
                .lock()
                .incoming
                .as_mut()
                .expect("not connected")
                .next()
                .await
                .ok_or_else(|| anyhow!("other half hung up"))?;
            self.executor.finish_waiting();
            let type_name = message.payload_type_name();
            let message = message.into_any();

            if message.is::<TypedEnvelope<M>>() {
                return Ok(*message.downcast().unwrap());
            }

            if message.is::<TypedEnvelope<GetPrivateUserInfo>>() {
                self.respond(
                    message
                        .downcast::<TypedEnvelope<GetPrivateUserInfo>>()
                        .unwrap()
                        .receipt(),
                    GetPrivateUserInfoResponse {
                        metrics_id: "the-metrics-id".into(),
                        staff: false,
                    },
                )
                .await;
                continue;
            }

            panic!(
                "fake server received unexpected message type: {:?}",
                type_name
            );
        }
    }

    pub async fn respond<T: proto::RequestMessage>(
        &self,
        receipt: Receipt<T>,
        response: T::Response,
    ) {
        self.peer.respond(receipt, response).unwrap()
    }

    fn connection_id(&self) -> ConnectionId {
        self.state.lock().connection_id.expect("not connected")
    }

    pub async fn build_user_store(
        &self,
        client: Arc<Client>,
        cx: &mut TestAppContext,
    ) -> ModelHandle<UserStore> {
        let http_client = FakeHttpClient::with_404_response();
        let user_store = cx.add_model(|cx| UserStore::new(client, http_client, cx));
        assert_eq!(
            self.receive::<proto::GetUsers>()
                .await
                .unwrap()
                .payload
                .user_ids,
            &[self.user_id]
        );
        user_store
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.disconnect();
    }
}

pub struct FakeHttpClient {
    handler: Box<
        dyn 'static
            + Send
            + Sync
            + Fn(Request) -> BoxFuture<'static, Result<Response, http::Error>>,
    >,
}

impl FakeHttpClient {
    pub fn create<Fut, F>(handler: F) -> Arc<dyn HttpClient>
    where
        Fut: 'static + Send + Future<Output = Result<Response, http::Error>>,
        F: 'static + Send + Sync + Fn(Request) -> Fut,
    {
        Arc::new(Self {
            handler: Box::new(move |req| Box::pin(handler(req))),
        })
    }

    pub fn with_404_response() -> Arc<dyn HttpClient> {
        Self::create(|_| async move {
            Ok(isahc::Response::builder()
                .status(404)
                .body(Default::default())
                .unwrap())
        })
    }
}

impl fmt::Debug for FakeHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeHttpClient").finish()
    }
}

impl HttpClient for FakeHttpClient {
    fn send(&self, req: Request) -> BoxFuture<Result<Response, crate::http::Error>> {
        let future = (self.handler)(req);
        Box::pin(async move { future.await.map(Into::into) })
    }
}
