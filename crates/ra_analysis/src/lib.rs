//! ra_analyzer crate is the brain of Rust analyzer. It relies on the `salsa`
//! crate, which provides and incremental on-demand database of facts.

macro_rules! ctry {
    ($expr:expr) => {
        match $expr {
            None => return Ok(None),
            Some(it) => it,
        }
    };
}

mod db;
mod imp;
mod completion;
mod symbol_index;
pub mod mock_analysis;
mod runnables;

mod extend_selection;
mod syntax_highlighting;

use std::{fmt, sync::Arc};

use rustc_hash::FxHashMap;
use ra_syntax::{SourceFileNode, TextRange, TextUnit, SmolStr, SyntaxKind};
use ra_text_edit::TextEdit;
use rayon::prelude::*;
use relative_path::RelativePathBuf;

use crate::{
    imp::AnalysisImpl,
    symbol_index::{SymbolIndex, FileSymbol},
};

pub use crate::{
    completion::{CompletionItem, CompletionItemKind, InsertText},
    runnables::{Runnable, RunnableKind},
};
pub use ra_editor::{
    Fold, FoldKind, HighlightedRange, LineIndex, StructureNode, Severity
};
pub use hir::FnSignatureInfo;

pub use ra_db::{
    Canceled, Cancelable, FilePosition, FileRange,
    CrateGraph, CrateId, SourceRootId, FileId, SyntaxDatabase, FilesDatabase
};

#[derive(Default)]
pub struct AnalysisChange {
    new_roots: Vec<(SourceRootId, bool)>,
    roots_changed: FxHashMap<SourceRootId, RootChange>,
    files_changed: Vec<(FileId, Arc<String>)>,
    libraries_added: Vec<LibraryData>,
    crate_graph: Option<CrateGraph>,
}

#[derive(Default)]
struct RootChange {
    added: Vec<AddFile>,
    removed: Vec<RemoveFile>,
}

#[derive(Debug)]
struct AddFile {
    file_id: FileId,
    path: RelativePathBuf,
    text: Arc<String>,
}

#[derive(Debug)]
struct RemoveFile {
    file_id: FileId,
    path: RelativePathBuf,
}

impl fmt::Debug for AnalysisChange {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut d = fmt.debug_struct("AnalysisChange");
        if !self.new_roots.is_empty() {
            d.field("new_roots", &self.new_roots);
        }
        if !self.roots_changed.is_empty() {
            d.field("roots_changed", &self.roots_changed);
        }
        if !self.files_changed.is_empty() {
            d.field("files_changed", &self.files_changed.len());
        }
        if !self.libraries_added.is_empty() {
            d.field("libraries_added", &self.libraries_added.len());
        }
        if !self.crate_graph.is_some() {
            d.field("crate_graph", &self.crate_graph);
        }
        d.finish()
    }
}

impl fmt::Debug for RootChange {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("AnalysisChange")
            .field("added", &self.added.len())
            .field("removed", &self.removed.len())
            .finish()
    }
}

impl AnalysisChange {
    pub fn new() -> AnalysisChange {
        AnalysisChange::default()
    }
    pub fn add_root(&mut self, root_id: SourceRootId, is_local: bool) {
        self.new_roots.push((root_id, is_local));
    }
    pub fn add_file(
        &mut self,
        root_id: SourceRootId,
        file_id: FileId,
        path: RelativePathBuf,
        text: Arc<String>,
    ) {
        let file = AddFile {
            file_id,
            path,
            text,
        };
        self.roots_changed
            .entry(root_id)
            .or_default()
            .added
            .push(file);
    }
    pub fn change_file(&mut self, file_id: FileId, new_text: Arc<String>) {
        self.files_changed.push((file_id, new_text))
    }
    pub fn remove_file(&mut self, root_id: SourceRootId, file_id: FileId, path: RelativePathBuf) {
        let file = RemoveFile { file_id, path };
        self.roots_changed
            .entry(root_id)
            .or_default()
            .removed
            .push(file);
    }
    pub fn add_library(&mut self, data: LibraryData) {
        self.libraries_added.push(data)
    }
    pub fn set_crate_graph(&mut self, graph: CrateGraph) {
        self.crate_graph = Some(graph);
    }
}

/// `AnalysisHost` stores the current state of the world.
#[derive(Debug, Default)]
pub struct AnalysisHost {
    db: db::RootDatabase,
}

impl AnalysisHost {
    /// Returns a snapshot of the current state, which you can query for
    /// semantic information.
    pub fn analysis(&self) -> Analysis {
        Analysis {
            imp: self.db.analysis(),
        }
    }
    /// Applies changes to the current state of the world. If there are
    /// outstanding snapshots, they will be canceled.
    pub fn apply_change(&mut self, change: AnalysisChange) {
        self.db.apply_change(change)
    }
}

