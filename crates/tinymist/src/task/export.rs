//! The actor that handles various document export, like PDF and SVG export.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};

use reflexo::ImmutPath;
use reflexo_typst::{Bytes, CompilationTask, ExportComputation};
use tinymist_project::LspWorld;
use tinymist_std::error::prelude::*;
use tinymist_std::fs::paths::write_atomic;
use tinymist_std::path::PathClean;
use tinymist_std::typst::TypstDocument;
use tinymist_task::{get_page_selection, ExportMarkdownTask, ExportTarget, PdfExport, TextExport};
use tokio::sync::mpsc;
use typlite::{Format, Typlite};
use typst::foundations::IntoValue;
use typst::visualize::Color;

use super::{FutureFolder, SyncTaskFactory};
use crate::project::{
    ApplyProjectTask, CompiledArtifact, DevEvent, DevExportEvent, EntryReader, ExportHtmlTask,
    ExportPdfTask, ExportPngTask, ExportSvgTask, ExportTask as ProjectExportTask, ExportTeXTask,
    ExportTextTask, LspCompiledArtifact, ProjectClient, ProjectTask, QueryTask, TaskWhen,
};
use crate::{actor::editor::EditorRequest, tool::word_count};

#[derive(Clone)]
pub struct ExportTask {
    pub handle: tokio::runtime::Handle,
    pub editor_tx: Option<mpsc::UnboundedSender<EditorRequest>>,
    pub factory: SyncTaskFactory<ExportUserConfig>,
    export_folder: FutureFolder,
    count_word_folder: FutureFolder,
}

impl ExportTask {
    pub fn new(
        handle: tokio::runtime::Handle,
        editor_tx: Option<mpsc::UnboundedSender<EditorRequest>>,
        export_config: ExportUserConfig,
    ) -> Self {
        Self {
            handle,
            editor_tx,
            factory: SyncTaskFactory::new(export_config),
            export_folder: FutureFolder::default(),
            count_word_folder: FutureFolder::default(),
        }
    }

    pub fn change_config(&self, config: ExportUserConfig) {
        self.factory.mutate(|data| *data = config);
    }

