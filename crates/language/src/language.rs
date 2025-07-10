mod buffer;
mod diagnostic_set;
mod highlight_map;
mod outline;
pub mod proto;
mod syntax_map;

#[cfg(test)]
mod buffer_tests;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use client::http::HttpClient;
use collections::HashMap;
use futures::{
    future::{BoxFuture, Shared},
    FutureExt, TryFutureExt,
};
use gpui::{MutableAppContext, Task};
use highlight_map::HighlightMap;
use lazy_static::lazy_static;
use parking_lot::{Mutex, RwLock};
use postage::watch;
use regex::Regex;
use serde::{de, Deserialize, Deserializer};
use serde_json::Value;
use std::{
    any::Any,
    cell::RefCell,
    fmt::Debug,
    mem,
    ops::Range,
    path::{Path, PathBuf},
    str,
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc,
    },
};
use syntax_map::SyntaxSnapshot;
use theme::{SyntaxTheme, Theme};
use tree_sitter::{self, Query};
use util::ResultExt;

#[cfg(any(test, feature = "test-support"))]
use futures::channel::mpsc;

pub use buffer::Operation;
pub use buffer::*;
pub use diagnostic_set::DiagnosticEntry;
pub use outline::{Outline, OutlineItem};
pub use tree_sitter::{Parser, Tree};

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

lazy_static! {
    pub static ref NEXT_GRAMMAR_ID: AtomicUsize = Default::default();
    pub static ref PLAIN_TEXT: Arc<Language> = Arc::new(Language::new(
        LanguageConfig {
            name: "Plain Text".into(),
            ..Default::default()
        },
        None,
    ));
}

pub trait ToLspPosition {
    fn to_lsp_position(self) -> lsp::Position;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LanguageServerName(pub Arc<str>);

/// Represents a Language Server, with certain cached sync properties.
/// Uses [`LspAdapter`] under the hood, but calls all 'static' methods
/// once at startup, and caches the results.
pub struct CachedLspAdapter {
    pub name: LanguageServerName,
    pub server_args: Vec<String>,
    pub initialization_options: Option<Value>,
    pub disk_based_diagnostic_sources: Vec<String>,
    pub disk_based_diagnostics_progress_token: Option<String>,
    pub language_ids: HashMap<String, String>,
    pub adapter: Box<dyn LspAdapter>,
}

impl CachedLspAdapter {
    pub async fn new<T: LspAdapter>(adapter: T) -> Arc<Self> {
        let adapter = Box::new(adapter);
        let name = adapter.name().await;
        let server_args = adapter.server_args().await;
        let initialization_options = adapter.initialization_options().await;
        let disk_based_diagnostic_sources = adapter.disk_based_diagnostic_sources().await;
        let disk_based_diagnostics_progress_token =
            adapter.disk_based_diagnostics_progress_token().await;
        let language_ids = adapter.language_ids().await;

        Arc::new(CachedLspAdapter {
            name,
            server_args,
            initialization_options,
            disk_based_diagnostic_sources,
            disk_based_diagnostics_progress_token,
            language_ids,
            adapter,
        })
    }

    pub async fn fetch_latest_server_version(
        &self,
        http: Arc<dyn HttpClient>,
    ) -> Result<Box<dyn 'static + Send + Any>> {
        self.adapter.fetch_latest_server_version(http).await
    }

    pub async fn fetch_server_binary(
        &self,
        version: Box<dyn 'static + Send + Any>,
        http: Arc<dyn HttpClient>,
        container_dir: PathBuf,
    ) -> Result<PathBuf> {
        self.adapter
            .fetch_server_binary(version, http, container_dir)
            .await
    }

    pub async fn cached_server_binary(&self, container_dir: PathBuf) -> Option<PathBuf> {
        self.adapter.cached_server_binary(container_dir).await
    }

    pub async fn process_diagnostics(&self, params: &mut lsp::PublishDiagnosticsParams) {
        self.adapter.process_diagnostics(params).await
    }

    pub async fn label_for_completion(
        &self,
        completion_item: &lsp::CompletionItem,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        self.adapter
            .label_for_completion(completion_item, language)
            .await
    }

    pub async fn label_for_symbol(
        &self,
        name: &str,
        kind: lsp::SymbolKind,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        self.adapter.label_for_symbol(name, kind, language).await
    }
}

