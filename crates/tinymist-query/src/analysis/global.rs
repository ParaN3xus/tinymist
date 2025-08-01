use std::num::NonZeroUsize;
use std::ops::DerefMut;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::{collections::HashSet, ops::Deref};

use comemo::{Track, Tracked};
use lsp_types::Url;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use tinymist_analysis::docs::DocString;
use tinymist_analysis::stats::AllocStats;
use tinymist_analysis::syntax::classify_def_loosely;
use tinymist_analysis::ty::term_value;
use tinymist_analysis::{analyze_expr_, analyze_import_};
use tinymist_lint::LintInfo;
use tinymist_project::{LspComputeGraph, LspWorld, TaskWhen};
use tinymist_std::hash::{hash128, FxDashMap};
use tinymist_std::typst::TypstDocument;
use tinymist_world::debug_loc::DataSource;
use tinymist_world::vfs::{PathResolution, WorkspaceResolver};
use tinymist_world::{EntryReader, DETACHED_ENTRY};
use typst::diag::{
    eco_format, At, FileError, FileResult, SourceDiagnostic, SourceResult, StrResult,
};
use typst::foundations::{Bytes, IntoValue, Module, StyleChain, Styles};
use typst::introspection::Introspector;
use typst::layout::Position;
use typst::model::BibliographyElem;
use typst::syntax::package::{PackageManifest, PackageSpec};
use typst::syntax::{Span, VirtualPath};
use typst_shim::eval::{eval_compat, Eval};

use super::{LspQuerySnapshot, TypeEnv};
use crate::adt::revision::{RevisionLock, RevisionManager, RevisionManagerLike, RevisionSlot};
use crate::analysis::prelude::*;
use crate::analysis::{
    analyze_signature, bib_info, definition, post_type_check, AnalysisStats, BibInfo,
    CompletionFeat, Definition, PathPreference, QueryStatGuard, SemanticTokenCache,
    SemanticTokenContext, SemanticTokens, Signature, SignatureTarget, Ty, TypeInfo,
};
use crate::docs::{DefDocs, TidyModuleDocs};
use crate::syntax::{
    classify_syntax, construct_module_dependencies, is_mark, resolve_id_by_path,
    scan_workspace_files, Decl, DefKind, ExprInfo, ExprRoute, LexicalScope, ModuleDependency,
    SyntaxClass,
};
use crate::upstream::{tooltip_, Tooltip};
use crate::{
    ColorTheme, CompilerQueryRequest, LspPosition, LspRange, LspWorldExt, PositionEncoding,
};

macro_rules! interned_str {
    ($name:ident, $value:expr) => {
        static $name: LazyLock<Interned<str>> = LazyLock::new(|| $value.into());
    };
}

/// The analysis data holds globally.
#[derive(Default, Clone)]
pub struct Analysis {
    /// The position encoding for the workspace.
    pub position_encoding: PositionEncoding,
    /// Whether to allow overlapping semantic tokens.
    pub allow_overlapping_token: bool,
    /// Whether to allow multiline semantic tokens.
    pub allow_multiline_token: bool,
    /// Whether to remove html from markup content in responses.
    pub remove_html: bool,
    /// Whether to utilize the extended `tinymist.resolveCodeAction` at client
    /// side.
    ///
    /// The extended feature by `tinymist.resolveCodeAction`:
    /// - supports Snippet edit.
    ///
    /// The example implementation can be found in the VS Code extension.
    pub extended_code_action: bool,
    /// Tinymist's completion features.
    pub completion_feat: CompletionFeat,
    /// The editor's color theme.
    pub color_theme: ColorTheme,
    /// When to trigger the lint.
    pub lint: TaskWhen,
    /// The periscope provider.
    pub periscope: Option<Arc<dyn PeriscopeProvider + Send + Sync>>,
    /// The global worker resources for analysis.
    pub workers: Arc<AnalysisGlobalWorkers>,
    /// The semantic token cache.
    pub tokens_caches: Arc<Mutex<SemanticTokenCache>>,
    /// The global caches for analysis.
    pub caches: AnalysisGlobalCaches,
    /// The revision-managed cache for analysis.
    pub analysis_rev_cache: Arc<Mutex<AnalysisRevCache>>,
    /// The statistics about the analyzers.
    pub stats: Arc<AnalysisStats>,
}

impl Analysis {
    /// Enters the analysis context.
    pub fn enter(&self, world: LspWorld) -> LocalContextGuard {
        self.enter_(world, self.lock_revision(None))
    }

    /// Enters the analysis context.
    pub(crate) fn enter_(&self, world: LspWorld, mut lg: AnalysisRevLock) -> LocalContextGuard {
        let lifetime = self.caches.lifetime.fetch_add(1, Ordering::SeqCst);
        let slot = self
            .analysis_rev_cache
            .lock()
            .find_revision(world.revision(), &lg);
        let tokens = lg.tokens.take();
        LocalContextGuard {
            _rev_lock: lg,
            local: LocalContext {
                tokens,
                caches: AnalysisLocalCaches::default(),
                shared: Arc::new(SharedContext {
                    slot,
                    lifetime,
                    world,
                    analysis: self.clone(),
                }),
            },
        }
    }

    /// Gets a snapshot for language queries.
    pub fn query_snapshot(
        self: Arc<Self>,
        snap: LspComputeGraph,
        req: Option<&CompilerQueryRequest>,
    ) -> LspQuerySnapshot {
        let rev_lock = self.lock_revision(req);
        LspQuerySnapshot {
            snap,
            analysis: self,
            rev_lock,
        }
    }

    /// Locks the revision in *main thread*.
    #[must_use]
    pub fn lock_revision(&self, req: Option<&CompilerQueryRequest>) -> AnalysisRevLock {
        let mut grid = self.analysis_rev_cache.lock();

        AnalysisRevLock {
            tokens: match req {
                Some(CompilerQueryRequest::SemanticTokensFull(req)) => Some(
                    SemanticTokenCache::acquire(self.tokens_caches.clone(), &req.path, None),
                ),
                Some(CompilerQueryRequest::SemanticTokensDelta(req)) => {
                    Some(SemanticTokenCache::acquire(
                        self.tokens_caches.clone(),
                        &req.path,
                        Some(&req.previous_result_id),
                    ))
                }
                _ => None,
            },
            inner: grid.manager.lock_estimated(),
            grid: self.analysis_rev_cache.clone(),
        }
    }

