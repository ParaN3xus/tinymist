use std::{pin::Pin, sync::Arc};
#[cfg(feature = "web")]
use std::{task::Context, task::Poll};

#[cfg(feature = "web")]
use base64::{Engine, engine::general_purpose};
use futures::{SinkExt, StreamExt, lock::Mutex};
use reflexo_typst::debug_loc::{DocumentPosition, ElementPoint};
#[cfg(feature = "web")]
use sync_ls::{PreviewMessageContent, PreviewNotificationParams};
use tinymist_std::{Error, error::IgnoreLogging};
use tokio::sync::{broadcast, mpsc};

use super::{editor::EditorActorRequest, render::RenderActorRequest};
use crate::{
    WsMessage,
    actor::{editor::DocToSrcJumpResolveRequest, render::ResolveSpanRequest},
};

#[cfg(not(feature = "web"))]
use futures::StreamExt;

// pub type CursorPosition = DocumentPosition;
pub type SrcToDocJumpInfo = DocumentPosition;

#[derive(Debug, Clone)]
pub enum WebviewActorRequest {
    ViewportPosition(DocumentPosition),
    SrcToDocJump(Vec<SrcToDocJumpInfo>),
    // CursorPosition(CursorPosition),
    CursorPaths(Vec<Vec<ElementPoint>>),
}

fn position_req(
    event: &'static str,
    DocumentPosition { page_no, x, y }: DocumentPosition,
) -> String {
    format!("{event},{page_no} {x} {y}")
}

fn positions_req(event: &'static str, positions: Vec<DocumentPosition>) -> String {
    format!("{event},")
        + &positions
            .iter()
            .map(|DocumentPosition { page_no, x, y }| format!("{page_no} {x} {y}"))
            .collect::<Vec<_>>()
            .join(",")
}

pub trait PreviewMessageWsMessageTransition {
    /// Convert to WsMessage
    fn to_ws_message(self) -> WsMessage;
    /// Create from WsMessage
    fn from_ws_message(msg: WsMessage) -> Self;
}

#[cfg(feature = "web")]
impl PreviewMessageWsMessageTransition for PreviewMessageContent {
    /// Convert to WsMessage
    fn to_ws_message(self) -> WsMessage {
        match self {
            PreviewMessageContent::Text { data } => WsMessage::Text(data),
            PreviewMessageContent::Binary { data } => {
                let bytes = general_purpose::STANDARD.decode(data).unwrap_or_default();
                WsMessage::Binary(bytes)
            }
        }
    }
    /// Create from WsMessage
    fn from_ws_message(msg: WsMessage) -> Self {
        match msg {
            WsMessage::Text(text) => PreviewMessageContent::Text { data: text },
            WsMessage::Binary(bytes) => PreviewMessageContent::Binary {
                data: general_purpose::STANDARD.encode(bytes),
            },
        }
    }
}

pub trait PreviewConnectionAdapter:
    futures::Sink<WsMessage, Error = reflexo_typst::Error>
    + futures::Stream<Item = Result<WsMessage, reflexo_typst::Error>>
{
    fn try_recv(self: Pin<&mut Self>) -> Result<Option<WsMessage>, mpsc::error::TryRecvError>;
}

#[cfg(feature = "web")]
pub struct LspMessageAdapter {
    rx: mpsc::UnboundedReceiver<WsMessage>,
    send_fn: Box<dyn Fn(&PreviewNotificationParams) + Send + Sync>,
}

#[cfg(feature = "web")]
impl Default for LspMessageAdapter {
    fn default() -> Self {
        let (_, rx) = mpsc::unbounded_channel();
        Self {
            rx,
            send_fn: Box::new(|_| {}),
        }
    }
}

#[cfg(feature = "web")]
impl LspMessageAdapter {
    pub fn new<F>(rx: mpsc::UnboundedReceiver<WsMessage>, send_fn: F) -> Self
    where
        F: Fn(&PreviewNotificationParams) + Send + Sync + 'static,
    {
        Self {
            rx,
            send_fn: Box::new(send_fn),
        }
    }
}

#[cfg(feature = "web")]
impl PreviewConnectionAdapter for LspMessageAdapter {
    fn try_recv(
        mut self: Pin<&mut LspMessageAdapter>,
    ) -> Result<Option<WsMessage>, mpsc::error::TryRecvError> {
        match self.rx.try_recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(mpsc::error::TryRecvError::Empty) => Err(mpsc::error::TryRecvError::Empty),
            Err(mpsc::error::TryRecvError::Disconnected) => Ok(None),
        }
    }
}

#[cfg(feature = "web")]
impl futures::Sink<WsMessage> for LspMessageAdapter {
    type Error = reflexo_typst::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    // Preview -> LSP -> Webview
    fn start_send(self: Pin<&mut Self>, _item: WsMessage) -> Result<(), Self::Error> {
        (self.send_fn)(&PreviewNotificationParams {
            content: PreviewMessageContent::from_ws_message(_item),
        });
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "web")]
impl futures::Stream for LspMessageAdapter {
    type Item = Result<WsMessage, reflexo_typst::Error>;