#[async_trait]
pub trait LspAdapter: 'static + Send + Sync {
    async fn name(&self) -> LanguageServerName;

    async fn fetch_latest_server_version(
        &self,
        http: Arc<dyn HttpClient>,
    ) -> Result<Box<dyn 'static + Send + Any>>;

    async fn fetch_server_binary(
        &self,
        version: Box<dyn 'static + Send + Any>,
        http: Arc<dyn HttpClient>,
        container_dir: PathBuf,
    ) -> Result<PathBuf>;

    async fn cached_server_binary(&self, container_dir: PathBuf) -> Option<PathBuf>;

    async fn process_diagnostics(&self, _: &mut lsp::PublishDiagnosticsParams) {}

    async fn label_for_completion(
        &self,
        _: &lsp::CompletionItem,
        _: &Arc<Language>,
    ) -> Option<CodeLabel> {
        None
    }

    async fn label_for_symbol(
        &self,
        _: &str,
        _: lsp::SymbolKind,
        _: &Arc<Language>,
    ) -> Option<CodeLabel> {
        None
    }

    async fn server_args(&self) -> Vec<String> {
        Vec::new()
    }

    async fn initialization_options(&self) -> Option<Value> {
        None
    }

    async fn disk_based_diagnostic_sources(&self) -> Vec<String> {
        Default::default()
    }

    async fn disk_based_diagnostics_progress_token(&self) -> Option<String> {
        None
    }

    async fn language_ids(&self) -> HashMap<String, String> {
        Default::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeLabel {
    pub text: String,
    pub runs: Vec<(Range<usize>, HighlightId)>,
    pub filter_range: Range<usize>,
}

#[derive(Deserialize)]
pub struct LanguageConfig {
    pub name: Arc<str>,
    pub path_suffixes: Vec<String>,
    pub brackets: Vec<BracketPair>,
    #[serde(default = "auto_indent_using_last_non_empty_line_default")]
    pub auto_indent_using_last_non_empty_line: bool,
    #[serde(default, deserialize_with = "deserialize_regex")]
    pub increase_indent_pattern: Option<Regex>,
    #[serde(default, deserialize_with = "deserialize_regex")]
    pub decrease_indent_pattern: Option<Regex>,
    #[serde(default)]
    pub autoclose_before: String,
    #[serde(default)]
    pub line_comment: Option<Arc<str>>,
    #[serde(default)]
    pub block_comment: Option<(Arc<str>, Arc<str>)>,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            name: "".into(),
            path_suffixes: Default::default(),
            brackets: Default::default(),
            auto_indent_using_last_non_empty_line: auto_indent_using_last_non_empty_line_default(),
            increase_indent_pattern: Default::default(),
            decrease_indent_pattern: Default::default(),
            autoclose_before: Default::default(),
            line_comment: Default::default(),
            block_comment: Default::default(),
        }
    }
}

fn auto_indent_using_last_non_empty_line_default() -> bool {
    true
}