    pub(crate) fn signal(
        &self,
        snap: &LspCompiledArtifact,
        client: &std::sync::Arc<(dyn ProjectClient + 'static)>,
    ) {
        let config = self.factory.task();

        self.signal_export(snap, &config, client);
        self.signal_count_word(snap, &config);
    }

    fn signal_export(
        &self,
        artifact: &LspCompiledArtifact,
        config: &Arc<ExportUserConfig>,
        client: &std::sync::Arc<(dyn ProjectClient + 'static)>,
    ) -> Option<()> {
        let doc = artifact.doc.as_ref()?;
        let s = artifact.snap.signal;

        let when = config.task.when().unwrap_or(&TaskWhen::Never);
        let need_export = match when {
            TaskWhen::Never => false,
            TaskWhen::Script => s.by_entry_update,
            TaskWhen::OnType => s.by_mem_events,
            TaskWhen::OnSave => s.by_fs_events,
            TaskWhen::OnDocumentHasTitle => s.by_fs_events && doc.info().title.is_some(),
        };

        let export_hook = config.development.then_some({
            let client = client.clone();

            let event = DevEvent::Export(DevExportEvent {
                id: artifact.id().to_string(),
                when: when.clone(),
                need_export,
                signal: s,
                path: config
                    .task
                    .as_export()
                    .and_then(|t| t.output.clone())
                    .map(|p| p.to_string()),
            });

            move || client.dev_event(event)
        });

        if !need_export {
            if let Some(f) = export_hook {
                f()
            }
            return None;
        }
        log::info!(
            "ExportTask(when={when:?}): export for {} with signal: {s:?}",
            artifact.id()
        );
        let rev = artifact.world().revision().get();
        let fut = self.export_folder.spawn(rev, || {
            let task = config.task.clone();
            let artifact = artifact.clone();
            Box::pin(async move {
                log_err(Self::do_export(task, artifact, None).await);
                if let Some(f) = export_hook {
                    f()
                }
                Some(())
            })
        })?;

        self.handle.spawn(fut);

        Some(())
    }

    fn signal_count_word(
        &self,
        artifact: &LspCompiledArtifact,
        config: &Arc<ExportUserConfig>,
    ) -> Option<()> {
        if !config.count_words {
            return None;
        }

        let editor_tx = self.editor_tx.clone()?;
        let rev = artifact.world().revision().get();
        let fut = self.count_word_folder.spawn(rev, || {
            let artifact = artifact.clone();
            Box::pin(async move {
                let id = artifact.id().clone();
                let doc = artifact.doc?;
                let wc =
                    log_err(FutureFolder::compute(move |_| word_count::word_count(&doc)).await);
                log::debug!("WordCount({id:?}:{rev}): {wc:?}");

                if let Some(wc) = wc {
                    let _ = editor_tx.send(EditorRequest::WordCount(id, wc));
                }

                Some(())
            })
        })?;

        self.handle.spawn(fut);

        Some(())
    }

    pub async fn do_export(
        task: ProjectTask,
        artifact: LspCompiledArtifact,
        lock_dir: Option<ImmutPath>,
    ) -> Result<Option<PathBuf>> {
        use reflexo_vec2svg::DefaultExportFeature;
        use ProjectTask::*;

        let CompiledArtifact { graph, doc, .. } = artifact;

        // Prepare the output path.
        let entry = graph.snap.world.entry_state();
        let config = task.as_export().unwrap();
        let output = config.output.clone().unwrap_or_default();
        let Some(write_to) = output.substitute(&entry) else {
            return Ok(None);
        };
        let write_to = if write_to.is_relative() {
            let cwd = std::env::current_dir().context("failed to get current directory")?;
            cwd.join(write_to).clean()
        } else {
            write_to.to_path_buf()
        };
        if write_to.is_relative() {
            bail!("ExportTask({task:?}): output path is relative: {write_to:?}");
        }
        if write_to.is_dir() {
            bail!("ExportTask({task:?}): output path is a directory: {write_to:?}");
        }
        let write_to = write_to.with_extension(task.extension());

        static EXPORT_ID: AtomicUsize = AtomicUsize::new(0);
        let export_id = EXPORT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        log::debug!("ExportTask({export_id}): exporting {entry:?} to {write_to:?}");
        if let Some(e) = write_to.parent() {
            if !e.exists() {
                std::fs::create_dir_all(e).context("failed to create directory")?;
            }
        }

        let _: Option<()> = lock_dir.and_then(|lock_dir| {
            let mut updater = crate::project::update_lock(lock_dir);

            let doc_id = updater.compiled(graph.world())?;

            updater.task(ApplyProjectTask {
                id: doc_id.clone(),
                document: doc_id,
                task: task.clone(),
            });
            updater.commit();

            Some(())
        });

        // Prepare the document.
        let doc = doc.context("cannot export with compilation errors")?;

        // Prepare data.
        let kind2 = task.clone();
        let data = FutureFolder::compute(move |_| -> Result<Bytes> {
            let doc = &doc;

            // static BLANK: Lazy<Page> = Lazy::new(Page::default);
            // todo: check warnings and errors inside
            let html_once = OnceLock::new();
            let html_doc = || -> Result<_> {
                html_once
                    .get_or_init(|| -> Result<_> {
                        Ok(match &doc {
                            TypstDocument::Html(html_doc) => html_doc.clone(),
                            TypstDocument::Paged(_) => extra_compile_for_export(graph.world())?,
                        })
                    })
                    .as_ref()
                    .map_err(|e| e.clone())
            };
            let page_once = OnceLock::new();
            let paged_doc = || {
                page_once
                    .get_or_init(|| -> Result<_> {
                        Ok(match &doc {
                            TypstDocument::Paged(paged_doc) => paged_doc.clone(),
                            TypstDocument::Html(_) => extra_compile_for_export(graph.world())?,
                        })
                    })
                    .as_ref()
                    .map_err(|e| e.clone())
            };
            let first_page = || {
                paged_doc()?
                    .pages
                    .first()
                    .context("no first page to export")
            };
            Ok(match kind2 {
                Preview(..) => Bytes::new([]),
                // todo: more pdf flags
                ExportPdf(config) => PdfExport::run(&graph, paged_doc()?, &config)?,
                Query(QueryTask {
                    export: _,
                    output_extension: _,
                    format,
                    selector,
                    field,
                    one,
                }) => {
                    let pretty = false;
                    let elements = reflexo_typst::query::retrieve(&graph.world(), &selector, doc)
                        .map_err(|e| anyhow::anyhow!("failed to retrieve: {e}"))?;
                    if one && elements.len() != 1 {
                        bail!("expected exactly one element, found {}", elements.len());
                    }

                    let mapped: Vec<_> = elements
                        .into_iter()
                        .filter_map(|c| match &field {
                            Some(field) => c.get_by_name(field).ok(),
                            _ => Some(c.into_value()),
                        })
                        .collect();

                    if one {
                        let Some(value) = mapped.first() else {
                            bail!("no such field found for element");
                        };
                        serialize(value, &format, pretty).map(Bytes::from_string)?
                    } else {
                        serialize(&mapped, &format, pretty).map(Bytes::from_string)?
                    }
                }
                ExportHtml(ExportHtmlTask { export: _ }) => Bytes::from_string(
                    typst_html::html(html_doc()?)
                        .map_err(|e| format!("export error: {e:?}"))
                        .context_ut("failed to export to html")?,
                ),
                ExportSvgHtml(ExportHtmlTask { export: _ }) => Bytes::from_string(
                    reflexo_vec2svg::render_svg_html::<DefaultExportFeature>(paged_doc()?),
                ),
                ExportText(ExportTextTask { export: _ }) => {
                    Bytes::from_string(TextExport::run_on_doc(doc)?)
                }
                ExportMd(ExportMarkdownTask {
                    processor,
                    assets_path,
                    export: _,
                }) => {
                    let conv = Typlite::new(Arc::new(graph.world().clone()))
                        .with_format(Format::Md)
                        .with_feature(typlite::TypliteFeat {
                            processor,
                            assets_path,
                            ..Default::default()
                        })
                        .convert()
                        .map_err(|e| anyhow::anyhow!("failed to convert to markdown: {e}"))?;

                    Bytes::from_string(conv)
                }
                // todo: duplicated code with ExportMd
                ExportTeX(ExportTeXTask {
                    processor,
                    assets_path,
                    export: _,
                }) => {
                    log::info!("ExportTask({export_id}): exporting to TeX with processor {processor:?} and assets path {assets_path:?}");
                    let conv = Typlite::new(Arc::new(graph.world().clone()))
                        .with_format(Format::LaTeX)
                        .with_feature(typlite::TypliteFeat {
                            processor,
                            assets_path,
                            ..Default::default()
                        })
                        .convert()
                        .map_err(|e| anyhow::anyhow!("failed to convert to latex: {e}"))?;

                    Bytes::from_string(conv)
                }
                ExportSvg(ExportSvgTask { export }) => {
                    let (is_first, merged_gap) = get_page_selection(&export)?;

                    Bytes::from_string(if is_first {
                        typst_svg::svg(first_page()?)
                    } else {
                        typst_svg::svg_merged(paged_doc()?, merged_gap)
                    })
                }
                ExportPng(ExportPngTask { export, ppi, fill }) => {
                    let ppi = ppi.to_f32();
                    if ppi <= 1e-6 {
                        bail!("invalid ppi: {ppi}");
                    }

                    let fill = if let Some(fill) = fill {
                        parse_color(fill).map_err(|err| anyhow::anyhow!("invalid fill ({err})"))?
                    } else {
                        Color::WHITE
                    };

                    let (is_first, merged_gap) = get_page_selection(&export)?;

                    let pixmap = if is_first {
                        typst_render::render(first_page()?, ppi / 72.)
                    } else {
                        typst_render::render_merged(paged_doc()?, ppi / 72., merged_gap, Some(fill))
                    };

                    Bytes::new(
                        pixmap
                            .encode_png()
                            .map_err(|err| anyhow::anyhow!("failed to encode PNG ({err})"))?,
                    )
                }
            })
        })
        .await??;

        let to = write_to.clone();
        tokio::task::spawn_blocking(move || write_atomic(to, data))
            .await
            .context_ut("failed to export")??;

        log::debug!("ExportTask({export_id}): export complete");
        Ok(Some(write_to))
    }
}

/// User configuration for export.
#[derive(Clone, PartialEq, Eq)]
pub struct ExportUserConfig {
    /// Tinymist's default export target.
    pub export_target: ExportTarget,
    pub task: ProjectTask,
    pub count_words: bool,
    /// Whether to run the server in development mode.
    pub development: bool,
}

impl Default for ExportUserConfig {
    fn default() -> Self {
        Self {
            export_target: ExportTarget::default(),
            task: ProjectTask::ExportPdf(ExportPdfTask {
                export: ProjectExportTask {
                    when: TaskWhen::Never,
                    output: None,
                    transform: vec![],
                },
                pdf_standards: vec![],
                creation_timestamp: None,
            }),
            count_words: false,
            development: false,
        }
    }
}

fn parse_color(fill: String) -> Result<Color> {
    match fill.as_str() {
        "black" => Ok(Color::BLACK),
        "white" => Ok(Color::WHITE),
        "red" => Ok(Color::RED),
        "green" => Ok(Color::GREEN),
        "blue" => Ok(Color::BLUE),
        hex if hex.starts_with('#') => {
            Color::from_str(&hex[1..]).context_ut("failed to parse color")
        }
        _ => bail!("invalid color: {fill}"),
    }
}

fn log_err<T>(artifact: Result<T>) -> Option<T> {
    match artifact {
        Ok(v) => Some(v),
        Err(err) => {
            log::error!("{err}");
            None
        }
    }
}

fn extra_compile_for_export<D: typst::Document + Send + Sync + 'static>(
    world: &LspWorld,
) -> Result<Arc<D>> {
    let res = tokio::task::block_in_place(|| CompilationTask::<D>::execute(world));

    match res.output {
        Ok(v) => Ok(v),
        Err(e) if e.is_empty() => bail!("failed to compile: internal error"),
        Err(e) => bail!("failed to compile: {}", e[0].message),
    }
}

/// Serialize data to the output format.
fn serialize(data: &impl serde::Serialize, format: &str, pretty: bool) -> Result<String> {
    Ok(match format {
        "json" if pretty => serde_json::to_string_pretty(data).context("serialize to json")?,
        "json" => serde_json::to_string(data).context("serialize to json")?,
        "yaml" => serde_yaml::to_string(&data).context_ut("serialize to yaml")?,
        "txt" => {
            use serde_json::Value::*;
            let value = serde_json::to_value(data).context("serialize to json value")?;
            match value {
                String(s) => s,
                _ => {
                    let kind = match value {
                        Null => "null",
                        Bool(_) => "boolean",
                        Number(_) => "number",
                        String(_) => "string",
                        Array(_) => "array",
                        Object(_) => "object",
                    };
                    bail!("expected a string value for format: {format}, got {kind}")
                }
            }
        }
        _ => bail!("unsupported format for query: {format}"),
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::export::ProjectCompilation;
    use crate::project::{CompileOnceArgs, CompileSignal};
    use crate::world::base::{CompileSnapshot, WorldComputeGraph};

    #[test]
    fn test_default_never() {
        let conf = ExportUserConfig::default();
        assert!(!conf.count_words);
        assert_eq!(conf.task.when(), Some(&TaskWhen::Never));
    }

    #[test]
    fn test_parse_color() {
        assert_eq!(parse_color("black".to_owned()).unwrap(), Color::BLACK);
        assert_eq!(parse_color("white".to_owned()).unwrap(), Color::WHITE);
        assert_eq!(parse_color("red".to_owned()).unwrap(), Color::RED);
        assert_eq!(parse_color("green".to_owned()).unwrap(), Color::GREEN);
        assert_eq!(parse_color("blue".to_owned()).unwrap(), Color::BLUE);
        assert_eq!(
            parse_color("#000000".to_owned()).unwrap().to_hex(),
            "#000000"
        );
        assert_eq!(
            parse_color("#ffffff".to_owned()).unwrap().to_hex(),
            "#ffffff"
        );
        assert_eq!(
            parse_color("#000000cc".to_owned()).unwrap().to_hex(),
            "#000000cc"
        );
        assert!(parse_color("invalid".to_owned()).is_err());
    }

    #[test]
    fn compilation_default_never() {
        let args = CompileOnceArgs::parse_from(["tinymist", "main.typ"]);
        let verse = args
            .resolve_system()
            .expect("failed to resolve system universe");

        let snap = CompileSnapshot::from_world(verse.snapshot());

        let graph = WorldComputeGraph::new(snap);

        let needs_run =
            ProjectCompilation::preconfig_timings(&graph).expect("failed to preconfigure timings");

        assert!(!needs_run);
    }

    // todo: on demand compilation
    #[test]
    fn compilation_run_paged_diagnostics() {
        let args = CompileOnceArgs::parse_from(["tinymist", "main.typ"]);
        let verse = args
            .resolve_system()
            .expect("failed to resolve system universe");

        let mut snap = CompileSnapshot::from_world(verse.snapshot());

        snap.signal = CompileSignal {
            by_entry_update: true,
            by_fs_events: false,
            by_mem_events: false,
        };

        let graph = WorldComputeGraph::new(snap);

        let needs_run =
            ProjectCompilation::preconfig_timings(&graph).expect("failed to preconfigure timings");

        assert!(needs_run);
    }

    use chrono::{DateTime, Utc};
    use tinymist_std::time::*;

    /// Parses a UNIX timestamp according to <https://reproducible-builds.org/specs/source-date-epoch/>
    pub fn convert_source_date_epoch(seconds: i64) -> Result<DateTime<Utc>, String> {
        DateTime::from_timestamp(seconds, 0).ok_or_else(|| "timestamp out of range".to_string())
    }

    /// Parses a UNIX timestamp according to <https://reproducible-builds.org/specs/source-date-epoch/>
    pub fn convert_system_time(seconds: i64) -> Result<Time, String> {
        if seconds < 0 {
            return Err("negative timestamp since unix epoch".to_string());
        }

        Time::UNIX_EPOCH
            .checked_add(Duration::new(seconds as u64, 0))
            .ok_or_else(|| "timestamp out of range".to_string())
    }

    #[test]
    fn test_timestamp_chrono() {
        let timestamp = 1_000_000_000;
        let date_time = convert_source_date_epoch(timestamp).unwrap();
        assert_eq!(date_time.timestamp(), timestamp);
    }

    #[test]
    fn test_timestamp_system() {
        let timestamp = 1_000_000_000;
        let date_time = convert_system_time(timestamp).unwrap();
        assert_eq!(
            date_time
                .duration_since(Time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            timestamp as u64
        );
    }

    use typst::foundations::Datetime as TypstDatetime;

    fn convert_datetime_chrono(date_time: DateTime<Utc>) -> Option<TypstDatetime> {
        use chrono::{Datelike, Timelike};
        TypstDatetime::from_ymd_hms(
            date_time.year(),
            date_time.month().try_into().ok()?,
            date_time.day().try_into().ok()?,
            date_time.hour().try_into().ok()?,
            date_time.minute().try_into().ok()?,
            date_time.second().try_into().ok()?,
        )
    }

    #[test]
    fn test_timestamp_pdf() {
        let timestamp = 1_000_000_000;
        let date_time = convert_source_date_epoch(timestamp).unwrap();
        assert_eq!(date_time.timestamp(), timestamp);
        let chrono_pdf_ts = convert_datetime_chrono(date_time).unwrap();

        let timestamp = 1_000_000_000;
        let date_time = convert_system_time(timestamp).unwrap();
        let system_pdf_ts = tinymist_std::time::to_typst_time(date_time.into());
        assert_eq!(chrono_pdf_ts, system_pdf_ts);
    }
}