    /// Clear all cached resources.
    pub fn clear_cache(&self) {
        self.caches.signatures.clear();
        self.caches.docstrings.clear();
        self.caches.def_signatures.clear();
        self.caches.static_signatures.clear();
        self.caches.terms.clear();
        self.tokens_caches.lock().clear();
        self.analysis_rev_cache.lock().clear();
    }

    /// Report the statistics of the analysis.
    pub fn report_query_stats(&self) -> String {
        self.stats.report()
    }

    /// Report the statistics of the allocation.
    pub fn report_alloc_stats(&self) -> String {
        AllocStats::report()
    }

    /// Get configured trigger suggest command.
    pub fn trigger_suggest(&self, context: bool) -> Option<Interned<str>> {
        interned_str!(INTERNED, "editor.action.triggerSuggest");

        (self.completion_feat.trigger_suggest && context).then(|| INTERNED.clone())
    }

    /// Get configured trigger parameter hints command.
    pub fn trigger_parameter_hints(&self, context: bool) -> Option<Interned<str>> {
        interned_str!(INTERNED, "editor.action.triggerParameterHints");
        (self.completion_feat.trigger_parameter_hints && context).then(|| INTERNED.clone())
    }

    /// Get configured trigger suggest after snippet command.
    ///
    /// > VS Code doesn't do that... Auto triggering suggestion only happens on
    /// > typing (word starts or trigger characters). However, you can use
    /// > editor.action.triggerSuggest as command on a suggestion to "manually"
    /// > retrigger suggest after inserting one
    pub fn trigger_on_snippet(&self, context: bool) -> Option<Interned<str>> {
        if !self.completion_feat.trigger_on_snippet_placeholders {
            return None;
        }

        self.trigger_suggest(context)
    }

    /// Get configured trigger on positional parameter hints command.
    pub fn trigger_on_snippet_with_param_hint(&self, context: bool) -> Option<Interned<str>> {
        interned_str!(INTERNED, "tinymist.triggerSuggestAndParameterHints");
        if !self.completion_feat.trigger_on_snippet_placeholders {
            return self.trigger_parameter_hints(context);
        }

        (self.completion_feat.trigger_suggest_and_parameter_hints && context)
            .then(|| INTERNED.clone())
    }
}

/// The periscope provider.
pub trait PeriscopeProvider {
    /// Resolve telescope image at the given position.
    fn periscope_at(
        &self,
        _ctx: &mut LocalContext,
        _doc: &TypstDocument,
        _pos: Position,
    ) -> Option<String> {
        None
    }
}

/// The local context guard that performs gc once dropped.
pub struct LocalContextGuard {
    /// The guarded local context
    pub local: LocalContext,
    /// The revision lock
    _rev_lock: AnalysisRevLock,
}

impl Deref for LocalContextGuard {
    type Target = LocalContext;

    fn deref(&self) -> &Self::Target {
        &self.local
    }
}

impl DerefMut for LocalContextGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.local
    }
}

// todo: gc in new thread
impl Drop for LocalContextGuard {
    fn drop(&mut self) {
        self.gc();
    }
}

impl LocalContextGuard {
    fn gc(&self) {
        let lifetime = self.lifetime;
        loop {
            let latest_clear_lifetime = self.analysis.caches.clear_lifetime.load(Ordering::Relaxed);
            if latest_clear_lifetime >= lifetime {
                return;
            }

            if self.analysis.caches.clear_lifetime.compare_exchange(
                latest_clear_lifetime,
                lifetime,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) != Ok(latest_clear_lifetime)
            {
                continue;
            }

            break;
        }

        let retainer = |l: u64| lifetime.saturating_sub(l) < 60;
        let caches = &self.analysis.caches;
        caches.def_signatures.retain(|(l, _)| retainer(*l));
        caches.static_signatures.retain(|(l, _)| retainer(*l));
        caches.terms.retain(|(l, _)| retainer(*l));
        caches.signatures.retain(|(l, _)| retainer(*l));
        caches.docstrings.retain(|(l, _)| retainer(*l));
    }
}

/// The local context for analyzers. In addition to the shared context, it also
/// holds mutable local caches.
pub struct LocalContext {
    /// The created semantic token context.
    pub(crate) tokens: Option<SemanticTokenContext>,
    /// Local caches for analysis.
    pub caches: AnalysisLocalCaches,
    /// The shared context
    pub shared: Arc<SharedContext>,
}

impl Deref for LocalContext {
    type Target = Arc<SharedContext>;

    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

impl DerefMut for LocalContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.shared
    }
}

impl LocalContext {
    /// Set list of packages for LSP-based completion.
    #[cfg(test)]
    pub fn test_package_list(&mut self, f: impl FnOnce() -> Vec<(PackageSpec, Option<EcoString>)>) {
        self.world.registry.test_package_list(f);
    }

    /// Set the files for LSP-based completion.
    #[cfg(test)]
    pub fn test_completion_files(&mut self, f: impl FnOnce() -> Vec<TypstFileId>) {
        self.caches.completion_files.get_or_init(f);
    }

    /// Set the files for analysis.
    #[cfg(test)]
    pub fn test_files(&mut self, f: impl FnOnce() -> Vec<TypstFileId>) {
        self.caches.root_files.get_or_init(f);
    }

    /// Get all the source files in the workspace.
    pub(crate) fn completion_files(
        &self,
        pref: &PathPreference,
    ) -> impl Iterator<Item = &TypstFileId> {
        let regexes = pref.ext_matcher();
        self.caches
            .completion_files
            .get_or_init(|| {
                if let Some(root) = self.world.entry_state().workspace_root() {
                    scan_workspace_files(&root, PathPreference::Special.ext_matcher(), |path| {
                        WorkspaceResolver::workspace_file(Some(&root), VirtualPath::new(path))
                    })
                } else {
                    vec![]
                }
            })
            .iter()
            .filter(move |fid| {
                fid.vpath()
                    .as_rooted_path()
                    .extension()
                    .and_then(|path| path.to_str())
                    .is_some_and(|path| regexes.is_match(path))
            })
    }

    /// Get all the source files in the workspace.
    pub fn source_files(&self) -> &Vec<TypstFileId> {
        self.caches.root_files.get_or_init(|| {
            self.completion_files(&PathPreference::Source {
                allow_package: false,
            })
            .copied()
            .collect()
        })
    }