#[derive(Debug)]
pub struct SourceChange {
    pub label: String,
    pub source_file_edits: Vec<SourceFileEdit>,
    pub file_system_edits: Vec<FileSystemEdit>,
    pub cursor_position: Option<FilePosition>,
}

#[derive(Debug)]
pub struct SourceFileEdit {
    pub file_id: FileId,
    pub edit: TextEdit,
}

#[derive(Debug)]
pub enum FileSystemEdit {
    CreateFile {
        source_root: SourceRootId,
        path: RelativePathBuf,
    },
    MoveFile {
        src: FileId,
        dst_source_root: SourceRootId,
        dst_path: RelativePathBuf,
    },
}

#[derive(Debug)]
pub struct Diagnostic {
    pub message: String,
    pub range: TextRange,
    pub fix: Option<SourceChange>,
    pub severity: Severity,
}

#[derive(Debug)]
pub struct Query {
    query: String,
    lowercased: String,
    only_types: bool,
    libs: bool,
    exact: bool,
    limit: usize,
}

impl Query {
    pub fn new(query: String) -> Query {
        let lowercased = query.to_lowercase();
        Query {
            query,
            lowercased,
            only_types: false,
            libs: false,
            exact: false,
            limit: usize::max_value(),
        }
    }
    pub fn only_types(&mut self) {
        self.only_types = true;
    }
    pub fn libs(&mut self) {
        self.libs = true;
    }
    pub fn exact(&mut self) {
        self.exact = true;
    }
    pub fn limit(&mut self, limit: usize) {
        self.limit = limit
    }
}

#[derive(Debug)]
pub struct NavigationTarget {
    file_id: FileId,
    symbol: FileSymbol,
}

impl NavigationTarget {
    pub fn name(&self) -> SmolStr {
        self.symbol.name.clone()
    }
    pub fn kind(&self) -> SyntaxKind {
        self.symbol.kind
    }
    pub fn file_id(&self) -> FileId {
        self.file_id
    }
    pub fn range(&self) -> TextRange {
        self.symbol.node_range
    }
}

/// Result of "goto def" query.
#[derive(Debug)]
pub struct ReferenceResolution {
    /// The range of the reference itself. Client does not know what constitutes
    /// a reference, it handles us only the offset. It's helpful to tell the
    /// client where the reference was.
    pub reference_range: TextRange,
    /// What this reference resolves to.
    pub resolves_to: Vec<NavigationTarget>,
}

impl ReferenceResolution {
    fn new(reference_range: TextRange) -> ReferenceResolution {
        ReferenceResolution {
            reference_range,
            resolves_to: Vec::new(),
        }
    }

    fn add_resolution(&mut self, file_id: FileId, symbol: FileSymbol) {
        self.resolves_to.push(NavigationTarget { file_id, symbol })
    }
}

/// Analysis is a snapshot of a world state at a moment in time. It is the main
/// entry point for asking semantic information about the world. When the world
/// state is advanced using `AnalysisHost::apply_change` method, all existing
/// `Analysis` are canceled (most method return `Err(Canceled)`).
#[derive(Debug)]
pub struct Analysis {
    pub(crate) imp: AnalysisImpl,
}