fn deserialize_regex<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Regex>, D::Error> {
    let source = Option::<String>::deserialize(d)?;
    if let Some(source) = source {
        Ok(Some(regex::Regex::new(&source).map_err(de::Error::custom)?))
    } else {
        Ok(None)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub struct FakeLspAdapter {
    pub name: &'static str,
    pub capabilities: lsp::ServerCapabilities,
    pub initializer: Option<Box<dyn 'static + Send + Sync + Fn(&mut lsp::FakeLanguageServer)>>,
    pub disk_based_diagnostics_progress_token: Option<String>,
    pub disk_based_diagnostics_sources: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BracketPair {
    pub start: String,
    pub end: String,
    pub close: bool,
    pub newline: bool,
}

pub struct Language {
    pub(crate) config: LanguageConfig,
    pub(crate) grammar: Option<Arc<Grammar>>,
    pub(crate) adapter: Option<Arc<CachedLspAdapter>>,

    #[cfg(any(test, feature = "test-support"))]
    fake_adapter: Option<(
        mpsc::UnboundedSender<lsp::FakeLanguageServer>,
        Arc<FakeLspAdapter>,
    )>,
}

pub struct Grammar {
    id: usize,
    pub(crate) ts_language: tree_sitter::Language,
    pub(crate) highlights_query: Option<Query>,
    pub(crate) brackets_config: Option<BracketConfig>,
    pub(crate) indents_config: Option<IndentConfig>,
    pub(crate) outline_config: Option<OutlineConfig>,
    pub(crate) injection_config: Option<InjectionConfig>,
    pub(crate) highlight_map: Mutex<HighlightMap>,
}

struct IndentConfig {
    query: Query,
    indent_capture_ix: u32,
    start_capture_ix: Option<u32>,
    end_capture_ix: Option<u32>,
}

struct OutlineConfig {
    query: Query,
    item_capture_ix: u32,
    name_capture_ix: u32,
    context_capture_ix: Option<u32>,
}

struct InjectionConfig {
    query: Query,
    content_capture_ix: u32,
    language_capture_ix: Option<u32>,
    languages_by_pattern_ix: Vec<Option<Box<str>>>,
}

struct BracketConfig {
    query: Query,
    open_capture_ix: u32,
    close_capture_ix: u32,
}

#[derive(Clone)]
pub enum LanguageServerBinaryStatus {
    CheckingForUpdate,
    Downloading,
    Downloaded,
    Cached,
    Failed { error: String },
}

pub struct LanguageRegistry {
    languages: RwLock<Vec<Arc<Language>>>,
    language_server_download_dir: Option<Arc<Path>>,
    lsp_binary_statuses_tx: async_broadcast::Sender<(Arc<Language>, LanguageServerBinaryStatus)>,
    lsp_binary_statuses_rx: async_broadcast::Receiver<(Arc<Language>, LanguageServerBinaryStatus)>,
    login_shell_env_loaded: Shared<Task<()>>,
    #[allow(clippy::type_complexity)]
    lsp_binary_paths: Mutex<
        HashMap<
            LanguageServerName,
            Shared<BoxFuture<'static, Result<PathBuf, Arc<anyhow::Error>>>>,
        >,
    >,
    subscription: RwLock<(watch::Sender<()>, watch::Receiver<()>)>,
    theme: RwLock<Option<Arc<Theme>>>,
}

impl LanguageRegistry {
    pub fn new(login_shell_env_loaded: Task<()>) -> Self {
        let (lsp_binary_statuses_tx, lsp_binary_statuses_rx) = async_broadcast::broadcast(16);
        Self {
            language_server_download_dir: None,
            languages: Default::default(),
            lsp_binary_statuses_tx,
            lsp_binary_statuses_rx,
            login_shell_env_loaded: login_shell_env_loaded.shared(),
            lsp_binary_paths: Default::default(),
            subscription: RwLock::new(watch::channel()),
            theme: Default::default(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> Self {
        Self::new(Task::ready(()))
    }

    pub fn add(&self, language: Arc<Language>) {
        if let Some(theme) = self.theme.read().clone() {
            language.set_theme(&theme.editor.syntax);
        }
        self.languages.write().push(language);
        *self.subscription.write().0.borrow_mut() = ();
    }

    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.subscription.read().1.clone()
    }

    pub fn set_theme(&self, theme: Arc<Theme>) {
        *self.theme.write() = Some(theme.clone());
        for language in self.languages.read().iter() {
            language.set_theme(&theme.editor.syntax);
        }
    }

    pub fn set_language_server_download_dir(&mut self, path: impl Into<Arc<Path>>) {
        self.language_server_download_dir = Some(path.into());
    }

    pub fn get_language(&self, name: &str) -> Option<Arc<Language>> {
        self.languages
            .read()
            .iter()
            .find(|language| language.name().to_lowercase() == name.to_lowercase())
            .cloned()
    }

    pub fn to_vec(&self) -> Vec<Arc<Language>> {
        self.languages.read().iter().cloned().collect()
    }

    pub fn language_names(&self) -> Vec<String> {
        self.languages
            .read()
            .iter()
            .map(|language| language.name().to_string())
            .collect()
    }

    pub fn select_language(&self, path: impl AsRef<Path>) -> Option<Arc<Language>> {
        let path = path.as_ref();
        let filename = path.file_name().and_then(|name| name.to_str());
        let extension = path.extension().and_then(|name| name.to_str());
        let path_suffixes = [extension, filename];
        self.languages
            .read()
            .iter()
            .find(|language| {
                language
                    .config
                    .path_suffixes
                    .iter()
                    .any(|suffix| path_suffixes.contains(&Some(suffix.as_str())))
            })
            .cloned()
    }

    pub fn start_language_server(
        self: &Arc<Self>,
        server_id: usize,
        language: Arc<Language>,
        root_path: Arc<Path>,
        http_client: Arc<dyn HttpClient>,
        cx: &mut MutableAppContext,
    ) -> Option<Task<Result<lsp::LanguageServer>>> {
        #[cfg(any(test, feature = "test-support"))]
        if language.fake_adapter.is_some() {
            let language = language;
            return Some(cx.spawn(|cx| async move {
                let (servers_tx, fake_adapter) = language.fake_adapter.as_ref().unwrap();
                let (server, mut fake_server) = lsp::LanguageServer::fake(
                    fake_adapter.name.to_string(),
                    fake_adapter.capabilities.clone(),
                    cx.clone(),
                );

                if let Some(initializer) = &fake_adapter.initializer {
                    initializer(&mut fake_server);
                }

                let servers_tx = servers_tx.clone();
                cx.background()
                    .spawn(async move {
                        if fake_server
                            .try_receive_notification::<lsp::notification::Initialized>()
                            .await
                            .is_some()
                        {
                            servers_tx.unbounded_send(fake_server).ok();
                        }
                    })
                    .detach();
                Ok(server)
            }));
        }

        let download_dir = self
            .language_server_download_dir
            .clone()
            .ok_or_else(|| anyhow!("language server download directory has not been assigned"))
            .log_err()?;

        let this = self.clone();
        let adapter = language.adapter.clone()?;
        let lsp_binary_statuses = self.lsp_binary_statuses_tx.clone();
        let login_shell_env_loaded = self.login_shell_env_loaded.clone();
        Some(cx.spawn(|cx| async move {
            login_shell_env_loaded.await;
            let server_binary_path = this
                .lsp_binary_paths
                .lock()
                .entry(adapter.name.clone())
                .or_insert_with(|| {
                    get_server_binary_path(
                        adapter.clone(),
                        language.clone(),
                        http_client,
                        download_dir,
                        lsp_binary_statuses,
                    )
                    .map_err(Arc::new)
                    .boxed()
                    .shared()
                })
                .clone()
                .map_err(|e| anyhow!(e));

            let server_binary_path = server_binary_path.await?;
            let server_args = &adapter.server_args;
            let server = lsp::LanguageServer::new(
                server_id,
                &server_binary_path,
                server_args,
                &root_path,
                cx,
            )?;
            Ok(server)
        }))
    }

    pub fn language_server_binary_statuses(
        &self,
    ) -> async_broadcast::Receiver<(Arc<Language>, LanguageServerBinaryStatus)> {
        self.lsp_binary_statuses_rx.clone()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::test()
    }
}

async fn get_server_binary_path(
    adapter: Arc<CachedLspAdapter>,
    language: Arc<Language>,
    http_client: Arc<dyn HttpClient>,
    download_dir: Arc<Path>,
    statuses: async_broadcast::Sender<(Arc<Language>, LanguageServerBinaryStatus)>,
) -> Result<PathBuf> {
    let container_dir = download_dir.join(adapter.name.0.as_ref());
    if !container_dir.exists() {
        smol::fs::create_dir_all(&container_dir)
            .await
            .context("failed to create container directory")?;
    }

    let path = fetch_latest_server_binary_path(
        adapter.clone(),
        language.clone(),
        http_client,
        &container_dir,
        statuses.clone(),
    )
    .await;
    if let Err(error) = path.as_ref() {
        if let Some(cached_path) = adapter.cached_server_binary(container_dir).await {
            statuses
                .broadcast((language.clone(), LanguageServerBinaryStatus::Cached))
                .await?;
            return Ok(cached_path);
        } else {
            statuses
                .broadcast((
                    language.clone(),
                    LanguageServerBinaryStatus::Failed {
                        error: format!("{:?}", error),
                    },
                ))
                .await?;
        }
    }
    path
}

async fn fetch_latest_server_binary_path(
    adapter: Arc<CachedLspAdapter>,
    language: Arc<Language>,
    http_client: Arc<dyn HttpClient>,
    container_dir: &Path,
    lsp_binary_statuses_tx: async_broadcast::Sender<(Arc<Language>, LanguageServerBinaryStatus)>,
) -> Result<PathBuf> {
    let container_dir: Arc<Path> = container_dir.into();
    lsp_binary_statuses_tx
        .broadcast((
            language.clone(),
            LanguageServerBinaryStatus::CheckingForUpdate,
        ))
        .await?;
    let version_info = adapter
        .fetch_latest_server_version(http_client.clone())
        .await?;
    lsp_binary_statuses_tx
        .broadcast((language.clone(), LanguageServerBinaryStatus::Downloading))
        .await?;
    let path = adapter
        .fetch_server_binary(version_info, http_client, container_dir.to_path_buf())
        .await?;
    lsp_binary_statuses_tx
        .broadcast((language.clone(), LanguageServerBinaryStatus::Downloaded))
        .await?;
    Ok(path)
}

impl Language {
    pub fn new(config: LanguageConfig, ts_language: Option<tree_sitter::Language>) -> Self {
        Self {
            config,
            grammar: ts_language.map(|ts_language| {
                Arc::new(Grammar {
                    id: NEXT_GRAMMAR_ID.fetch_add(1, SeqCst),
                    highlights_query: None,
                    brackets_config: None,
                    outline_config: None,
                    indents_config: None,
                    injection_config: None,
                    ts_language,
                    highlight_map: Default::default(),
                })
            }),
            adapter: None,

            #[cfg(any(test, feature = "test-support"))]
            fake_adapter: None,
        }
    }

    pub fn lsp_adapter(&self) -> Option<Arc<CachedLspAdapter>> {
        self.adapter.clone()
    }

    pub fn with_highlights_query(mut self, source: &str) -> Result<Self> {
        let grammar = self.grammar_mut();
        grammar.highlights_query = Some(Query::new(grammar.ts_language, source)?);
        Ok(self)
    }

    pub fn with_brackets_query(mut self, source: &str) -> Result<Self> {
        let grammar = self.grammar_mut();
        let query = Query::new(grammar.ts_language, source)?;
        let mut open_capture_ix = None;
        let mut close_capture_ix = None;
        get_capture_indices(
            &query,
            &mut [
                ("open", &mut open_capture_ix),
                ("close", &mut close_capture_ix),
            ],
        );
        if let Some((open_capture_ix, close_capture_ix)) = open_capture_ix.zip(close_capture_ix) {
            grammar.brackets_config = Some(BracketConfig {
                query,
                open_capture_ix,
                close_capture_ix,
            });
        }
        Ok(self)
    }

    pub fn with_indents_query(mut self, source: &str) -> Result<Self> {
        let grammar = self.grammar_mut();
        let query = Query::new(grammar.ts_language, source)?;
        let mut indent_capture_ix = None;
        let mut start_capture_ix = None;
        let mut end_capture_ix = None;
        get_capture_indices(
            &query,
            &mut [
                ("indent", &mut indent_capture_ix),
                ("start", &mut start_capture_ix),
                ("end", &mut end_capture_ix),
            ],
        );
        if let Some(indent_capture_ix) = indent_capture_ix {
            grammar.indents_config = Some(IndentConfig {
                query,
                indent_capture_ix,
                start_capture_ix,
                end_capture_ix,
            });
        }
        Ok(self)
    }

    pub fn with_outline_query(mut self, source: &str) -> Result<Self> {
        let grammar = self.grammar_mut();
        let query = Query::new(grammar.ts_language, source)?;
        let mut item_capture_ix = None;
        let mut name_capture_ix = None;
        let mut context_capture_ix = None;
        get_capture_indices(
            &query,
            &mut [
                ("item", &mut item_capture_ix),
                ("name", &mut name_capture_ix),
                ("context", &mut context_capture_ix),
            ],
        );
        if let Some((item_capture_ix, name_capture_ix)) = item_capture_ix.zip(name_capture_ix) {
            grammar.outline_config = Some(OutlineConfig {
                query,
                item_capture_ix,
                name_capture_ix,
                context_capture_ix,
            });
        }
        Ok(self)
    }

    pub fn with_injection_query(mut self, source: &str) -> Result<Self> {
        let grammar = self.grammar_mut();
        let query = Query::new(grammar.ts_language, source)?;
        let mut language_capture_ix = None;
        let mut content_capture_ix = None;
        get_capture_indices(
            &query,
            &mut [
                ("language", &mut language_capture_ix),
                ("content", &mut content_capture_ix),
            ],
        );
        let languages_by_pattern_ix = (0..query.pattern_count())
            .map(|ix| {
                query.property_settings(ix).iter().find_map(|setting| {
                    if setting.key.as_ref() == "language" {
                        return setting.value.clone();
                    } else {
                        None
                    }
                })
            })
            .collect();
        if let Some(content_capture_ix) = content_capture_ix {
            grammar.injection_config = Some(InjectionConfig {
                query,
                language_capture_ix,
                content_capture_ix,
                languages_by_pattern_ix,
            });
        }
        Ok(self)
    }

    fn grammar_mut(&mut self) -> &mut Grammar {
        Arc::get_mut(self.grammar.as_mut().unwrap()).unwrap()
    }

    pub fn with_lsp_adapter(mut self, lsp_adapter: Arc<CachedLspAdapter>) -> Self {
        self.adapter = Some(lsp_adapter);
        self
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_fake_lsp_adapter(
        &mut self,
        fake_lsp_adapter: Arc<FakeLspAdapter>,
    ) -> mpsc::UnboundedReceiver<lsp::FakeLanguageServer> {
        let (servers_tx, servers_rx) = mpsc::unbounded();
        self.fake_adapter = Some((servers_tx, fake_lsp_adapter.clone()));
        let adapter = CachedLspAdapter::new(fake_lsp_adapter).await;
        self.adapter = Some(adapter);
        servers_rx
    }

    pub fn name(&self) -> Arc<str> {
        self.config.name.clone()
    }

    pub fn line_comment_prefix(&self) -> Option<&Arc<str>> {
        self.config.line_comment.as_ref()
    }

    pub fn block_comment_delimiters(&self) -> Option<(&Arc<str>, &Arc<str>)> {
        self.config
            .block_comment
            .as_ref()
            .map(|(start, end)| (start, end))
    }

    pub async fn disk_based_diagnostic_sources(&self) -> &[String] {
        match self.adapter.as_ref() {
            Some(adapter) => &adapter.disk_based_diagnostic_sources,
            None => &[],
        }
    }

    pub async fn disk_based_diagnostics_progress_token(&self) -> Option<&str> {
        if let Some(adapter) = self.adapter.as_ref() {
            adapter.disk_based_diagnostics_progress_token.as_deref()
        } else {
            None
        }
    }

    pub async fn process_diagnostics(&self, diagnostics: &mut lsp::PublishDiagnosticsParams) {
        if let Some(processor) = self.adapter.as_ref() {
            processor.process_diagnostics(diagnostics).await;
        }
    }

    pub async fn label_for_completion(
        self: &Arc<Self>,
        completion: &lsp::CompletionItem,
    ) -> Option<CodeLabel> {
        self.adapter
            .as_ref()?
            .label_for_completion(completion, self)
            .await
    }

    pub async fn label_for_symbol(
        self: &Arc<Self>,
        name: &str,
        kind: lsp::SymbolKind,
    ) -> Option<CodeLabel> {
        self.adapter
            .as_ref()?
            .label_for_symbol(name, kind, self)
            .await
    }

    pub fn highlight_text<'a>(
        self: &'a Arc<Self>,
        text: &'a Rope,
        range: Range<usize>,
    ) -> Vec<(Range<usize>, HighlightId)> {
        let mut result = Vec::new();
        if let Some(grammar) = &self.grammar {
            let tree = grammar.parse_text(text, None);
            let captures =
                SyntaxSnapshot::single_tree_captures(range.clone(), text, &tree, self, |grammar| {
                    grammar.highlights_query.as_ref()
                });
            let highlight_maps = vec![grammar.highlight_map()];
            let mut offset = 0;
            for chunk in BufferChunks::new(text, range, Some((captures, highlight_maps)), vec![]) {
                let end_offset = offset + chunk.text.len();
                if let Some(highlight_id) = chunk.syntax_highlight_id {
                    if !highlight_id.is_default() {
                        result.push((offset..end_offset, highlight_id));
                    }
                }
                offset = end_offset;
            }
        }
        result
    }

    pub fn brackets(&self) -> &[BracketPair] {
        &self.config.brackets
    }

    pub fn path_suffixes(&self) -> &[String] {
        &self.config.path_suffixes
    }

    pub fn should_autoclose_before(&self, c: char) -> bool {
        c.is_whitespace() || self.config.autoclose_before.contains(c)
    }

    pub fn set_theme(&self, theme: &SyntaxTheme) {
        if let Some(grammar) = self.grammar.as_ref() {
            if let Some(highlights_query) = &grammar.highlights_query {
                *grammar.highlight_map.lock() =
                    HighlightMap::new(highlights_query.capture_names(), theme);
            }
        }
    }

    pub fn grammar(&self) -> Option<&Arc<Grammar>> {
        self.grammar.as_ref()
    }
}

impl Debug for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Language")
            .field("name", &self.config.name)
            .finish()
    }
}

impl Grammar {
    pub fn id(&self) -> usize {
        self.id
    }

    fn parse_text(&self, text: &Rope, old_tree: Option<Tree>) -> Tree {
        PARSER.with(|parser| {
            let mut parser = parser.borrow_mut();
            parser
                .set_language(self.ts_language)
                .expect("incompatible grammar");
            let mut chunks = text.chunks_in_range(0..text.len());
            parser
                .parse_with(
                    &mut move |offset, _| {
                        chunks.seek(offset);
                        chunks.next().unwrap_or("").as_bytes()
                    },
                    old_tree.as_ref(),
                )
                .unwrap()
        })
    }

    pub fn highlight_map(&self) -> HighlightMap {
        self.highlight_map.lock().clone()
    }

    pub fn highlight_id_for_name(&self, name: &str) -> Option<HighlightId> {
        let capture_id = self
            .highlights_query
            .as_ref()?
            .capture_index_for_name(name)?;
        Some(self.highlight_map.lock().get(capture_id))
    }
}

impl CodeLabel {
    pub fn plain(text: String, filter_text: Option<&str>) -> Self {
        let mut result = Self {
            runs: Vec::new(),
            filter_range: 0..text.len(),
            text,
        };
        if let Some(filter_text) = filter_text {
            if let Some(ix) = result.text.find(filter_text) {
                result.filter_range = ix..ix + filter_text.len();
            }
        }
        result
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for FakeLspAdapter {
    fn default() -> Self {
        Self {
            name: "the-fake-language-server",
            capabilities: lsp::LanguageServer::full_capabilities(),
            initializer: None,
            disk_based_diagnostics_progress_token: None,
            disk_based_diagnostics_sources: Vec::new(),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl LspAdapter for Arc<FakeLspAdapter> {
    async fn name(&self) -> LanguageServerName {
        LanguageServerName(self.name.into())
    }

    async fn fetch_latest_server_version(
        &self,
        _: Arc<dyn HttpClient>,
    ) -> Result<Box<dyn 'static + Send + Any>> {
        unreachable!();
    }

    async fn fetch_server_binary(
        &self,
        _: Box<dyn 'static + Send + Any>,
        _: Arc<dyn HttpClient>,
        _: PathBuf,
    ) -> Result<PathBuf> {
        unreachable!();
    }

    async fn cached_server_binary(&self, _: PathBuf) -> Option<PathBuf> {
        unreachable!();
    }

    async fn process_diagnostics(&self, _: &mut lsp::PublishDiagnosticsParams) {}

    async fn disk_based_diagnostic_sources(&self) -> Vec<String> {
        self.disk_based_diagnostics_sources.clone()
    }

    async fn disk_based_diagnostics_progress_token(&self) -> Option<String> {
        self.disk_based_diagnostics_progress_token.clone()
    }
}

fn get_capture_indices(query: &Query, captures: &mut [(&str, &mut Option<u32>)]) {
    for (ix, name) in query.capture_names().iter().enumerate() {
        for (capture_name, index) in captures.iter_mut() {
            if capture_name == name {
                **index = Some(ix as u32);
                break;
            }
        }
    }
}

pub fn point_to_lsp(point: PointUtf16) -> lsp::Position {
    lsp::Position::new(point.row, point.column)
}

pub fn point_from_lsp(point: lsp::Position) -> PointUtf16 {
    PointUtf16::new(point.line, point.character)
}

pub fn range_to_lsp(range: Range<PointUtf16>) -> lsp::Range {
    lsp::Range {
        start: point_to_lsp(range.start),
        end: point_to_lsp(range.end),
    }
}

pub fn range_from_lsp(range: lsp::Range) -> Range<PointUtf16> {
    let mut start = point_from_lsp(range.start);
    let mut end = point_from_lsp(range.end);
    if start > end {
        mem::swap(&mut start, &mut end);
    }
    start..end
}