    /// Get the module dependencies of the workspace.
    pub fn module_dependencies(&mut self) -> &HashMap<TypstFileId, ModuleDependency> {
        if self.caches.module_deps.get().is_some() {
            self.caches.module_deps.get().unwrap()
        } else {
            // may cause multiple times to calculate, but it is okay because we have mutable
            // reference to self.
            let deps = construct_module_dependencies(self);
            self.caches.module_deps.get_or_init(|| deps)
        }
    }

    /// Get all depended files in the workspace, inclusively.
    pub fn depended_source_files(&self) -> EcoVec<TypstFileId> {
        let mut ids = self.depended_files();
        let preference = PathPreference::Source {
            allow_package: false,
        };
        ids.retain(|id| preference.is_match(id.vpath().as_rooted_path()));
        ids
    }

    /// Get all depended file ids of a compilation, inclusively.
    /// Note: must be called after compilation.
    pub fn depended_files(&self) -> EcoVec<TypstFileId> {
        self.world.depended_files()
    }

    /// Get the world surface for Typst compiler.
    pub fn world(&self) -> &LspWorld {
        &self.shared.world
    }

    /// Get the shared context.
    pub fn shared(&self) -> &Arc<SharedContext> {
        &self.shared
    }

    /// Get the shared context.
    pub fn shared_(&self) -> Arc<SharedContext> {
        self.shared.clone()
    }

    /// Fork a new context for searching in the workspace.
    pub fn fork_for_search(&mut self) -> SearchCtx {
        SearchCtx {
            ctx: self,
            searched: Default::default(),
            worklist: Default::default(),
        }
    }

    pub(crate) fn preload_package(&self, entry_point: TypstFileId) {
        self.shared_().preload_package(entry_point);
    }

    pub(crate) fn with_vm<T>(&self, f: impl FnOnce(&mut typst_shim::eval::Vm) -> T) -> T {
        crate::upstream::with_vm((self.world() as &dyn World).track(), f)
    }

    pub(crate) fn const_eval(&self, rr: ast::Expr<'_>) -> Option<Value> {
        SharedContext::const_eval(rr)
    }

    pub(crate) fn mini_eval(&self, rr: ast::Expr<'_>) -> Option<Value> {
        self.const_eval(rr)
            .or_else(|| self.with_vm(|vm| rr.eval(vm).ok()))
    }

    pub(crate) fn cached_tokens(&mut self, source: &Source) -> (SemanticTokens, Option<String>) {
        let tokens = crate::analysis::semantic_tokens::get_semantic_tokens(self, source);

        let result_id = self.tokens.as_ref().map(|t| {
            let id = t.next.revision;
            t.next
                .data
                .set(tokens.clone())
                .unwrap_or_else(|_| panic!("unexpected slot overwrite {id}"));
            id.to_string()
        });
        (tokens, result_id)
    }

    /// Get the expression information of a source file.
    pub(crate) fn expr_stage_by_id(&mut self, fid: TypstFileId) -> Option<ExprInfo> {
        Some(self.expr_stage(&self.source_by_id(fid).ok()?))
    }

    /// Get the expression information of a source file.
    pub(crate) fn expr_stage(&mut self, source: &Source) -> ExprInfo {
        let id = source.id();
        let cache = &self.caches.modules.entry(id).or_default().expr_stage;
        cache.get_or_init(|| self.shared.expr_stage(source)).clone()
    }

    /// Get the type check information of a source file.
    pub(crate) fn type_check(&mut self, source: &Source) -> Arc<TypeInfo> {
        let id = source.id();
        let cache = &self.caches.modules.entry(id).or_default().type_check;
        cache.get_or_init(|| self.shared.type_check(source)).clone()
    }

    pub(crate) fn lint(&mut self, source: &Source) -> EcoVec<SourceDiagnostic> {
        self.shared.lint(source).diagnostics
    }

    /// Get the type check information of a source file.
    pub(crate) fn type_check_by_id(&mut self, id: TypstFileId) -> Arc<TypeInfo> {
        let cache = &self.caches.modules.entry(id).or_default().type_check;
        cache
            .clone()
            .get_or_init(|| {
                let source = self.source_by_id(id).ok();
                source
                    .map(|s| self.shared.type_check(&s))
                    .unwrap_or_default()
            })
            .clone()
    }

    pub(crate) fn type_of_span(&mut self, s: Span) -> Option<Ty> {
        let scheme = self.type_check_by_id(s.id()?);
        let ty = scheme.type_of_span(s)?;
        Some(scheme.simplify(ty, false))
    }

    pub(crate) fn def_docs(&mut self, def: &Definition) -> Option<DefDocs> {
        // let plain_docs = sym.head.docs.as_deref();
        // let plain_docs = plain_docs.or(sym.head.oneliner.as_deref());
        match def.decl.kind() {
            DefKind::Function => {
                let sig = self.sig_of_def(def.clone())?;
                let docs = crate::docs::sig_docs(&sig)?;
                Some(DefDocs::Function(Box::new(docs)))
            }
            DefKind::Struct | DefKind::Constant | DefKind::Variable => {
                let docs = crate::docs::var_docs(self, def.decl.span())?;
                Some(DefDocs::Variable(docs))
            }
            DefKind::Module => {
                let ei = self.expr_stage_by_id(def.decl.file_id()?)?;
                Some(DefDocs::Module(TidyModuleDocs {
                    docs: ei.module_docstring.docs.clone().unwrap_or_default(),
                }))
            }
            DefKind::Reference => None,
        }
    }
}

/// The shared analysis context for analyzers.
pub struct SharedContext {
    /// The caches lifetime tick for analysis.
    pub lifetime: u64,
    /// The world surface for Typst compiler.
    pub world: LspWorld,
    /// The analysis data
    pub analysis: Analysis,
    /// The using analysis revision slot
    slot: Arc<RevisionSlot<AnalysisRevSlot>>,
}

impl SharedContext {
    /// Get revision of current analysis
    pub fn revision(&self) -> usize {
        self.slot.revision
    }

    /// Get the position encoding during session.
    pub(crate) fn position_encoding(&self) -> PositionEncoding {
        self.analysis.position_encoding
    }

    /// Convert an LSP position to a Typst position.
    pub fn to_typst_pos(&self, position: LspPosition, src: &Source) -> Option<usize> {
        crate::to_typst_position(position, self.analysis.position_encoding, src)
    }

    /// Converts an LSP position with some offset.
    pub fn to_typst_pos_offset(
        &self,
        source: &Source,
        position: LspPosition,
        shift: usize,
    ) -> Option<usize> {
        let offset = self.to_typst_pos(position, source)?;
        Some(ceil_char_boundary(source.text(), offset + shift))
    }