    // Webview -> LSP -> Preview
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(msg)) => Poll::Ready(Some(Ok(msg))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(feature = "web")]
impl std::future::Future for LspMessageAdapter {
    type Output = Self;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let adapter = std::mem::replace(&mut *self, LspMessageAdapter::default());
        Poll::Ready(adapter)
    }
}

pub struct WebsocketAdapter<S>(pub S);

impl<S> PreviewConnectionAdapter for WebsocketAdapter<S>
where
    S: futures::Sink<WsMessage, Error = reflexo_typst::Error>
        + futures::Stream<Item = Result<WsMessage, reflexo_typst::Error>>
        + Unpin,
{
    fn try_recv(self: Pin<&mut Self>) -> Result<Option<WsMessage>, mpsc::error::TryRecvError> {
        use futures::StreamExt;
        use std::task::{Context, Poll};

        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        let this = self.get_mut();

        match this.0.poll_next_unpin(&mut cx) {
            Poll::Ready(Some(Ok(msg))) => Ok(Some(msg)),
            Poll::Ready(Some(Err(_))) => Err(mpsc::error::TryRecvError::Disconnected),
            Poll::Ready(None) => Err(mpsc::error::TryRecvError::Disconnected),
            Poll::Pending => Err(mpsc::error::TryRecvError::Empty),
        }
    }
}

impl<S> futures::Sink<WsMessage> for WebsocketAdapter<S>
where
    S: futures::Sink<WsMessage, Error = reflexo_typst::Error> + Unpin,
{
    type Error = reflexo_typst::Error;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.0).poll_ready(cx)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
        std::pin::Pin::new(&mut self.0).start_send(item)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.0).poll_close(cx)
    }
}

impl<S> futures::Stream for WebsocketAdapter<S>
where
    S: futures::Stream<Item = Result<WsMessage, reflexo_typst::Error>> + Unpin,
{
    type Item = Result<WsMessage, reflexo_typst::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.0).poll_next(cx)
    }
}

pub struct WebviewActor {
    webview_websocket_conn: Arc<Mutex<Pin<Box<dyn PreviewConnectionAdapter + Send>>>>,

    svg_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    mailbox: broadcast::Receiver<WebviewActorRequest>,

    broadcast_sender: broadcast::Sender<WebviewActorRequest>,
    editor_sender: mpsc::UnboundedSender<EditorActorRequest>,
    render_sender: broadcast::Sender<RenderActorRequest>,
}

pub struct Channels {
    pub svg: (
        mpsc::UnboundedSender<Vec<u8>>,
        mpsc::UnboundedReceiver<Vec<u8>>,
    ),
}

impl WebviewActor {
    pub fn set_up_channels() -> Channels {
        Channels {
            svg: mpsc::unbounded_channel(),
        }
    }
    pub fn new(
        websocket_conn: Arc<Mutex<Pin<Box<dyn PreviewConnectionAdapter + Send>>>>,
        svg_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
        broadcast_sender: broadcast::Sender<WebviewActorRequest>,
        mailbox: broadcast::Receiver<WebviewActorRequest>,
        editor_sender: mpsc::UnboundedSender<EditorActorRequest>,
        render_sender: broadcast::Sender<RenderActorRequest>,
    ) -> Self {
        Self {
            webview_websocket_conn: websocket_conn,
            svg_receiver,
            mailbox,
            broadcast_sender,
            editor_sender,
            render_sender,
        }
    }

    #[cfg(not(feature = "web"))]
    pub async fn run(mut self) {
        loop {
            if self.step_async().await {
                break;
            }
        }
        log::info!("WebviewActor: exiting");
    }

    pub async fn handle_mailbox_message(&mut self, msg: WebviewActorRequest) {
        log::trace!("WebviewActor: received message from mailbox: {msg:?}");
        let mut conn = self.webview_websocket_conn.lock().await;
        match msg {
            WebviewActorRequest::SrcToDocJump(jump_info) => {
                let msg = positions_req("jump", jump_info);
                conn.as_mut()
                    .send(WsMessage::Binary(msg.into()))
                    .await
                    .log_error("WebViewActor");
            }
            WebviewActorRequest::ViewportPosition(jump_info) => {
                let msg = position_req("viewport", jump_info);
                conn.as_mut()
                    .send(WsMessage::Binary(msg.into()))
                    .await
                    .log_error("WebViewActor");
            }
            WebviewActorRequest::CursorPaths(jump_info) => {
                let json = serde_json::to_string(&jump_info).unwrap();
                let msg = format!("cursor-paths,{json}");
                conn.as_mut()
                    .send(WsMessage::Binary(msg.into()))
                    .await
                    .log_error("WebViewActor");
            }
        }
    }

    pub async fn handle_svg_receiver_message(&mut self, svg: Vec<u8>) {
        let _scope = typst_timing::TimingScope::new("webview_actor_send_svg");
        self.webview_websocket_conn
            .lock()
            .await
            .send(WsMessage::Binary(svg.into()))
            .await
            .log_error("WebViewActor");
    }

