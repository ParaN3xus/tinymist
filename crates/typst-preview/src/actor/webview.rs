use std::sync::Arc;
#[cfg(feature = "web")]
use std::{pin::Pin, task::Context, task::Poll};

use base64::{Engine, engine::general_purpose};
use futures::{SinkExt, StreamExt, lock::Mutex};
use reflexo_typst::debug_loc::{DocumentPosition, ElementPoint};
#[cfg(feature = "web")]
use sync_ls::{PreviewMessageContent, PreviewNotificationParams};
use tinymist_std::error::IgnoreLogging;
use tokio::sync::{broadcast, mpsc};

use super::{editor::EditorActorRequest, render::RenderActorRequest};
use crate::{
    WsMessage,
    actor::{editor::DocToSrcJumpResolveRequest, render::ResolveSpanRequest},
};

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

impl std::future::Future for LspMessageAdapter {
    type Output = Self;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let adapter = std::mem::replace(&mut *self, LspMessageAdapter::default());
        Poll::Ready(adapter)
    }
}

pub trait PreviewMessageWsMessageTransition {
    /// Convert to WsMessage
    fn to_ws_message(self) -> WsMessage;
    /// Create from WsMessage
    fn from_ws_message(msg: WsMessage) -> Self;
}

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

#[cfg(feature = "web")]
pub(crate) type WebviewActorConnectionAdaptor = LspMessageAdapter;
#[cfg(not(feature = "web"))]
pub(crate) type WebviewActorConnectionAdaptor = HyperWebsocketStream;

pub struct WebviewActor {
    webview_websocket_conn: Arc<Mutex<WebviewActorConnectionAdaptor>>,

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
        websocket_conn: Arc<Mutex<WebviewActorConnectionAdaptor>>,
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

    pub async fn run(mut self) {
        loop {
            if self.handle_async().await {
                break;
            }
        }
        log::info!("WebviewActor: exiting");
    }

    pub async fn handle_async(&mut self) -> bool {
        tokio::select! {
            Ok(msg) = self.mailbox.recv() => {
                log::trace!("WebviewActor: received message from mailbox: {msg:?}");
                match msg {
                    WebviewActorRequest::SrcToDocJump(jump_info) => {
                        let msg = positions_req("jump", jump_info);
                        self.webview_websocket_conn.send(WsMessage::Binary(msg.into()))
                          .await.log_error("WebViewActor");
                    }
                    WebviewActorRequest::ViewportPosition(jump_info) => {
                        let msg = position_req("viewport", jump_info);
                        self.webview_websocket_conn.send(WsMessage::Binary(msg.into()))
                          .await.log_error("WebViewActor");
                    }
                    WebviewActorRequest::CursorPaths(jump_info) => {
                        let json = serde_json::to_string(&jump_info).unwrap();
                        let msg = format!("cursor-paths,{json}");
                        self.webview_websocket_conn.send(WsMessage::Binary(msg.into()))
                          .await.log_error("WebViewActor");
                    }
                }
                false
            }
            Some(svg) = self.svg_receiver.recv() => {
                log::trace!("WebviewActor: received svg from renderer");
                let _scope = typst_timing::TimingScope::new("webview_actor_send_svg");
                self.webview_websocket_conn.send(WsMessage::Binary(svg.into()))
                .await.log_error("WebViewActor");
                false
            }
            websocket_result = async {
                let mut conn = self.webview_websocket_conn.lock().await;
                conn.next().await
            } => {
                if let Some(msg) = websocket_result {
                    log::trace!("WebviewActor: received message from websocket: {msg:?}");
                    let Ok(msg) = msg else {
                        log::info!("WebviewActor: no more messages from websocket: {}", msg.unwrap_err());
                        return true;
                    };
                    let WsMessage::Text(msg) = msg else {
                        log::info!("WebviewActor: received non-text message from websocket: {msg:?}");
                        let _ = self.webview_websocket_conn.lock().await.send(WsMessage::Text(format!("Webview Actor: error, received non-text message: {msg:?}")))
                        .await;
                        return true;
                    };
                    if msg == "current" {
                        self.render_sender.send(RenderActorRequest::RenderFullLatest).log_error("WebViewActor");
                    } else if msg.starts_with("srclocation") {
                        let location = msg.split(' ').nth(1).unwrap();
                        self.editor_sender.send(EditorActorRequest::DocToSrcJumpResolve(
                            DocToSrcJumpResolveRequest {
                                span: location.trim().to_owned(),
                            },
                        )).log_error("WebViewActor");
                    } else if msg.starts_with("outline-sync") {
                        let location = msg.split(',').nth(1).unwrap();
                        let location = location.split(' ').collect::<Vec::<&str>>();
                        let page_no = location[0].parse().unwrap();
                        let x = location.get(1).map(|s| s.parse().unwrap()).unwrap_or(0.);
                        let y = location.get(2).map(|s| s.parse().unwrap()).unwrap_or(0.);
                        let pos = DocumentPosition { page_no, x, y };

                        self.broadcast_sender.send(WebviewActorRequest::ViewportPosition(pos)).log_error("WebViewActor");
                    } else if msg.starts_with("srcpath") {
                        let path = msg.split(' ').nth(1).unwrap();
                        let path = serde_json::from_str(path);
                        if let Ok(path) = path {
                            let path: Vec<(u32, u32, String)> = path;
                            let path = path.into_iter().map(ElementPoint::from).collect::<Vec<_>>();
                            self.render_sender.send(RenderActorRequest::WebviewResolveSpan(ResolveSpanRequest(path))).log_error("WebViewActor");
                        };
                    } else if msg.starts_with("src-point") {
                        let path = msg.split(' ').nth(1).unwrap();
                        let path = serde_json::from_str(path);
                        if let Ok(path) = path {
                            self.render_sender.send(RenderActorRequest::WebviewResolveFrameLoc(path)).log_error("WebViewActor");
                        };
                    } else {
                        let err = self.webview_websocket_conn.lock().await.send(WsMessage::Text(format!("error, received unknown message: {msg}"))).await;
                        log::info!("WebviewActor: received unknown message from websocket: {msg} {err:?}");
                        return true;
                    }
                    false
                } else {
                    true
                }
            }
            else => {
                true
            }
        }
    }

    pub fn handle(&mut self) -> bool {
        futures::executor::block_on(self.handle_async())
    }
}