    /// Convert a Typst offset to an LSP position.
    pub fn to_lsp_pos(&self, typst_offset: usize, src: &Source) -> LspPosition {
        crate::to_lsp_position(typst_offset, self.analysis.position_encoding, src)
    }

    /// Convert an LSP range to a Typst range.
    pub fn to_typst_range(&self, position: LspRange, src: &Source) -> Option<Range<usize>> {
        crate::to_typst_range(position, self.analysis.position_encoding, src)
    }

    /// Convert a Typst range to an LSP range.
    pub fn to_lsp_range(&self, position: Range<usize>, src: &Source) -> LspRange {
        crate::to_lsp_range(position, src, self.analysis.position_encoding)
    }

    /// Convert a Typst range to an LSP range.
    pub fn to_lsp_range_(&self, position: Range<usize>, fid: TypstFileId) -> Option<LspRange> {
        let ext = fid
            .vpath()
            .as_rootless_path()
            .extension()
            .and_then(|ext| ext.to_str());
        // yaml/yml/bib
        if matches!(ext, Some("yaml" | "yml" | "bib")) {
            let bytes = self.file_by_id(fid).ok()?;
            let bytes_len = bytes.len();
            let loc = loc_info(bytes)?;
            // binary search
            let start = find_loc(bytes_len, &loc, position.start, self.position_encoding())?;
            let end = find_loc(bytes_len, &loc, position.end, self.position_encoding())?;
            return Some(LspRange { start, end });
        }

        let source = self.source_by_id(fid).ok()?;

        Some(self.to_lsp_range(position, &source))
    }

    /// Resolve the real path for a file id.
    pub fn path_for_id(&self, id: TypstFileId) -> Result<PathResolution, FileError> {
        self.world.path_for_id(id)
    }

    /// Resolve the uri for a file id.
    pub fn uri_for_id(&self, fid: TypstFileId) -> Result<Url, FileError> {
        self.world.uri_for_id(fid)
    }

    /// Get file's id by its path
    pub fn file_id_by_path(&self, path: &Path) -> FileResult<TypstFileId> {
        self.world.file_id_by_path(path)
    }

    /// Get the content of a file by file id.
    pub fn file_by_id(&self, fid: TypstFileId) -> FileResult<Bytes> {
        self.world.file(fid)
    }

    /// Get the source of a file by file id.
    pub fn source_by_id(&self, fid: TypstFileId) -> FileResult<Source> {
        self.world.source(fid)
    }

    /// Get the source of a file by file path.
    pub fn source_by_path(&self, path: &Path) -> FileResult<Source> {
        self.source_by_id(self.file_id_by_path(path)?)
    }