    pub async fn handle_websocket_message(&mut self, msg: Result<WsMessage, Error>) -> bool {
        log::trace!("WebviewActor: received message from websocket: {msg:?}");
        let Ok(msg) = msg else {
            // only for system
            log::info!(
                "WebviewActor: no more messages from websocket: {}",
                msg.unwrap_err()
            );
            return true;
        };
        let WsMessage::Text(msg) = msg else {
            log::info!("WebviewActor: received non-text message from websocket: {msg:?}");
            let _ = self
                .webview_websocket_conn
                .lock()
                .await
                .send(WsMessage::Text(format!(
                    "Webview Actor: error, received non-text message: {msg:?}"
                )))
                .await;
            return true;
        };
        if msg == "current" {
            log::debug!("Webview Actor: received current, requesting RenderFullLatest");
            self.render_sender
                .send(RenderActorRequest::RenderFullLatest)
                .log_error("WebViewActor");
        } else if msg.starts_with("srclocation") {
            let location = msg.split(' ').nth(1).unwrap();
            self.editor_sender
                .send(EditorActorRequest::DocToSrcJumpResolve(
                    DocToSrcJumpResolveRequest {
                        span: location.trim().to_owned(),
                    },
                ))
                .log_error("WebViewActor");
        } else if msg.starts_with("outline-sync") {
            let location = msg.split(',').nth(1).unwrap();
            let location = location.split(' ').collect::<Vec<&str>>();
            let page_no = location[0].parse().unwrap();
            let x = location.get(1).map(|s| s.parse().unwrap()).unwrap_or(0.);
            let y = location.get(2).map(|s| s.parse().unwrap()).unwrap_or(0.);
            let pos = DocumentPosition { page_no, x, y };

            self.broadcast_sender
                .send(WebviewActorRequest::ViewportPosition(pos))
                .log_error("WebViewActor");
        } else if msg.starts_with("srcpath") {
            let path = msg.split(' ').nth(1).unwrap();
            let path = serde_json::from_str(path);
            if let Ok(path) = path {
                let path: Vec<(u32, u32, String)> = path;
                let path = path.into_iter().map(ElementPoint::from).collect::<Vec<_>>();
                self.render_sender
                    .send(RenderActorRequest::WebviewResolveSpan(ResolveSpanRequest(
                        path,
                    )))
                    .log_error("WebViewActor");
            };
        } else if msg.starts_with("src-point") {
            let path = msg.split(' ').nth(1).unwrap();
            let path = serde_json::from_str(path);
            if let Ok(path) = path {
                self.render_sender
                    .send(RenderActorRequest::WebviewResolveFrameLoc(path))
                    .log_error("WebViewActor");
            };
        } else {
            let err = self
                .webview_websocket_conn
                .lock()
                .await
                .send(WsMessage::Text(format!(
                    "error, received unknown message: {msg}"
                )))
                .await;
            log::info!("WebviewActor: received unknown message from websocket: {msg} {err:?}");
            return true;
        }
        false
    }

    #[cfg(not(feature = "web"))]
    pub async fn step_async(&mut self) -> bool {
        tokio::select! {
            Ok(msg) = self.mailbox.recv() =>{
                log::trace!("WebviewActor: received message from mailbox: {msg:?}");
                self.handle_mailbox_message(msg).await;
                false
            }
            Some(svg) = self.svg_receiver.recv() => {
                log::trace!("WebviewActor: received svg from renderer");
                self.handle_svg_receiver_message(svg).await;
                false
            }
            websocket_result = async {
                let mut conn = self.webview_websocket_conn.lock().await;
                conn.next().await
            } =>{
                if let Some(msg) = websocket_result {
                    self.handle_websocket_message(msg).await
                } else {
                    true
                }
            }
            else => {
                true
            }
        }
    }

    #[cfg(feature = "web")]
    async fn step_sync_inner(&mut self) -> bool {
        while let Ok(msg) = self.mailbox.try_recv() {
            log::trace!("WebviewActor: received message from mailbox: {msg:?}");
            self.handle_mailbox_message(msg).await;
        }
        while let Ok(svg) = self.svg_receiver.try_recv() {
            log::trace!("WebviewActor: received svg from renderer");
            self.handle_svg_receiver_message(svg).await;
        }
        let mut websocket_messages = Vec::new();
        {
            let mut conn = self.webview_websocket_conn.lock().await;
            loop {
                match conn.as_mut().try_recv() {
                    Ok(Some(msg)) => {
                        websocket_messages.push(Ok(msg));
                    }
                    Ok(None) => {
                        drop(conn);
                        return true; // conn closed
                    }
                    Err(_) => {
                        break; // empty
                    }
                }
            }
            drop(conn);
        }
        for msg in websocket_messages {
            let should_exit = self.handle_websocket_message(msg).await;
            if should_exit {
                return true;
            }
        }

        false
    }

    #[cfg(feature = "web")]
    pub fn step(&mut self) -> bool {
        futures::executor::block_on(self.step_sync_inner())
    }
}
