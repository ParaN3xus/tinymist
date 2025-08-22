// todo: remove me
#![allow(missing_docs)]

mod actor;
mod debug_loc;
mod outline;

pub use crate::actor::editor::{
    CompileStatus, ControlPlaneMessage, ControlPlaneResponse, ControlPlaneRx, ControlPlaneTx,
    PanelScrollByPositionRequest,
};
#[cfg(feature = "web")]
use crate::actor::render::{OutlineRenderActor, RenderActor};
#[cfg(feature = "web")]
use crate::actor::webview::WebviewActor;
pub use crate::actor::webview::{LspMessageAdapter, PreviewMessageWsMessageTransition};
pub use crate::outline::Outline;

#[cfg(feature = "web")]
use std::cell::RefCell;
#[cfg(feature = "web")]
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::{collections::HashMap, future::Future, path::PathBuf, pin::Pin};

use futures::StreamExt;
use futures::lock::Mutex;
use futures::sink::SinkExt;
use reflexo_typst::Error;
use reflexo_typst::args::TaskWhen;
use reflexo_typst::debug_loc::{DocumentPosition, SourceSpanOffset};
use serde::{Deserialize, Serialize};
use tinymist_std::error::IgnoreLogging;
use tinymist_std::typst::TypstDocument;
use tokio::sync::{broadcast, mpsc};
use typst::{layout::Position, syntax::Span};

use std::sync::Mutex as SyncMutex;

use crate::actor::editor::{EditorActor, EditorActorRequest};
use crate::actor::render::RenderActorRequest;
use crate::actor::webview::{WebviewActorConnectionAdaptor, WebviewActorRequest};
use crate::debug_loc::SpanInterner;

type StopFuture = Pin<Box<dyn Future<Output = ()> + Send + Sync>>;

// enum Preview Mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum PreviewMode {
    /// Preview mode for regular document
    #[cfg_attr(feature = "clap", clap(name = "document"))]
    Document,

    /// Preview mode for slide
    #[cfg_attr(feature = "clap", clap(name = "slide"))]
    Slide,
}

/// The configurations about the preview.
#[derive(Debug, Clone, Default)]
pub struct PreviewConfig {
    /// Whether to enable partial rendering.
    pub enable_partial_rendering: bool,
    /// The refresh style of the preview.
    pub refresh_style: TaskWhen,
    /// The invert colors setting for the preview.
    pub invert_colors: String,
}

/// Get the HTML for the frontend by a given preview mode and server to connect
pub fn frontend_html(html: &str, mode: PreviewMode, to: &str) -> String {
    let mode = match mode {
        PreviewMode::Document => "Doc",
        PreviewMode::Slide => "Slide",
    };

    html.replace("ws://127.0.0.1:23625", to).replace(
        "preview-arg:previewMode:Doc",
        format!("preview-arg:previewMode:{mode}").as_str(),
    )
}

/// Shortcut to create a previewer.
pub async fn preview<T>(config: PreviewConfig, conn: ControlPlaneTx, server: Arc<T>) -> Previewer
where
    T: EditorServer,
{
    PreviewBuilder::new(config).build(conn, server).await
}

pub struct Previewer {
    stop: Option<Box<dyn FnOnce() -> StopFuture + Send + Sync>>,
    data_plane_handle: Option<tokio::task::JoinHandle<()>>,
    data_plane_resources: Option<(DataPlane, Option<mpsc::Sender<()>>, mpsc::Receiver<()>)>,
    control_plane_handle: Option<tokio::task::JoinHandle<()>>,
    #[cfg(feature = "web")]
    editor_actor: Option<EditorActor>,
    #[cfg(feature = "web")]
    render_actor: Arc<SyncMutex<Option<RenderActor>>>,
    #[cfg(feature = "web")]
    webview_actor: Arc<SyncMutex<Option<WebviewActor>>>,
    #[cfg(feature = "web")]
    outline_render_actor: Arc<SyncMutex<Option<OutlineRenderActor>>>,
}