    /// Classifies the syntax under span that can be operated on by IDE
    /// functionality.
    pub fn classify_span<'s>(&self, source: &'s Source, span: Span) -> Option<SyntaxClass<'s>> {
        let node = LinkedNode::new(source.root()).find(span)?;
        let cursor = node.offset() + 1;
        classify_syntax(node, cursor)
    }

    /// Classifies the syntax under position that can be operated on by IDE
    /// functionality. It is preferred to select a decl if it is at the starts
    /// of some mark.
    pub fn classify_for_decl<'s>(
        &self,
        source: &'s Source,
        position: LspPosition,
    ) -> Option<SyntaxClass<'s>> {
        let cursor = self.to_typst_pos_offset(source, position, 1)?;
        let mut node = LinkedNode::new(source.root()).leaf_at_compat(cursor)?;

        // In the case that the cursor is at the end of an identifier.
        // e.g. `f(x|)`, we will select the `x`
        if cursor == node.offset() + 1 && is_mark(node.kind()) {
            let prev_leaf = node.prev_leaf();
            if let Some(prev_leaf) = prev_leaf {
                if prev_leaf.range().end == node.offset() {
                    node = prev_leaf;
                }
            }
        }

        classify_syntax(node, cursor)
    }

    /// Resolve extra font information.
    pub fn font_info(&self, font: typst::text::Font) -> Option<Arc<DataSource>> {
        self.world.font_resolver.describe_font(&font)
    }

    /// Get the local packages and their descriptions.
    pub fn local_packages(&self) -> EcoVec<PackageSpec> {
        crate::package::list_package_by_namespace(&self.world.registry, eco_format!("local"))
            .into_iter()
            .map(|(_, spec)| spec)
            .collect()
    }

    pub(crate) fn const_eval(rr: ast::Expr<'_>) -> Option<Value> {
        Some(match rr {
            ast::Expr::None(_) => Value::None,
            ast::Expr::Auto(_) => Value::Auto,
            ast::Expr::Bool(v) => Value::Bool(v.get()),
            ast::Expr::Int(v) => Value::Int(v.get()),
            ast::Expr::Float(v) => Value::Float(v.get()),
            ast::Expr::Numeric(v) => Value::numeric(v.get()),
            ast::Expr::Str(v) => Value::Str(v.get().into()),
            _ => return None,
        })
    }

    /// Get a module by file id.
    pub fn module_by_id(&self, fid: TypstFileId) -> SourceResult<Module> {
        let source = self.source_by_id(fid).at(Span::detached())?;
        self.module_by_src(source)
    }

    /// Get a module by string.
    pub fn module_by_str(&self, rr: String) -> Option<Module> {
        let src = Source::new(*DETACHED_ENTRY, rr);
        self.module_by_src(src).ok()
    }

    /// Get (Create) a module by source.
    pub fn module_by_src(&self, source: Source) -> SourceResult<Module> {
        eval_compat(&self.world, &source)
    }

    /// Try to load a module from the current source file.
    pub fn module_by_syntax(&self, source: &SyntaxNode) -> Option<Value> {
        let (src, scope) = self.analyze_import(source);
        if let Some(scope) = scope {
            return Some(scope);
        }

        match src {
            Some(Value::Str(s)) => {
                let id = resolve_id_by_path(&self.world, source.span().id()?, s.as_str())?;
                self.module_by_id(id).ok().map(Value::Module)
            }
            _ => None,
        }
    }

    /// Get the expression information of a source file.
    pub(crate) fn expr_stage_by_id(self: &Arc<Self>, fid: TypstFileId) -> Option<ExprInfo> {
        Some(self.expr_stage(&self.source_by_id(fid).ok()?))
    }

    /// Get the expression information of a source file.
    pub(crate) fn expr_stage(self: &Arc<Self>, source: &Source) -> ExprInfo {
        let mut route = ExprRoute::default();
        self.expr_stage_(source, &mut route)
    }

    /// Get the expression information of a source file.
    pub(crate) fn expr_stage_(
        self: &Arc<Self>,
        source: &Source,
        route: &mut ExprRoute,
    ) -> ExprInfo {
        use crate::syntax::expr_of;
        let guard = self.query_stat(source.id(), "expr_stage");
        self.slot.expr_stage.compute(hash128(&source), |prev| {
            expr_of(self.clone(), source.clone(), route, guard, prev)
        })
    }

    pub(crate) fn exports_of(
        self: &Arc<Self>,
        source: &Source,
        route: &mut ExprRoute,
    ) -> Option<Arc<LazyHash<LexicalScope>>> {
        if let Some(s) = route.get(&source.id()) {
            return s.clone();
        }

        Some(self.expr_stage_(source, route).exports.clone())
    }

    /// Get the type check information of a source file.
    pub(crate) fn type_check(self: &Arc<Self>, source: &Source) -> Arc<TypeInfo> {
        let mut route = TypeEnv::default();
        self.type_check_(source, &mut route)
    }

    /// Get the type check information of a source file.
    pub(crate) fn type_check_(
        self: &Arc<Self>,
        source: &Source,
        route: &mut TypeEnv,
    ) -> Arc<TypeInfo> {
        use crate::analysis::type_check;

        let ei = self.expr_stage(source);
        let guard = self.query_stat(source.id(), "type_check");
        self.slot.type_check.compute(hash128(&ei), |prev| {
            // todo: recursively check changed scheme type
            if let Some(cache_hint) = prev.filter(|prev| prev.revision == ei.revision) {
                return cache_hint;
            }

            guard.miss();
            type_check(self.clone(), ei, route)
        })
    }

    /// Get the lint result of a source file.
    #[typst_macros::time(span = source.root().span())]
    pub(crate) fn lint(self: &Arc<Self>, source: &Source) -> LintInfo {
        let ei = self.expr_stage(source);
        let ti = self.type_check(source);
        let guard = self.query_stat(source.id(), "lint");
        self.slot.lint.compute(hash128(&(&ei, &ti)), |_prev| {
            guard.miss();
            tinymist_lint::lint_file(&self.world, &ei, ti)
        })
    }

    pub(crate) fn type_of_func(self: &Arc<Self>, func: Func) -> Signature {
        crate::log_debug_ct!("convert runtime func {func:?}");
        analyze_signature(self, SignatureTarget::Convert(func)).unwrap()
    }

    pub(crate) fn type_of_value(self: &Arc<Self>, val: &Value) -> Ty {
        crate::log_debug_ct!("convert runtime value {val:?}");

        // todo: check performance on peeking signature source frequently
        let cache_key = val;
        let cached = self
            .analysis
            .caches
            .terms
            .m
            .get(&hash128(&cache_key))
            .and_then(|slot| (cache_key == &slot.1 .0).then_some(slot.1 .1.clone()));
        if let Some(cached) = cached {
            return cached;
        }

        let res = term_value(val);

        self.analysis
            .caches
            .terms
            .m
            .entry(hash128(&cache_key))
            .or_insert_with(|| (self.lifetime, (cache_key.clone(), res.clone())));

        res
    }

    pub(crate) fn def_of_span(
        self: &Arc<Self>,
        source: &Source,
        doc: Option<&TypstDocument>,
        span: Span,
    ) -> Option<Definition> {
        let syntax = self.classify_span(source, span)?;
        definition(self, source, doc, syntax)
    }

    pub(crate) fn def_of_decl(&self, decl: &Interned<Decl>) -> Option<Definition> {
        match decl.as_ref() {
            Decl::Func(..) => Some(Definition::new(decl.clone(), None)),
            Decl::Module(..) => None,
            _ => None,
        }
    }

    pub(crate) fn def_of_syntax(
        self: &Arc<Self>,
        source: &Source,
        doc: Option<&TypstDocument>,
        syntax: SyntaxClass,
    ) -> Option<Definition> {
        definition(self, source, doc, syntax)
    }

    pub(crate) fn def_of_syntax_or_dyn(
        self: &Arc<Self>,
        source: &Source,
        doc: Option<&TypstDocument>,
        syntax: SyntaxClass,
    ) -> Option<Definition> {
        let def = self.def_of_syntax(source, doc, syntax.clone());
        match def.as_ref().map(|d| d.decl.kind()) {
            // todo: DefKind::Function
            Some(DefKind::Reference | DefKind::Module | DefKind::Function) => return def,
            Some(DefKind::Struct | DefKind::Constant | DefKind::Variable) | None => {}
        }

        // Checks that we resolved a high-equality definition.
        let know_ty_well = def
            .as_ref()
            .and_then(|d| self.simplified_type_of_span(d.decl.span()))
            .filter(|ty| !matches!(ty, Ty::Any))
            .is_some();
        if know_ty_well {
            return def;
        }

        let def_ref = def.as_ref();
        let def_name = || Some(def_ref?.name().clone());
        let dyn_def = self
            .analyze_expr(syntax.node())
            .iter()
            .find_map(|(value, _)| {
                let def = Definition::from_value(value.clone(), def_name)?;
                None.or_else(|| {
                    let source = self.source_by_id(def.decl.file_id()?).ok()?;
                    let node = LinkedNode::new(source.root()).find(def.decl.span())?;
                    let def_at_the_span = classify_def_loosely(node)?;
                    self.def_of_span(&source, doc, def_at_the_span.name()?.span())
                })
                .or(Some(def))
            });

        // Uses the dynamic definition or the fallback definition.
        dyn_def.or(def)
    }

    pub(crate) fn simplified_type_of_span(self: &Arc<Self>, span: Span) -> Option<Ty> {
        let source = self.source_by_id(span.id()?).ok()?;
        let (ti, ty) = self.type_of_span_(&source, span)?;
        Some(ti.simplify(ty, false))
    }

    pub(crate) fn type_of_span(self: &Arc<Self>, span: Span) -> Option<Ty> {
        let source = self.source_by_id(span.id()?).ok()?;
        Some(self.type_of_span_(&source, span)?.1)
    }

    pub(crate) fn type_of_span_(
        self: &Arc<Self>,
        source: &Source,
        span: Span,
    ) -> Option<(Arc<TypeInfo>, Ty)> {
        let ti = self.type_check(source);
        let ty = ti.type_of_span(span)?;
        Some((ti, ty))
    }

    pub(crate) fn post_type_of_node(self: &Arc<Self>, node: LinkedNode) -> Option<Ty> {
        let id = node.span().id()?;
        let source = self.source_by_id(id).ok()?;
        let ty_chk = self.type_check(&source);

        let ty = post_type_check(self.clone(), &ty_chk, node.clone())
            .or_else(|| ty_chk.type_of_span(node.span()))?;
        Some(ty_chk.simplify(ty, false))
    }

    pub(crate) fn sig_of_def(self: &Arc<Self>, def: Definition) -> Option<Signature> {
        crate::log_debug_ct!("check definition func {def:?}");
        let source = def.decl.file_id().and_then(|id| self.source_by_id(id).ok());
        analyze_signature(self, SignatureTarget::Def(source, def))
    }

    pub(crate) fn sig_of_type(self: &Arc<Self>, ti: &TypeInfo, ty: Ty) -> Option<Signature> {
        super::sig_of_type(self, ti, ty)
    }

    pub(crate) fn sig_of_type_or_dyn(
        self: &Arc<Self>,
        ti: &TypeInfo,
        callee_ty: Ty,
        callee: &SyntaxNode,
    ) -> Option<Signature> {
        self.sig_of_type(ti, callee_ty).or_else(|| {
            self.analyze_expr(callee).iter().find_map(|(value, _)| {
                let Value::Func(callee) = value else {
                    return None;
                };

                // Converts with cache
                analyze_signature(self, SignatureTarget::Runtime(callee.clone()))
            })
        })
    }

    /// Try to find imported target from the current source file.
    /// This function will try to resolves target statically.
    ///
    /// ## Returns
    /// The first value is the resolved source.
    /// The second value is the resolved scope.
    pub fn analyze_import(&self, source: &SyntaxNode) -> (Option<Value>, Option<Value>) {
        if let Some(v) = source.cast::<ast::Expr>().and_then(Self::const_eval) {
            return (Some(v), None);
        }
        let token = &self.analysis.workers.import;
        token.enter(|| analyze_import_(&self.world, source))
    }

    /// Try to load a module from the current source file.
    pub fn analyze_expr(&self, source: &SyntaxNode) -> EcoVec<(Value, Option<Styles>)> {
        let token = &self.analysis.workers.expression;
        token.enter(|| analyze_expr_(&self.world, source))
    }

    /// Get bib info of a source file.
    pub fn analyze_bib(&self, introspector: &Introspector) -> Option<Arc<BibInfo>> {
        let world = &self.world;
        let world = (world as &dyn World).track();

        analyze_bib(world, introspector.track())
    }

    /// Describe the item under the cursor.
    ///
    /// Passing a `document` (from a previous compilation) is optional, but
    /// enhances the autocompletions. Label completions, for instance, are
    /// only generated when the document is available.
    pub fn tooltip(&self, source: &Source, cursor: usize) -> Option<Tooltip> {
        let token = &self.analysis.workers.tooltip;
        token.enter(|| tooltip_(&self.world, source, cursor))
    }

    /// Get the manifest of a package by file id.
    pub fn get_manifest(&self, toml_id: TypstFileId) -> StrResult<PackageManifest> {
        crate::package::get_manifest(&self.world, toml_id)
    }

    /// Compute the signature of a function.
    pub fn compute_signature(
        self: &Arc<Self>,
        func: SignatureTarget,
        compute: impl FnOnce(&Arc<Self>) -> Option<Signature> + Send + Sync + 'static,
    ) -> Option<Signature> {
        let res = match func {
            SignatureTarget::Def(src, def) => self
                .analysis
                .caches
                .def_signatures
                .entry(hash128(&(src, def.clone())), self.lifetime),
            SignatureTarget::SyntaxFast(source, span) => {
                let cache_key = (source, span, true);
                self.analysis
                    .caches
                    .static_signatures
                    .entry(hash128(&cache_key), self.lifetime)
            }
            SignatureTarget::Syntax(source, span) => {
                let cache_key = (source, span);
                self.analysis
                    .caches
                    .static_signatures
                    .entry(hash128(&cache_key), self.lifetime)
            }
            SignatureTarget::Convert(rt) => self
                .analysis
                .caches
                .signatures
                .entry(hash128(&(&rt, true)), self.lifetime),
            SignatureTarget::Runtime(rt) => self
                .analysis
                .caches
                .signatures
                .entry(hash128(&rt), self.lifetime),
        };
        res.get_or_init(|| compute(self)).clone()
    }

    pub(crate) fn compute_docstring(
        self: &Arc<Self>,
        fid: TypstFileId,
        docs: String,
        kind: DefKind,
    ) -> Option<Arc<DocString>> {
        let res = self
            .analysis
            .caches
            .docstrings
            .entry(hash128(&(fid, &docs, kind)), self.lifetime);
        res.get_or_init(|| {
            crate::syntax::docs::do_compute_docstring(self, fid, docs, kind).map(Arc::new)
        })
        .clone()
    }

    /// Remove html tags from markup content if necessary.
    pub fn remove_html(&self, markup: EcoString) -> EcoString {
        if !self.analysis.remove_html {
            return markup;
        }

        static REMOVE_HTML_COMMENT_REGEX: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r#"<!--[\s\S]*?-->"#).unwrap());
        REMOVE_HTML_COMMENT_REGEX
            .replace_all(&markup, "")
            .trim()
            .into()
    }

    fn query_stat(&self, id: TypstFileId, query: &'static str) -> QueryStatGuard {
        let stats = &self.analysis.stats.query_stats;
        let entry = stats.entry(id).or_default();
        let entry = entry.entry(query).or_default();
        QueryStatGuard {
            bucket: entry.clone(),
            since: std::time::SystemTime::now(),
        }
    }

    /// Check on a module before really needing them. But we likely use them
    /// after a while.
    pub(crate) fn prefetch_type_check(self: &Arc<Self>, _fid: TypstFileId) {
        // crate::log_debug_ct!("prefetch type check {fid:?}");
        // let this = self.clone();
        // rayon::spawn(move || {
        //     let Some(source) = this.world.source(fid).ok() else {
        //         return;
        //     };
        //     this.type_check(&source);
        //     // crate::log_debug_ct!("prefetch type check end {fid:?}");
        // });
    }

    pub(crate) fn preload_package(self: Arc<Self>, entry_point: TypstFileId) {
        crate::log_debug_ct!("preload package start {entry_point:?}");

        #[derive(Clone)]
        struct Preloader {
            shared: Arc<SharedContext>,
            analyzed: Arc<Mutex<HashSet<TypstFileId>>>,
        }

        impl Preloader {
            fn work(&self, fid: TypstFileId) {
                crate::log_debug_ct!("preload package {fid:?}");
                let source = self.shared.source_by_id(fid).ok().unwrap();
                let exprs = self.shared.expr_stage(&source);
                self.shared.type_check(&source);
                exprs.imports.iter().for_each(|(fid, _)| {
                    if !self.analyzed.lock().insert(*fid) {
                        return;
                    }
                    self.work(*fid);
                })
            }
        }

        let preloader = Preloader {
            shared: self,
            analyzed: Arc::default(),
        };

        preloader.work(entry_point);
    }
}