impl Analysis {
    pub fn file_text(&self, file_id: FileId) -> Arc<String> {
        self.imp.db.file_text(file_id)
    }
    pub fn file_syntax(&self, file_id: FileId) -> SourceFileNode {
        self.imp.db.source_file(file_id).clone()
    }
    pub fn file_line_index(&self, file_id: FileId) -> Arc<LineIndex> {
        self.imp.db.file_lines(file_id)
    }
    pub fn extend_selection(&self, frange: FileRange) -> TextRange {
        extend_selection::extend_selection(&self.imp.db, frange)
    }
    pub fn matching_brace(&self, file: &SourceFileNode, offset: TextUnit) -> Option<TextUnit> {
        ra_editor::matching_brace(file, offset)
    }
    pub fn syntax_tree(&self, file_id: FileId) -> String {
        let file = self.imp.db.source_file(file_id);
        ra_editor::syntax_tree(&file)
    }
    pub fn join_lines(&self, frange: FileRange) -> SourceChange {
        let file = self.imp.db.source_file(frange.file_id);
        SourceChange::from_local_edit(frange.file_id, ra_editor::join_lines(&file, frange.range))
    }
    pub fn on_enter(&self, position: FilePosition) -> Option<SourceChange> {
        let file = self.imp.db.source_file(position.file_id);
        let edit = ra_editor::on_enter(&file, position.offset)?;
        let res = SourceChange::from_local_edit(position.file_id, edit);
        Some(res)
    }
    pub fn on_eq_typed(&self, position: FilePosition) -> Option<SourceChange> {
        let file = self.imp.db.source_file(position.file_id);
        Some(SourceChange::from_local_edit(
            position.file_id,
            ra_editor::on_eq_typed(&file, position.offset)?,
        ))
    }
    pub fn file_structure(&self, file_id: FileId) -> Vec<StructureNode> {
        let file = self.imp.db.source_file(file_id);
        ra_editor::file_structure(&file)
    }
    pub fn folding_ranges(&self, file_id: FileId) -> Vec<Fold> {
        let file = self.imp.db.source_file(file_id);
        ra_editor::folding_ranges(&file)
    }
    pub fn symbol_search(&self, query: Query) -> Cancelable<Vec<NavigationTarget>> {
        let res = symbol_index::world_symbols(&*self.imp.db, query)?
            .into_iter()
            .map(|(file_id, symbol)| NavigationTarget { file_id, symbol })
            .collect();
        Ok(res)
    }
    pub fn approximately_resolve_symbol(
        &self,
        position: FilePosition,
    ) -> Cancelable<Option<ReferenceResolution>> {
        self.imp.approximately_resolve_symbol(position)
    }
    pub fn find_all_refs(&self, position: FilePosition) -> Cancelable<Vec<(FileId, TextRange)>> {
        self.imp.find_all_refs(position)
    }
    pub fn doc_text_for(&self, nav: NavigationTarget) -> Cancelable<Option<String>> {
        self.imp.doc_text_for(nav)
    }
    pub fn parent_module(&self, position: FilePosition) -> Cancelable<Vec<NavigationTarget>> {
        self.imp.parent_module(position)
    }
    pub fn module_path(&self, position: FilePosition) -> Cancelable<Option<String>> {
        self.imp.module_path(position)
    }
    pub fn crate_for(&self, file_id: FileId) -> Cancelable<Vec<CrateId>> {
        self.imp.crate_for(file_id)
    }
    pub fn crate_root(&self, crate_id: CrateId) -> Cancelable<FileId> {
        Ok(self.imp.crate_root(crate_id))
    }
    pub fn runnables(&self, file_id: FileId) -> Cancelable<Vec<Runnable>> {
        let file = self.imp.db.source_file(file_id);
        Ok(runnables::runnables(self, &file, file_id))
    }
    pub fn highlight(&self, file_id: FileId) -> Cancelable<Vec<HighlightedRange>> {
        syntax_highlighting::highlight(&*self.imp.db, file_id)
    }
    pub fn completions(&self, position: FilePosition) -> Cancelable<Option<Vec<CompletionItem>>> {
        self.imp.completions(position)
    }
    pub fn assists(&self, frange: FileRange) -> Cancelable<Vec<SourceChange>> {
        Ok(self.imp.assists(frange))
    }
    pub fn diagnostics(&self, file_id: FileId) -> Cancelable<Vec<Diagnostic>> {
        self.imp.diagnostics(file_id)
    }
    pub fn resolve_callable(
        &self,
        position: FilePosition,
    ) -> Cancelable<Option<(FnSignatureInfo, Option<usize>)>> {
        self.imp.resolve_callable(position)
    }
    pub fn type_of(&self, frange: FileRange) -> Cancelable<Option<String>> {
        self.imp.type_of(frange)
    }
    pub fn rename(
        &self,
        position: FilePosition,
        new_name: &str,
    ) -> Cancelable<Vec<SourceFileEdit>> {
        self.imp.rename(position, new_name)
    }
}

pub struct LibraryData {
    root_id: SourceRootId,
    root_change: RootChange,
    symbol_index: SymbolIndex,
}

impl fmt::Debug for LibraryData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("LibraryData")
            .field("root_id", &self.root_id)
            .field("root_change", &self.root_change)
            .field("n_symbols", &self.symbol_index.len())
            .finish()
    }
}

impl LibraryData {
    pub fn prepare(
        root_id: SourceRootId,
        files: Vec<(FileId, RelativePathBuf, Arc<String>)>,
    ) -> LibraryData {
        let symbol_index = SymbolIndex::for_files(files.par_iter().map(|(file_id, _, text)| {
            let file = SourceFileNode::parse(text);
            (*file_id, file)
        }));
        let mut root_change = RootChange::default();
        root_change.added = files
            .into_iter()
            .map(|(file_id, path, text)| AddFile {
                file_id,
                path,
                text,
            })
            .collect();
        LibraryData {
            root_id,
            root_change,
            symbol_index,
        }
    }
}

#[test]
fn analysis_is_send() {
    fn is_send<T: Send>() {}
    is_send::<Analysis>();
}