impl Previewer {
    /// Send stop requests to preview actors.
    pub async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop().await;
        }
    }

    pub fn schedule_async(&mut self) {
        if let Some(editor_actor) = self.editor_actor.as_mut() {
            editor_actor.step();
        }

        if let Some(ref mut webview_actor) = *self.webview_actor.lock().unwrap() {
            webview_actor.step();
        }
        if let Some(ref mut render_actor) = *self.render_actor.lock().unwrap() {
            render_actor.step();
        }
        if let Some(ref mut webview_actor) = *self.webview_actor.lock().unwrap() {
            webview_actor.step();
        }

        if let Some(editor_actor) = self.editor_actor.as_mut() {
            editor_actor.step();
        }

        if let Some(ref mut outline_render) = *self.outline_render_actor.lock().unwrap() {
            outline_render.step();
        }
    }

    /// Join all the previewer actors. Note: send stop request first.
    pub async fn join(mut self) {
        let data_plane_handle = self.data_plane_handle.take().expect("must bind data plane");
        let _ = tokio::join!(data_plane_handle, self.control_plane_handle.unwrap());
    }

    /// Listen streams that accepting data plane messages.
    pub fn start_data_plane<S: 'static, SFut: Future<Output = S> + Send + 'static>(
        &mut self,
        mut streams: mpsc::UnboundedReceiver<SFut>,
        caster: impl Fn(S) -> Result<Mutex<WebviewActorConnectionAdaptor>, Error>
        + Send
        + Sync
        + Copy
        + 'static,
    ) {
        let idle_timeout = reflexo_typst::time::Duration::from_secs(5);
        let (conn_handler, shutdown_tx, mut shutdown_data_plane_rx) =
            self.data_plane_resources.take().unwrap();
        let (alive_tx, mut alive_rx) = mpsc::unbounded_channel::<()>();
        let webview_actor_ref = self.webview_actor.clone();
        let render_actor_ref = self.render_actor.clone();
        let outline_render_actor_ref = self.outline_render_actor.clone();
        let recv = move |conn| {
            let h = conn_handler.clone();
            let alive_tx = alive_tx.clone();

            spawn_cpu(async move {
                let conn = Arc::new(caster(conn.await).unwrap());

                if h.enable_partial_rendering {
                    conn.lock()
                        .await
                        .send(WsMessage::Binary("partial-rendering,true".into()))
                        .await
                        .log_error("SendPartialRendering");
                }
                if !h.invert_colors.is_empty() {
                    conn.lock()
                        .await
                        .send(WsMessage::Binary(
                            format!("invert-colors,{}", h.invert_colors).into(),
                        ))
                        .await
                        .log_error("SendInvertColor");
                }
                let actor::webview::Channels { svg } =
                    actor::webview::WebviewActor::set_up_channels();
                let webview_actor = actor::webview::WebviewActor::new(
                    conn,
                    svg.1,
                    h.webview_tx.clone(),
                    h.webview_tx.subscribe(),
                    h.editor_tx.clone(),
                    h.renderer_tx.clone(),
                );
                *webview_actor_ref.lock().unwrap() = Some(webview_actor);

                let render_actor = actor::render::RenderActor::new(
                    h.renderer_tx.subscribe(),
                    h.doc_sender.clone(),
                    h.editor_tx.clone(),
                    svg.0,
                    h.webview_tx,
                );
                *render_actor_ref.lock().unwrap() = Some(render_actor);

                #[cfg(not(feature = "web"))]
                tokio::spawn(render_actor.run());

                let outline_render_actor = actor::render::OutlineRenderActor::new(
                    h.renderer_tx.subscribe(),
                    h.doc_sender.clone(),
                    h.editor_tx.clone(),
                    h.span_interner,
                );
                *outline_render_actor_ref.lock().unwrap() = Some(outline_render_actor);

                #[cfg(not(feature = "web"))]
                tokio::spawn(outline_render_actor.run());

                struct FinallySend(mpsc::UnboundedSender<()>);
                impl Drop for FinallySend {
                    fn drop(&mut self) {
                        let _ = self.0.send(());
                    }
                }

                let _send = FinallySend(alive_tx);
                #[cfg(not(feature = "web"))]
                webview_actor.run().await;
            })
        };

        let data_plane_handle = spawn_cpu(async move {
            let mut alive_cnt = 0;
            #[cfg(not(feature = "web"))]
            let mut shutdown_bell = tokio::time::interval(idle_timeout);
            #[cfg(feature = "web")]
            let mut shutdown_bell = wasmtimer::tokio::interval(idle_timeout);
            loop {
                if shutdown_tx.is_some() {
                    shutdown_bell.reset();
                }
                tokio::select! {
                    Some(()) = shutdown_data_plane_rx.recv() => {
                        log::info!("Data plane server shutdown");
                        return;
                    }
                    Some(stream) = streams.recv() => {
                        alive_cnt += 1;
                        #[cfg(not(feature = "web"))]
                        tokio::spawn(recv(stream));
                        #[cfg(feature = "web")]
                        {
                            recv(stream);
                            return;
                        }
                    },
                    _ = alive_rx.recv() => {
                        alive_cnt -= 1;
                    }
                    _ = shutdown_bell.tick(), if alive_cnt == 0 && shutdown_tx.is_some() => {
                        let shutdown_tx = shutdown_tx.expect("scheduled shutdown without shutdown_tx");
                        log::info!(
                            "Data plane server has been idle for {idle_timeout:?}, shutting down."
                        );
                        let _ = shutdown_tx.send(()).await;
                        log::info!("Data plane server shutdown");
                        return;
                    }
                }
            }
        });

        #[cfg(feature = "web")]
        {
            self.data_plane_handle = None;
        }
        #[cfg(not(feature = "web"))]
        {
            self.data_plane_handle = Some(data_plane_handle);
        }
    }
}