// Needed by recursive computation
type DeferredCompute<T> = Arc<OnceLock<T>>;

#[derive(Clone)]
struct IncrCacheMap<K, V> {
    revision: usize,
    global: Arc<Mutex<FxDashMap<K, (usize, V)>>>,
    prev: Arc<Mutex<FxHashMap<K, DeferredCompute<V>>>>,
    next: Arc<Mutex<FxHashMap<K, DeferredCompute<V>>>>,
}

impl<K: Eq + Hash, V> Default for IncrCacheMap<K, V> {
    fn default() -> Self {
        Self {
            revision: 0,
            global: Arc::default(),
            prev: Arc::default(),
            next: Arc::default(),
        }
    }
}

impl<K, V> IncrCacheMap<K, V> {
    fn compute(&self, key: K, compute: impl FnOnce(Option<V>) -> V) -> V
    where
        K: Clone + Eq + Hash,
        V: Clone,
    {
        let next = self.next.lock().entry(key.clone()).or_default().clone();

        next.get_or_init(|| {
            let prev = self.prev.lock().get(&key).cloned();
            let prev = prev.and_then(|prev| prev.get().cloned());
            let prev = prev.or_else(|| {
                let global = self.global.lock();
                global.get(&key).map(|global| global.1.clone())
            });

            let res = compute(prev);

            let global = self.global.lock();
            let entry = global.entry(key.clone());
            use dashmap::mapref::entry::Entry;
            match entry {
                Entry::Occupied(mut entry) => {
                    let (revision, _) = entry.get();
                    if *revision < self.revision {
                        entry.insert((self.revision, res.clone()));
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert((self.revision, res.clone()));
                }
            }

            res
        })
        .clone()
    }

    fn crawl(&self, revision: usize) -> Self {
        Self {
            revision,
            prev: self.next.clone(),
            global: self.global.clone(),
            next: Default::default(),
        }
    }
}

#[derive(Clone)]
struct CacheMap<T> {
    m: Arc<FxDashMap<u128, (u64, T)>>,
    // pub alloc: AllocStats,
}

impl<T> Default for CacheMap<T> {
    fn default() -> Self {
        Self {
            m: Default::default(),
            // alloc: Default::default(),
        }
    }
}

impl<T> CacheMap<T> {
    fn clear(&self) {
        self.m.clear();
    }