type MpScChannel<T> = (mpsc::UnboundedSender<T>, mpsc::UnboundedReceiver<T>);
type BroadcastChannel<T> = (broadcast::Sender<T>, broadcast::Receiver<T>);

pub struct PreviewBuilder {
    config: PreviewConfig,
    shutdown_tx: Option<mpsc::Sender<()>>,
    renderer_mailbox: BroadcastChannel<RenderActorRequest>,
    editor_conn: MpScChannel<EditorActorRequest>,
    webview_conn: BroadcastChannel<WebviewActorRequest>,
    doc_sender: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,

    compile_watcher: OnceLock<Arc<CompileWatcher>>,
}

impl PreviewBuilder {
    pub fn new(config: PreviewConfig) -> Self {
        Self {
            config,
            shutdown_tx: None,
            renderer_mailbox: broadcast::channel(1024),
            editor_conn: mpsc::unbounded_channel(),
            webview_conn: broadcast::channel(32),
            doc_sender: Arc::new(parking_lot::RwLock::new(None)),
            compile_watcher: OnceLock::new(),
        }
    }

    pub fn with_shutdown_tx(mut self, shutdown_tx: mpsc::Sender<()>) -> Self {
        self.shutdown_tx = Some(shutdown_tx);
        self
    }

    pub fn compile_watcher(&self, task_id: String) -> &Arc<CompileWatcher> {
        log::info!(
            "PreviewBuilder: creating compile watcher with id: {}",
            task_id
        );
        self.compile_watcher.get_or_init(|| {
            Arc::new(CompileWatcher {
                task_id,
                when: self.config.refresh_style.clone(),
                doc_sender: self.doc_sender.clone(),
                editor_tx: self.editor_conn.0.clone(),
                render_tx: self.renderer_mailbox.0.clone(),
            })
        })
    }

    pub async fn build<T: EditorServer>(self, conn: ControlPlaneTx, server: Arc<T>) -> Previewer {
        let PreviewBuilder {
            config,
            shutdown_tx,
            renderer_mailbox,
            editor_conn: (editor_tx, editor_rx),
            webview_conn: (webview_tx, _),
            doc_sender,
            ..
        } = self;

        // Shared resource
        let span_interner = SpanInterner::new();
        let (shutdown_data_plane_tx, shutdown_data_plane_rx) = mpsc::channel(1);

        // Spawns the editor actor
        let editor_actor = EditorActor::new(
            server,
            editor_rx,
            conn,
            renderer_mailbox.0.clone(),
            webview_tx.clone(),
            span_interner.clone(),
        );

        #[cfg(not(feature = "web"))]
        let control_plane_handle = {
            log::info!("Previewer: editor actor spawned");
            Some(tokio::spawn(editor_actor.run()))
        };
        #[cfg(feature = "web")]
        let control_plane_handle = {
            log::info!("Previewer: editor actor spawned (web mode)");
            None
        };

        // Delayed data plane binding
        let data_plane = DataPlane {
            span_interner: span_interner.clone(),
            webview_tx: webview_tx.clone(),
            editor_tx: editor_tx.clone(),
            invert_colors: config.invert_colors,
            renderer_tx: renderer_mailbox.0.clone(),
            enable_partial_rendering: config.enable_partial_rendering,
            doc_sender,
        };

        Previewer {
            control_plane_handle,
            data_plane_handle: None,
            data_plane_resources: Some((data_plane, shutdown_tx, shutdown_data_plane_rx)),
            stop: Some(Box::new(move || {
                Box::pin(async move {
                    let _ = shutdown_data_plane_tx.send(()).await;
                    let _ = editor_tx.send(EditorActorRequest::Shutdown);
                })
            })),
            #[cfg(feature = "web")]
            editor_actor: Some(editor_actor),
            #[cfg(feature = "web")]
            render_actor: Arc::new(SyncMutex::new(None)),
            #[cfg(feature = "web")]
            webview_actor: Arc::new(SyncMutex::new(None)),
            #[cfg(feature = "web")]
            outline_render_actor: Arc::new(SyncMutex::new(None)),
        }
    }
}

#[derive(Debug)]
pub enum WsMessage {
    /// A text WebSocket message
    Text(String),
    /// A binary WebSocket message
    Binary(Vec<u8>),
}

pub type SourceLocation = reflexo_typst::debug_loc::SourceLocation;

pub enum Location {
    Src(SourceLocation),
}

trait EditorServerDyn: Send + Sync + 'static {
    fn update_memory_files(
        &self,
        files: MemoryFiles,
        reset_shadow: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + '_>>;
    fn remove_memory_files(
        &self,
        files: MemoryFilesShort,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + '_>>;
}

pub trait EditorServer: Send + Sync + 'static {
    fn update_memory_files(
        &self,
        _files: MemoryFiles,
        _reset_shadow: bool,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        async { Ok(()) }
    }

    fn remove_memory_files(
        &self,
        _files: MemoryFilesShort,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        async { Ok(()) }
    }
}

impl<T: EditorServer> EditorServerDyn for T {
    fn update_memory_files(
        &self,
        files: MemoryFiles,
        reset_shadow: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + '_>> {
        Box::pin(EditorServer::update_memory_files(self, files, reset_shadow))
    }
    fn remove_memory_files(
        &self,
        files: MemoryFilesShort,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + '_>> {
        Box::pin(EditorServer::remove_memory_files(self, files))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DocToSrcJumpInfo {
    pub filepath: String,
    pub start: Option<(usize, usize)>, // row, column
    pub end: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangeCursorPositionRequest {
    filepath: PathBuf,
    line: u32,
    /// fixme: character is 0-based, UTF-16 code unit.
    /// We treat it as UTF-8 now.
    character: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveSourceLocRequest {
    pub filepath: PathBuf,
    pub line: u32,
    /// fixme: character is 0-based, UTF-16 code unit.
    /// We treat it as UTF-8 now.
    pub character: u32,
}

impl ResolveSourceLocRequest {
    pub fn to_byte_offset(&self, src: &typst::syntax::Source) -> Option<usize> {
        src.line_column_to_byte(self.line as usize, self.character as usize)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryFiles {
    pub files: HashMap<PathBuf, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryFilesShort {
    pub files: Vec<PathBuf>,
    // mtime: Option<u64>,
}

pub trait CompileView: Send + Sync {
    /// Get the compiled document.
    fn doc(&self) -> Option<TypstDocument>;
    /// Get the compile status.
    fn status(&self) -> CompileStatus;

    /// Check if the view is by OnSaved event.
    fn is_on_saved(&self) -> bool;
    /// Check if the view is by entry update.
    fn is_by_entry_update(&self) -> bool;

    /// Resolve the source span offset.
    fn resolve_source_span(&self, _by: Location) -> Option<SourceSpanOffset> {
        None
    }

    /// Resolve a physical location in the document.
    fn resolve_frame_loc(
        &self,
        _pos: &DocumentPosition,
    ) -> Option<(SourceSpanOffset, SourceSpanOffset)> {
        None
    }

    /// Resolve the document position.
    fn resolve_document_position(&self, _by: Location) -> Vec<Position> {
        vec![]
    }

    /// Resolve the span with an optional offset.
    fn resolve_span(&self, _s: Span, _offset: Option<usize>) -> Option<DocToSrcJumpInfo> {
        None
    }
}

pub struct CompileWatcher {
    task_id: String,
    when: TaskWhen,
    doc_sender: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
    editor_tx: mpsc::UnboundedSender<EditorActorRequest>,
    render_tx: broadcast::Sender<RenderActorRequest>,
}

impl CompileWatcher {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn status(&self, status: CompileStatus) {
        let _ = self
            .editor_tx
            .send(EditorActorRequest::CompileStatus(status));
    }

    pub fn notify_compile(&self, view: Arc<dyn CompileView>) {
        log::info!(
            "Preview({:?}): received notification: signal({:?}, {:?}), when {:?}",
            self.task_id,
            view.is_by_entry_update(),
            view.is_on_saved(),
            self.when
        );
        if !view.is_by_entry_update()
            && (matches!(self.when, TaskWhen::OnSave) && !view.is_on_saved())
        {
            return;
        }

        let status = view.status();
        match status {
            CompileStatus::CompileSuccess => {
                // it is ok to ignore the error here
                log::info!(
                    "CompileWatcher id {}: Compile succeed! Writing to doc_sender",
                    self.task_id,
                );

                *self.doc_sender.write() = Some(view);

                // todo: is it right that ignore zero broadcast receiver?
                let _ = self.render_tx.send(RenderActorRequest::RenderIncremental);
                let _ = self.editor_tx.send(EditorActorRequest::CompileStatus(
                    CompileStatus::CompileSuccess,
                ));
            }
            CompileStatus::Compiling | CompileStatus::CompileError => {
                let _ = self
                    .editor_tx
                    .send(EditorActorRequest::CompileStatus(status));
            }
        }
    }
}

#[derive(Clone)]
struct DataPlane {
    span_interner: SpanInterner,
    webview_tx: broadcast::Sender<WebviewActorRequest>,
    editor_tx: mpsc::UnboundedSender<EditorActorRequest>,
    enable_partial_rendering: bool,
    invert_colors: String,
    renderer_tx: broadcast::Sender<RenderActorRequest>,
    doc_sender: Arc<parking_lot::RwLock<Option<Arc<dyn CompileView>>>>,
}

/// The invert colors for the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewInvertColors {
    /// Inverts all elements.
    Enum(PreviewInvertColor),
    /// Inverts colors per element kinds.
    Object(PreviewInvertColorObject),
}

impl Default for PreviewInvertColors {
    fn default() -> Self {
        PreviewInvertColors::Enum(PreviewInvertColor::Never)
    }
}

impl serde::Serialize for PreviewInvertColors {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            PreviewInvertColors::Enum(color) => color.serialize(serializer),
            PreviewInvertColors::Object(obj) => obj.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for PreviewInvertColors {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PreviewInvertColors;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string or an object with image and rest fields")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(PreviewInvertColors::Enum(Deserialize::deserialize(
                    serde::de::value::StrDeserializer::new(v),
                )?))
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                Ok(PreviewInvertColors::Object(Deserialize::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// The ways of inverting colors in the preview.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PreviewInvertColor {
    /// Never inverts colors.
    #[default]
    Never,
    /// Inverts colors automatically based on the color theme.
    Auto,
    /// Always inverts colors.
    Always,
}

/// The invert colors for the preview, which can be applied to images and other
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewInvertColorObject {
    /// The invert color mode about images.
    #[serde(default)]
    pub image: PreviewInvertColor,
    /// The invert color mode about rest elements.
    #[serde(default)]
    pub rest: PreviewInvertColor,
}

// todo: move me to tinymist-std
#[cfg(not(target_arch = "wasm32"))]
/// tokio::spawn wrapper
pub fn spawn_cpu<F>(func: F) -> tokio::task::JoinHandle<<F as futures::Future>::Output>
where
    F: Future + Send + 'static,
    <F as futures::Future>::Output: std::marker::Send,
{
    tokio::spawn(func)
}

#[cfg(target_arch = "wasm32")]
/// tokio::spawn wrapper - runs on main thread blocking
pub fn spawn_cpu<F>(func: F)
where
    F: Future + 'static,
{
    wasm_bindgen_futures::spawn_local(async move {
        let _ = func.await;
    });
}