    fn retain(&self, mut f: impl FnMut(&mut (u64, T)) -> bool) {
        self.m.retain(|_k, v| f(v));
    }
}

impl<T: Default + Clone> CacheMap<T> {
    fn entry(&self, key: u128, lifetime: u64) -> T {
        let entry = self.m.entry(key);
        let entry = entry.or_insert_with(|| (lifetime, T::default()));
        entry.1.clone()
    }
}

/// Shared workers to limit resource usage
#[derive(Default)]
pub struct AnalysisGlobalWorkers {
    /// A possible long running import dynamic analysis task
    import: RateLimiter,
    /// A possible long running expression dynamic analysis task
    expression: RateLimiter,
    /// A possible long running tooltip dynamic analysis task
    tooltip: RateLimiter,
}

/// A global (compiler server spanned) cache for all level of analysis results
/// of a module.
#[derive(Default, Clone)]
pub struct AnalysisGlobalCaches {
    lifetime: Arc<AtomicU64>,
    clear_lifetime: Arc<AtomicU64>,
    def_signatures: CacheMap<DeferredCompute<Option<Signature>>>,
    static_signatures: CacheMap<DeferredCompute<Option<Signature>>>,
    signatures: CacheMap<DeferredCompute<Option<Signature>>>,
    docstrings: CacheMap<DeferredCompute<Option<Arc<DocString>>>>,
    terms: CacheMap<(Value, Ty)>,
}

/// A local (lsp request spanned) cache for all level of analysis results of a
/// module.
///
/// You should not hold it across requests, because input like source code may
/// change.
#[derive(Default)]
pub struct AnalysisLocalCaches {
    modules: HashMap<TypstFileId, ModuleAnalysisLocalCache>,
    completion_files: OnceLock<Vec<TypstFileId>>,
    root_files: OnceLock<Vec<TypstFileId>>,
    module_deps: OnceLock<HashMap<TypstFileId, ModuleDependency>>,
}

/// A local cache for module-level analysis results of a module.
///
/// You should not hold it across requests, because input like source code may
/// change.
#[derive(Default)]
pub struct ModuleAnalysisLocalCache {
    expr_stage: OnceLock<ExprInfo>,
    type_check: OnceLock<Arc<TypeInfo>>,
}

/// A revision-managed (per input change) cache for all level of analysis
/// results of a module.
#[derive(Default)]
pub struct AnalysisRevCache {
    default_slot: AnalysisRevSlot,
    manager: RevisionManager<AnalysisRevSlot>,
}

impl RevisionManagerLike for AnalysisRevCache {
    fn gc(&mut self, rev: usize) {
        self.manager.gc(rev);

        // todo: the following code are time consuming.
        {
            let mut max_ei = FxHashMap::default();
            let es = self.default_slot.expr_stage.global.lock();
            for r in es.iter() {
                let rev: &mut usize = max_ei.entry(r.1.fid).or_default();
                *rev = (*rev).max(r.1.revision);
            }
            es.retain(|_, r| r.1.revision == *max_ei.get(&r.1.fid).unwrap_or(&0));
        }

        {
            let mut max_ti = FxHashMap::default();
            let ts = self.default_slot.type_check.global.lock();
            for r in ts.iter() {
                let rev: &mut usize = max_ti.entry(r.1.fid).or_default();
                *rev = (*rev).max(r.1.revision);
            }
            ts.retain(|_, r| r.1.revision == *max_ti.get(&r.1.fid).unwrap_or(&0));
        }

        {
            let mut max_li = FxHashMap::default();
            let ts = self.default_slot.lint.global.lock();
            for r in ts.iter() {
                let rev: &mut usize = max_li.entry(r.1.fid).or_default();
                *rev = (*rev).max(r.1.revision);
            }
            ts.retain(|_, r| r.1.revision == *max_li.get(&r.1.fid).unwrap_or(&0));
        }
    }
}

impl AnalysisRevCache {
    fn clear(&mut self) {
        self.manager.clear();
        self.default_slot = Default::default();
    }

    /// Find the last revision slot by revision number.
    fn find_revision(
        &mut self,
        revision: NonZeroUsize,
        lg: &AnalysisRevLock,
    ) -> Arc<RevisionSlot<AnalysisRevSlot>> {
        lg.inner.access(revision);
        self.manager.find_revision(revision, |slot_base| {
            log::debug!("analysis revision {} is created", revision.get());
            slot_base
                .map(|slot| AnalysisRevSlot {
                    revision: slot.revision,
                    expr_stage: slot.data.expr_stage.crawl(revision.get()),
                    type_check: slot.data.type_check.crawl(revision.get()),
                    lint: slot.data.lint.crawl(revision.get()),
                })
                .unwrap_or_else(|| self.default_slot.clone())
        })
    }
}

/// A lock for revision.
pub struct AnalysisRevLock {
    inner: RevisionLock,
    tokens: Option<SemanticTokenContext>,
    grid: Arc<Mutex<AnalysisRevCache>>,
}

impl Drop for AnalysisRevLock {
    fn drop(&mut self) {
        let mut mu = self.grid.lock();
        let gc_revision = mu.manager.unlock(&mut self.inner);

        if let Some(gc_revision) = gc_revision {
            let grid = self.grid.clone();
            rayon::spawn(move || {
                grid.lock().gc(gc_revision);
            });
        }
    }
}

#[derive(Default, Clone)]
struct AnalysisRevSlot {
    revision: usize,
    expr_stage: IncrCacheMap<u128, ExprInfo>,
    type_check: IncrCacheMap<u128, Arc<TypeInfo>>,
    lint: IncrCacheMap<u128, LintInfo>,
}

impl Drop for AnalysisRevSlot {
    fn drop(&mut self) {
        log::debug!("analysis revision {} is dropped", self.revision);
    }
}

fn ceil_char_boundary(text: &str, mut cursor: usize) -> usize {
    // while is not char boundary, move cursor to right
    while cursor < text.len() && !text.is_char_boundary(cursor) {
        cursor += 1;
    }

    cursor.min(text.len())
}

#[typst_macros::time]
#[comemo::memoize]
fn analyze_bib(
    world: Tracked<dyn World + '_>,
    introspector: Tracked<Introspector>,
) -> Option<Arc<BibInfo>> {
    let bib_elem = BibliographyElem::find(introspector).ok()?;

    // todo: it doesn't respect the style chain which can be get from
    // `analyze_expr`
    let csl_style = bib_elem
        .style
        .get_ref(StyleChain::default())
        .derived
        .clone();

    let Value::Array(paths) = bib_elem.sources.clone().into_value() else {
        return None;
    };
    let elem_fid = bib_elem.span().id()?;
    let files = paths
        .into_iter()
        .flat_map(|path| path.cast().ok())
        .flat_map(|bib_path: EcoString| {
            let bib_fid = resolve_id_by_path(world.deref(), elem_fid, &bib_path)?;
            Some((bib_fid, world.file(bib_fid).ok()?))
        });

    bib_info(csl_style, files)
}

#[comemo::memoize]
fn loc_info(bytes: Bytes) -> Option<EcoVec<(usize, String)>> {
    let mut loc = EcoVec::new();
    let mut offset = 0;
    for line in bytes.split(|byte| *byte == b'\n') {
        loc.push((offset, String::from_utf8(line.to_owned()).ok()?));
        offset += line.len() + 1;
    }
    Some(loc)
}

fn find_loc(
    len: usize,
    loc: &EcoVec<(usize, String)>,
    mut offset: usize,
    encoding: PositionEncoding,
) -> Option<LspPosition> {
    if offset > len {
        offset = len;
    }

    let r = match loc.binary_search_by_key(&offset, |line| line.0) {
        Ok(i) => i,
        Err(i) => i - 1,
    };

    let (start, s) = loc.get(r)?;
    let byte_offset = offset.saturating_sub(*start);

    let column_prefix = if byte_offset <= s.len() {
        &s[..byte_offset]
    } else {
        let line = (r + 1) as u32;
        return Some(LspPosition { line, character: 0 });
    };

    let line = r as u32;
    let character = match encoding {
        PositionEncoding::Utf8 => column_prefix.chars().count(),
        PositionEncoding::Utf16 => column_prefix.chars().map(|ch| ch.len_utf16()).sum(),
    } as u32;

    Some(LspPosition { line, character })
}

/// The context for searching in the workspace.
pub struct SearchCtx<'a> {
    /// The inner analysis context.
    pub ctx: &'a mut LocalContext,
    /// The set of files that have been searched.
    pub searched: HashSet<TypstFileId>,
    /// The files that need to be searched.
    pub worklist: Vec<TypstFileId>,
}

impl SearchCtx<'_> {
    /// Push a file to the worklist.
    pub fn push(&mut self, fid: TypstFileId) -> bool {
        if self.searched.insert(fid) {
            self.worklist.push(fid);
            true
        } else {
            false
        }
    }

    /// Push the dependents of a file to the worklist.
    pub fn push_dependents(&mut self, fid: TypstFileId) {
        let deps = self.ctx.module_dependencies().get(&fid);
        let dependents = deps.map(|dep| dep.dependents.clone()).into_iter().flatten();
        for dep in dependents {
            self.push(dep);
        }
    }
}

/// A rate limiter on some (cpu-heavy) action
#[derive(Default)]
pub struct RateLimiter {
    token: std::sync::Mutex<()>,
}

impl RateLimiter {
    /// Executes some (cpu-heavy) action with rate limit
    #[must_use]
    pub fn enter<T>(&self, f: impl FnOnce() -> T) -> T {
        let _c = self.token.lock().unwrap();
        f()
    }
}
