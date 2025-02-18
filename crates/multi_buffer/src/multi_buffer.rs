mod anchor;
#[cfg(test)]
mod multi_buffer_tests;

pub use anchor::{Anchor, AnchorRangeExt, Offset};
use anyhow::{anyhow, Result};
use clock::ReplicaId;
use collections::{BTreeMap, Bound, HashMap, HashSet};
use futures::{channel::mpsc, SinkExt};
use gpui::{AppContext, EntityId, EventEmitter, Model, ModelContext, Task};
use itertools::Itertools;
use language::{
    language_settings::{language_settings, LanguageSettings},
    AutoindentMode, Buffer, BufferChunks, BufferRow, BufferSnapshot, Capability, CharClassifier,
    CharKind, Chunk, CursorShape, DiagnosticEntry, DiskState, File, IndentGuide, IndentSize,
    Language, LanguageScope, OffsetRangeExt, OffsetUtf16, Outline, OutlineItem, Point, PointUtf16,
    Selection, TextDimension, ToOffset as _, ToOffsetUtf16 as _, ToPoint as _, ToPointUtf16 as _,
    TransactionId, Unclipped,
};
use smallvec::SmallVec;
use std::{
    any::type_name,
    borrow::Cow,
    cell::{Ref, RefCell},
    cmp, fmt,
    future::Future,
    io,
    iter::{self, FromIterator},
    mem,
    ops::{Range, RangeBounds, Sub},
    str,
    sync::Arc,
    time::{Duration, Instant},
};
use sum_tree::{Bias, Cursor, SumTree};
use text::{
    locator::Locator,
    subscription::{Subscription, Topic},
    BufferId, Edit, TextSummary,
};
use theme::SyntaxTheme;

use util::post_inc;

#[cfg(any(test, feature = "test-support"))]
use gpui::Context;

const NEWLINES: &[u8] = &[b'\n'; u8::MAX as usize];

#[derive(Debug, Default, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExcerptId(usize);

impl From<ExcerptId> for EntityId {
    fn from(id: ExcerptId) -> Self {
        EntityId::from(id.0 as u64)
    }
}

/// One or more [`Buffers`](Buffer) being edited in a single view.
///
/// See <https://zed.dev/features#multi-buffers>
pub struct MultiBuffer {
    /// A snapshot of the [`Excerpt`]s in the MultiBuffer.
    /// Use [`MultiBuffer::snapshot`] to get a up-to-date snapshot.
    snapshot: RefCell<MultiBufferSnapshot>,
    /// Contains the state of the buffers being edited
    buffers: RefCell<HashMap<BufferId, BufferState>>,
    subscriptions: Topic,
    /// If true, the multi-buffer only contains a single [`Buffer`] and a single [`Excerpt`]
    singleton: bool,
    history: History,
    title: Option<String>,
    capability: Capability,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    ExcerptsAdded {
        buffer: Model<Buffer>,
        predecessor: ExcerptId,
        excerpts: Vec<(ExcerptId, ExcerptRange<language::Anchor>)>,
    },
    ExcerptsRemoved {
        ids: Vec<ExcerptId>,
    },
    ExcerptsExpanded {
        ids: Vec<ExcerptId>,
    },
    ExcerptsEdited {
        ids: Vec<ExcerptId>,
    },
    Edited {
        singleton_buffer_edited: bool,
        edited_buffer: Option<Model<Buffer>>,
    },
    TransactionUndone {
        transaction_id: TransactionId,
    },
    Reloaded,
    ReloadNeeded,

    LanguageChanged(BufferId),
    CapabilityChanged,
    Reparsed(BufferId),
    Saved,
    FileHandleChanged,
    Closed,
    Discarded,
    DirtyChanged,
    DiagnosticsUpdated,
}

/// A diff hunk, representing a range of consequent lines in a multibuffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiBufferDiffHunk {
    /// The row range in the multibuffer where this diff hunk appears.
    pub row_range: Range<MultiBufferRow>,
    /// The buffer ID that this hunk belongs to.
    pub buffer_id: BufferId,
    /// The range of the underlying buffer that this hunk corresponds to.
    pub buffer_range: Range<text::Anchor>,
    /// The range within the buffer's diff base that this hunk corresponds to.
    pub diff_base_byte_range: Range<usize>,
}

pub type MultiBufferPoint = Point;

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq, Hash, serde::Deserialize)]
#[serde(transparent)]
pub struct MultiBufferRow(pub u32);

impl MultiBufferRow {
    pub const MIN: Self = Self(0);
    pub const MAX: Self = Self(u32::MAX);
}

#[derive(Clone)]
struct History {
    next_transaction_id: TransactionId,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    transaction_depth: usize,
    group_interval: Duration,
}

#[derive(Clone)]
struct Transaction {
    id: TransactionId,
    buffer_transactions: HashMap<BufferId, text::TransactionId>,
    first_edit_at: Instant,
    last_edit_at: Instant,
    suppress_grouping: bool,
}

pub trait ToOffset: 'static + fmt::Debug {
    fn to_offset(&self, snapshot: &MultiBufferSnapshot) -> usize;
}

pub trait ToOffsetUtf16: 'static + fmt::Debug {
    fn to_offset_utf16(&self, snapshot: &MultiBufferSnapshot) -> OffsetUtf16;
}

pub trait ToPoint: 'static + fmt::Debug {
    fn to_point(&self, snapshot: &MultiBufferSnapshot) -> Point;
}

pub trait ToPointUtf16: 'static + fmt::Debug {
    fn to_point_utf16(&self, snapshot: &MultiBufferSnapshot) -> PointUtf16;
}

struct BufferState {
    buffer: Model<Buffer>,
    last_version: clock::Global,
    last_non_text_state_update_count: usize,
    excerpts: Vec<Locator>,
    _subscriptions: [gpui::Subscription; 2],
}

/// The contents of a [`MultiBuffer`] at a single point in time.
#[derive(Clone, Default)]
pub struct MultiBufferSnapshot {
    singleton: bool,
    excerpts: SumTree<Excerpt>,
    excerpt_ids: SumTree<ExcerptIdMapping>,
    trailing_excerpt_update_count: usize,
    non_text_state_update_count: usize,
    edit_count: usize,
    is_dirty: bool,
    has_deleted_file: bool,
    has_conflict: bool,
    show_headers: bool,
}

#[derive(Clone)]
pub struct ExcerptInfo {
    pub id: ExcerptId,
    pub buffer: BufferSnapshot,
    pub buffer_id: BufferId,
    pub range: ExcerptRange<text::Anchor>,
    pub text_summary: TextSummary,
}

impl std::fmt::Debug for ExcerptInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(type_name::<Self>())
            .field("id", &self.id)
            .field("buffer_id", &self.buffer_id)
            .field("path", &self.buffer.file().map(|f| f.path()))
            .field("range", &self.range)
            .finish()
    }
}

/// A boundary between [`Excerpt`]s in a [`MultiBuffer`]
#[derive(Debug)]
pub struct ExcerptBoundary {
    pub prev: Option<ExcerptInfo>,
    pub next: Option<ExcerptInfo>,
    /// The row in the `MultiBuffer` where the boundary is located
    pub row: MultiBufferRow,
}

impl ExcerptBoundary {
    pub fn starts_new_buffer(&self) -> bool {
        match (self.prev.as_ref(), self.next.as_ref()) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(prev), Some(next)) => prev.buffer_id != next.buffer_id,
        }
    }
}

/// A slice into a [`Buffer`] that is being edited in a [`MultiBuffer`].
#[derive(Clone)]
struct Excerpt {
    /// The unique identifier for this excerpt
    id: ExcerptId,
    /// The location of the excerpt in the [`MultiBuffer`]
    locator: Locator,
    /// The buffer being excerpted
    buffer_id: BufferId,
    /// A snapshot of the buffer being excerpted
    buffer: BufferSnapshot,
    /// The range of the buffer to be shown in the excerpt
    range: ExcerptRange<text::Anchor>,
    /// The last row in the excerpted slice of the buffer
    max_buffer_row: BufferRow,
    /// A summary of the text in the excerpt
    text_summary: TextSummary,
    has_trailing_newline: bool,
}

/// A public view into an [`Excerpt`] in a [`MultiBuffer`].
///
/// Contains methods for getting the [`Buffer`] of the excerpt,
/// as well as mapping offsets to/from buffer and multibuffer coordinates.
#[derive(Clone)]
pub struct MultiBufferExcerpt<'a> {
    excerpt: &'a Excerpt,
    excerpt_offset: usize,
    excerpt_position: Point,
}

#[derive(Clone, Debug)]
struct ExcerptIdMapping {
    id: ExcerptId,
    locator: Locator,
}

/// A range of text from a single [`Buffer`], to be shown as an [`Excerpt`].
/// These ranges are relative to the buffer itself
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExcerptRange<T> {
    /// The full range of text to be shown in the excerpt.
    pub context: Range<T>,
    /// The primary range of text to be highlighted in the excerpt.
    /// In a multi-buffer search, this would be the text that matched the search
    pub primary: Option<Range<T>>,
}

#[derive(Clone, Debug, Default)]
pub struct ExcerptSummary {
    excerpt_id: ExcerptId,
    /// The location of the last [`Excerpt`] being summarized
    excerpt_locator: Locator,
    widest_line_number: u32,
    text: TextSummary,
}

#[derive(Clone)]
pub struct MultiBufferRows<'a> {
    buffer_row_range: Range<u32>,
    excerpts: Cursor<'a, Excerpt, Point>,
}

pub struct MultiBufferChunks<'a> {
    range: Range<usize>,
    excerpts: Cursor<'a, Excerpt, usize>,
    excerpt_chunks: Option<ExcerptChunks<'a>>,
    language_aware: bool,
}

pub struct MultiBufferBytes<'a> {
    range: Range<usize>,
    excerpts: Cursor<'a, Excerpt, usize>,
    excerpt_bytes: Option<ExcerptBytes<'a>>,
    chunk: &'a [u8],
}

pub struct ReversedMultiBufferBytes<'a> {
    range: Range<usize>,
    excerpts: Cursor<'a, Excerpt, usize>,
    excerpt_bytes: Option<ExcerptBytes<'a>>,
    chunk: &'a [u8],
}

struct ExcerptChunks<'a> {
    excerpt_id: ExcerptId,
    content_chunks: BufferChunks<'a>,
    footer_height: usize,
}

struct ExcerptBytes<'a> {
    content_bytes: text::Bytes<'a>,
    padding_height: usize,
    reversed: bool,
}

struct BufferEdit {
    range: Range<usize>,
    new_text: Arc<str>,
    is_insertion: bool,
    original_indent_column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpandExcerptDirection {
    Up,
    Down,
    UpAndDown,
}

impl ExpandExcerptDirection {
    pub fn should_expand_up(&self) -> bool {
        match self {
            ExpandExcerptDirection::Up => true,
            ExpandExcerptDirection::Down => false,
            ExpandExcerptDirection::UpAndDown => true,
        }
    }

    pub fn should_expand_down(&self) -> bool {
        match self {
            ExpandExcerptDirection::Up => false,
            ExpandExcerptDirection::Down => true,
            ExpandExcerptDirection::UpAndDown => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MultiBufferIndentGuide {
    pub multibuffer_row_range: Range<MultiBufferRow>,
    pub buffer: IndentGuide,
}

impl std::ops::Deref for MultiBufferIndentGuide {
    type Target = IndentGuide;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl MultiBuffer {
    pub fn new(capability: Capability) -> Self {
        Self {
            snapshot: RefCell::new(MultiBufferSnapshot {
                show_headers: true,
                ..MultiBufferSnapshot::default()
            }),
            buffers: RefCell::default(),
            subscriptions: Topic::default(),
            singleton: false,
            capability,
            title: None,
            history: History {
                next_transaction_id: clock::Lamport::default(),
                undo_stack: Vec::new(),
                redo_stack: Vec::new(),
                transaction_depth: 0,
                group_interval: Duration::from_millis(300),
            },
        }
    }

    pub fn without_headers(capability: Capability) -> Self {
        Self {
            snapshot: Default::default(),
            buffers: Default::default(),
            subscriptions: Default::default(),
            singleton: false,
            capability,
            history: History {
                next_transaction_id: Default::default(),
                undo_stack: Default::default(),
                redo_stack: Default::default(),
                transaction_depth: 0,
                group_interval: Duration::from_millis(300),
            },
            title: Default::default(),
        }
    }

    pub fn clone(&self, new_cx: &mut ModelContext<Self>) -> Self {
        let mut buffers = HashMap::default();
        for (buffer_id, buffer_state) in self.buffers.borrow().iter() {
            buffers.insert(
                *buffer_id,
                BufferState {
                    buffer: buffer_state.buffer.clone(),
                    last_version: buffer_state.last_version.clone(),
                    last_non_text_state_update_count: buffer_state.last_non_text_state_update_count,
                    excerpts: buffer_state.excerpts.clone(),
                    _subscriptions: [
                        new_cx.observe(&buffer_state.buffer, |_, _, cx| cx.notify()),
                        new_cx.subscribe(&buffer_state.buffer, Self::on_buffer_event),
                    ],
                },
            );
        }
        Self {
            snapshot: RefCell::new(self.snapshot.borrow().clone()),
            buffers: RefCell::new(buffers),
            subscriptions: Default::default(),
            singleton: self.singleton,
            capability: self.capability,
            history: self.history.clone(),
            title: self.title.clone(),
        }
    }

    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    pub fn read_only(&self) -> bool {
        self.capability == Capability::ReadOnly
    }

    pub fn singleton(buffer: Model<Buffer>, cx: &mut ModelContext<Self>) -> Self {
        let mut this = Self::new(buffer.read(cx).capability());
        this.singleton = true;
        this.push_excerpts(
            buffer,
            [ExcerptRange {
                context: text::Anchor::MIN..text::Anchor::MAX,
                primary: None,
            }],
            cx,
        );
        this.snapshot.borrow_mut().singleton = true;
        this
    }

    /// Returns an up-to-date snapshot of the MultiBuffer.
    pub fn snapshot(&self, cx: &AppContext) -> MultiBufferSnapshot {
        self.sync(cx);
        self.snapshot.borrow().clone()
    }

    pub fn read(&self, cx: &AppContext) -> Ref<MultiBufferSnapshot> {
        self.sync(cx);
        self.snapshot.borrow()
    }

    pub fn as_singleton(&self) -> Option<Model<Buffer>> {
        if self.singleton {
            return Some(
                self.buffers
                    .borrow()
                    .values()
                    .next()
                    .unwrap()
                    .buffer
                    .clone(),
            );
        } else {
            None
        }
    }

    pub fn is_singleton(&self) -> bool {
        self.singleton
    }

    pub fn subscribe(&mut self) -> Subscription {
        self.subscriptions.subscribe()
    }

    pub fn is_dirty(&self, cx: &AppContext) -> bool {
        self.read(cx).is_dirty()
    }

    pub fn has_deleted_file(&self, cx: &AppContext) -> bool {
        self.read(cx).has_deleted_file()
    }

    pub fn has_conflict(&self, cx: &AppContext) -> bool {
        self.read(cx).has_conflict()
    }

    // The `is_empty` signature doesn't match what clippy expects
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self, cx: &AppContext) -> usize {
        self.read(cx).len()
    }

    pub fn is_empty(&self, cx: &AppContext) -> bool {
        self.len(cx) != 0
    }

    pub fn symbols_containing<T: ToOffset>(
        &self,
        offset: T,
        theme: Option<&SyntaxTheme>,
        cx: &AppContext,
    ) -> Option<(BufferId, Vec<OutlineItem<Anchor>>)> {
        self.read(cx).symbols_containing(offset, theme)
    }

    pub fn edit<I, S, T>(
        &self,
        edits: I,
        autoindent_mode: Option<AutoindentMode>,
        cx: &mut ModelContext<Self>,
    ) where
        I: IntoIterator<Item = (Range<S>, T)>,
        S: ToOffset,
        T: Into<Arc<str>>,
    {
        let snapshot = self.read(cx);
        let edits = edits
            .into_iter()
            .map(|(range, new_text)| {
                let mut range = range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot);
                if range.start > range.end {
                    mem::swap(&mut range.start, &mut range.end);
                }
                (range, new_text.into())
            })
            .collect::<Vec<_>>();

        return edit_internal(self, snapshot, edits, autoindent_mode, cx);

        // Non-generic part of edit, hoisted out to avoid blowing up LLVM IR.
        fn edit_internal(
            this: &MultiBuffer,
            snapshot: Ref<MultiBufferSnapshot>,
            edits: Vec<(Range<usize>, Arc<str>)>,
            mut autoindent_mode: Option<AutoindentMode>,
            cx: &mut ModelContext<MultiBuffer>,
        ) {
            if this.read_only() || this.buffers.borrow().is_empty() {
                return;
            }

            if let Some(buffer) = this.as_singleton() {
                buffer.update(cx, |buffer, cx| {
                    buffer.edit(edits, autoindent_mode, cx);
                });
                cx.emit(Event::ExcerptsEdited {
                    ids: this.excerpt_ids(),
                });
                return;
            }

            let original_indent_columns = match &mut autoindent_mode {
                Some(AutoindentMode::Block {
                    original_indent_columns,
                }) => mem::take(original_indent_columns),
                _ => Default::default(),
            };

            let (buffer_edits, edited_excerpt_ids) =
                this.convert_edits_to_buffer_edits(edits, &snapshot, &original_indent_columns);
            drop(snapshot);

            for (buffer_id, mut edits) in buffer_edits {
                edits.sort_unstable_by_key(|edit| edit.range.start);
                this.buffers.borrow()[&buffer_id]
                    .buffer
                    .update(cx, |buffer, cx| {
                        let mut edits = edits.into_iter().peekable();
                        let mut insertions = Vec::new();
                        let mut original_indent_columns = Vec::new();
                        let mut deletions = Vec::new();
                        let empty_str: Arc<str> = Arc::default();
                        while let Some(BufferEdit {
                            mut range,
                            new_text,
                            mut is_insertion,
                            original_indent_column,
                        }) = edits.next()
                        {
                            while let Some(BufferEdit {
                                range: next_range,
                                is_insertion: next_is_insertion,
                                ..
                            }) = edits.peek()
                            {
                                if range.end >= next_range.start {
                                    range.end = cmp::max(next_range.end, range.end);
                                    is_insertion |= *next_is_insertion;
                                    edits.next();
                                } else {
                                    break;
                                }
                            }

                            if is_insertion {
                                original_indent_columns.push(original_indent_column);
                                insertions.push((
                                    buffer.anchor_before(range.start)
                                        ..buffer.anchor_before(range.end),
                                    new_text.clone(),
                                ));
                            } else if !range.is_empty() {
                                deletions.push((
                                    buffer.anchor_before(range.start)
                                        ..buffer.anchor_before(range.end),
                                    empty_str.clone(),
                                ));
                            }
                        }

                        let deletion_autoindent_mode =
                            if let Some(AutoindentMode::Block { .. }) = autoindent_mode {
                                Some(AutoindentMode::Block {
                                    original_indent_columns: Default::default(),
                                })
                            } else {
                                autoindent_mode.clone()
                            };
                        let insertion_autoindent_mode =
                            if let Some(AutoindentMode::Block { .. }) = autoindent_mode {
                                Some(AutoindentMode::Block {
                                    original_indent_columns,
                                })
                            } else {
                                autoindent_mode.clone()
                            };

                        buffer.edit(deletions, deletion_autoindent_mode, cx);
                        buffer.edit(insertions, insertion_autoindent_mode, cx);
                    })
            }

            cx.emit(Event::ExcerptsEdited {
                ids: edited_excerpt_ids,
            });
        }
    }

    fn convert_edits_to_buffer_edits(
        &self,
        edits: Vec<(Range<usize>, Arc<str>)>,
        snapshot: &MultiBufferSnapshot,
        original_indent_columns: &[u32],
    ) -> (HashMap<BufferId, Vec<BufferEdit>>, Vec<ExcerptId>) {
        let mut buffer_edits: HashMap<BufferId, Vec<BufferEdit>> = Default::default();
        let mut edited_excerpt_ids = Vec::new();
        let mut cursor = snapshot.excerpts.cursor::<usize>(&());
        for (ix, (range, new_text)) in edits.into_iter().enumerate() {
            let original_indent_column = original_indent_columns.get(ix).copied().unwrap_or(0);
            cursor.seek(&range.start, Bias::Right, &());
            if cursor.item().is_none() && range.start == *cursor.start() {
                cursor.prev(&());
            }
            let start_excerpt = cursor.item().expect("start offset out of bounds");
            let start_overshoot = range.start - cursor.start();
            let buffer_start = start_excerpt
                .range
                .context
                .start
                .to_offset(&start_excerpt.buffer)
                + start_overshoot;
            edited_excerpt_ids.push(start_excerpt.id);

            cursor.seek(&range.end, Bias::Right, &());
            if cursor.item().is_none() && range.end == *cursor.start() {
                cursor.prev(&());
            }
            let end_excerpt = cursor.item().expect("end offset out of bounds");
            let end_overshoot = range.end - cursor.start();
            let buffer_end = end_excerpt
                .range
                .context
                .start
                .to_offset(&end_excerpt.buffer)
                + end_overshoot;

            if start_excerpt.id == end_excerpt.id {
                buffer_edits
                    .entry(start_excerpt.buffer_id)
                    .or_default()
                    .push(BufferEdit {
                        range: buffer_start..buffer_end,
                        new_text,
                        is_insertion: true,
                        original_indent_column,
                    });
            } else {
                edited_excerpt_ids.push(end_excerpt.id);
                let start_excerpt_range = buffer_start
                    ..start_excerpt
                        .range
                        .context
                        .end
                        .to_offset(&start_excerpt.buffer);
                let end_excerpt_range = end_excerpt
                    .range
                    .context
                    .start
                    .to_offset(&end_excerpt.buffer)
                    ..buffer_end;
                buffer_edits
                    .entry(start_excerpt.buffer_id)
                    .or_default()
                    .push(BufferEdit {
                        range: start_excerpt_range,
                        new_text: new_text.clone(),
                        is_insertion: true,
                        original_indent_column,
                    });
                buffer_edits
                    .entry(end_excerpt.buffer_id)
                    .or_default()
                    .push(BufferEdit {
                        range: end_excerpt_range,
                        new_text: new_text.clone(),
                        is_insertion: false,
                        original_indent_column,
                    });

                cursor.seek(&range.start, Bias::Right, &());
                cursor.next(&());
                while let Some(excerpt) = cursor.item() {
                    if excerpt.id == end_excerpt.id {
                        break;
                    }
                    buffer_edits
                        .entry(excerpt.buffer_id)
                        .or_default()
                        .push(BufferEdit {
                            range: excerpt.range.context.to_offset(&excerpt.buffer),
                            new_text: new_text.clone(),
                            is_insertion: false,
                            original_indent_column,
                        });
                    edited_excerpt_ids.push(excerpt.id);
                    cursor.next(&());
                }
            }
        }
        (buffer_edits, edited_excerpt_ids)
    }

    pub fn autoindent_ranges<I, S>(&self, ranges: I, cx: &mut ModelContext<Self>)
    where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
    {
        let snapshot = self.read(cx);
        let empty = Arc::<str>::from("");
        let edits = ranges
            .into_iter()
            .map(|range| {
                let mut range = range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot);
                if range.start > range.end {
                    mem::swap(&mut range.start, &mut range.end);
                }
                (range, empty.clone())
            })
            .collect::<Vec<_>>();

        return autoindent_ranges_internal(self, snapshot, edits, cx);

        fn autoindent_ranges_internal(
            this: &MultiBuffer,
            snapshot: Ref<MultiBufferSnapshot>,
            edits: Vec<(Range<usize>, Arc<str>)>,
            cx: &mut ModelContext<MultiBuffer>,
        ) {
            if this.read_only() || this.buffers.borrow().is_empty() {
                return;
            }

            if let Some(buffer) = this.as_singleton() {
                buffer.update(cx, |buffer, cx| {
                    buffer.autoindent_ranges(edits.into_iter().map(|e| e.0), cx);
                });
                cx.emit(Event::ExcerptsEdited {
                    ids: this.excerpt_ids(),
                });
                return;
            }

            let (buffer_edits, edited_excerpt_ids) =
                this.convert_edits_to_buffer_edits(edits, &snapshot, &[]);
            drop(snapshot);

            for (buffer_id, mut edits) in buffer_edits {
                edits.sort_unstable_by_key(|edit| edit.range.start);

                let mut ranges: Vec<Range<usize>> = Vec::new();
                for edit in edits {
                    if let Some(last_range) = ranges.last_mut() {
                        if edit.range.start <= last_range.end {
                            last_range.end = last_range.end.max(edit.range.end);
                            continue;
                        }
                    }
                    ranges.push(edit.range);
                }

                this.buffers.borrow()[&buffer_id]
                    .buffer
                    .update(cx, |buffer, cx| {
                        buffer.autoindent_ranges(ranges, cx);
                    })
            }

            cx.emit(Event::ExcerptsEdited {
                ids: edited_excerpt_ids,
            });
        }
    }

    // Inserts newlines at the given position to create an empty line, returning the start of the new line.
    // You can also request the insertion of empty lines above and below the line starting at the returned point.
    // Panics if the given position is invalid.
    pub fn insert_empty_line(
        &mut self,
        position: impl ToPoint,
        space_above: bool,
        space_below: bool,
        cx: &mut ModelContext<Self>,
    ) -> Point {
        let multibuffer_point = position.to_point(&self.read(cx));
        if let Some(buffer) = self.as_singleton() {
            buffer.update(cx, |buffer, cx| {
                buffer.insert_empty_line(multibuffer_point, space_above, space_below, cx)
            })
        } else {
            let (buffer, buffer_point, _) =
                self.point_to_buffer_point(multibuffer_point, cx).unwrap();
            self.start_transaction(cx);
            let empty_line_start = buffer.update(cx, |buffer, cx| {
                buffer.insert_empty_line(buffer_point, space_above, space_below, cx)
            });
            self.end_transaction(cx);
            multibuffer_point + (empty_line_start - buffer_point)
        }
    }

    pub fn start_transaction(&mut self, cx: &mut ModelContext<Self>) -> Option<TransactionId> {
        self.start_transaction_at(Instant::now(), cx)
    }

    pub fn start_transaction_at(
        &mut self,
        now: Instant,
        cx: &mut ModelContext<Self>,
    ) -> Option<TransactionId> {
        if let Some(buffer) = self.as_singleton() {
            return buffer.update(cx, |buffer, _| buffer.start_transaction_at(now));
        }

        for BufferState { buffer, .. } in self.buffers.borrow().values() {
            buffer.update(cx, |buffer, _| buffer.start_transaction_at(now));
        }
        self.history.start_transaction(now)
    }

    pub fn end_transaction(&mut self, cx: &mut ModelContext<Self>) -> Option<TransactionId> {
        self.end_transaction_at(Instant::now(), cx)
    }

    pub fn end_transaction_at(
        &mut self,
        now: Instant,
        cx: &mut ModelContext<Self>,
    ) -> Option<TransactionId> {
        if let Some(buffer) = self.as_singleton() {
            return buffer.update(cx, |buffer, cx| buffer.end_transaction_at(now, cx));
        }

        let mut buffer_transactions = HashMap::default();
        for BufferState { buffer, .. } in self.buffers.borrow().values() {
            if let Some(transaction_id) =
                buffer.update(cx, |buffer, cx| buffer.end_transaction_at(now, cx))
            {
                buffer_transactions.insert(buffer.read(cx).remote_id(), transaction_id);
            }
        }

        if self.history.end_transaction(now, buffer_transactions) {
            let transaction_id = self.history.group().unwrap();
            Some(transaction_id)
        } else {
            None
        }
    }

    pub fn edited_ranges_for_transaction<D>(
        &self,
        transaction_id: TransactionId,
        cx: &AppContext,
    ) -> Vec<Range<D>>
    where
        D: TextDimension + Ord + Sub<D, Output = D>,
    {
        if let Some(buffer) = self.as_singleton() {
            return buffer
                .read(cx)
                .edited_ranges_for_transaction_id(transaction_id)
                .collect::<Vec<_>>();
        }

        let Some(transaction) = self.history.transaction(transaction_id) else {
            return Vec::new();
        };

        let mut ranges = Vec::new();
        let snapshot = self.read(cx);
        let buffers = self.buffers.borrow();
        let mut cursor = snapshot.excerpts.cursor::<ExcerptSummary>(&());

        for (buffer_id, buffer_transaction) in &transaction.buffer_transactions {
            let Some(buffer_state) = buffers.get(buffer_id) else {
                continue;
            };

            let buffer = buffer_state.buffer.read(cx);
            for range in buffer.edited_ranges_for_transaction_id::<D>(*buffer_transaction) {
                for excerpt_id in &buffer_state.excerpts {
                    cursor.seek(excerpt_id, Bias::Left, &());
                    if let Some(excerpt) = cursor.item() {
                        if excerpt.locator == *excerpt_id {
                            let excerpt_buffer_start =
                                excerpt.range.context.start.summary::<D>(buffer);
                            let excerpt_buffer_end = excerpt.range.context.end.summary::<D>(buffer);
                            let excerpt_range = excerpt_buffer_start.clone()..excerpt_buffer_end;
                            if excerpt_range.contains(&range.start)
                                && excerpt_range.contains(&range.end)
                            {
                                let excerpt_start = D::from_text_summary(&cursor.start().text);

                                let mut start = excerpt_start.clone();
                                start.add_assign(&(range.start - excerpt_buffer_start.clone()));
                                let mut end = excerpt_start;
                                end.add_assign(&(range.end - excerpt_buffer_start));

                                ranges.push(start..end);
                                break;
                            }
                        }
                    }
                }
            }
        }

        ranges.sort_by_key(|range| range.start.clone());
        ranges
    }

    pub fn merge_transactions(
        &mut self,
        transaction: TransactionId,
        destination: TransactionId,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(buffer) = self.as_singleton() {
            buffer.update(cx, |buffer, _| {
                buffer.merge_transactions(transaction, destination)
            });
        } else if let Some(transaction) = self.history.forget(transaction) {
            if let Some(destination) = self.history.transaction_mut(destination) {
                for (buffer_id, buffer_transaction_id) in transaction.buffer_transactions {
                    if let Some(destination_buffer_transaction_id) =
                        destination.buffer_transactions.get(&buffer_id)
                    {
                        if let Some(state) = self.buffers.borrow().get(&buffer_id) {
                            state.buffer.update(cx, |buffer, _| {
                                buffer.merge_transactions(
                                    buffer_transaction_id,
                                    *destination_buffer_transaction_id,
                                )
                            });
                        }
                    } else {
                        destination
                            .buffer_transactions
                            .insert(buffer_id, buffer_transaction_id);
                    }
                }
            }
        }
    }

    pub fn finalize_last_transaction(&mut self, cx: &mut ModelContext<Self>) {
        self.history.finalize_last_transaction();
        for BufferState { buffer, .. } in self.buffers.borrow().values() {
            buffer.update(cx, |buffer, _| {
                buffer.finalize_last_transaction();
            });
        }
    }

    pub fn push_transaction<'a, T>(&mut self, buffer_transactions: T, cx: &ModelContext<Self>)
    where
        T: IntoIterator<Item = (&'a Model<Buffer>, &'a language::Transaction)>,
    {
        self.history
            .push_transaction(buffer_transactions, Instant::now(), cx);
        self.history.finalize_last_transaction();
    }

    pub fn group_until_transaction(
        &mut self,
        transaction_id: TransactionId,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(buffer) = self.as_singleton() {
            buffer.update(cx, |buffer, _| {
                buffer.group_until_transaction(transaction_id)
            });
        } else {
            self.history.group_until(transaction_id);
        }
    }

    pub fn set_active_selections(
        &self,
        selections: &[Selection<Anchor>],
        line_mode: bool,
        cursor_shape: CursorShape,
        cx: &mut ModelContext<Self>,
    ) {
        let mut selections_by_buffer: HashMap<BufferId, Vec<Selection<text::Anchor>>> =
            Default::default();
        let snapshot = self.read(cx);
        let mut cursor = snapshot.excerpts.cursor::<Option<&Locator>>(&());
        for selection in selections {
            let start_locator = snapshot.excerpt_locator_for_id(selection.start.excerpt_id);
            let end_locator = snapshot.excerpt_locator_for_id(selection.end.excerpt_id);

            cursor.seek(&Some(start_locator), Bias::Left, &());
            while let Some(excerpt) = cursor.item() {
                if excerpt.locator > *end_locator {
                    break;
                }

                let mut start = excerpt.range.context.start;
                let mut end = excerpt.range.context.end;
                if excerpt.id == selection.start.excerpt_id {
                    start = selection.start.text_anchor;
                }
                if excerpt.id == selection.end.excerpt_id {
                    end = selection.end.text_anchor;
                }
                selections_by_buffer
                    .entry(excerpt.buffer_id)
                    .or_default()
                    .push(Selection {
                        id: selection.id,
                        start,
                        end,
                        reversed: selection.reversed,
                        goal: selection.goal,
                    });

                cursor.next(&());
            }
        }

        for (buffer_id, buffer_state) in self.buffers.borrow().iter() {
            if !selections_by_buffer.contains_key(buffer_id) {
                buffer_state
                    .buffer
                    .update(cx, |buffer, cx| buffer.remove_active_selections(cx));
            }
        }

        for (buffer_id, mut selections) in selections_by_buffer {
            self.buffers.borrow()[&buffer_id]
                .buffer
                .update(cx, |buffer, cx| {
                    selections.sort_unstable_by(|a, b| a.start.cmp(&b.start, buffer));
                    let mut selections = selections.into_iter().peekable();
                    let merged_selections = Arc::from_iter(iter::from_fn(|| {
                        let mut selection = selections.next()?;
                        while let Some(next_selection) = selections.peek() {
                            if selection.end.cmp(&next_selection.start, buffer).is_ge() {
                                let next_selection = selections.next().unwrap();
                                if next_selection.end.cmp(&selection.end, buffer).is_ge() {
                                    selection.end = next_selection.end;
                                }
                            } else {
                                break;
                            }
                        }
                        Some(selection)
                    }));
                    buffer.set_active_selections(merged_selections, line_mode, cursor_shape, cx);
                });
        }
    }

    pub fn remove_active_selections(&self, cx: &mut ModelContext<Self>) {
        for buffer in self.buffers.borrow().values() {
            buffer
                .buffer
                .update(cx, |buffer, cx| buffer.remove_active_selections(cx));
        }
    }

    pub fn undo(&mut self, cx: &mut ModelContext<Self>) -> Option<TransactionId> {
        let mut transaction_id = None;
        if let Some(buffer) = self.as_singleton() {
            transaction_id = buffer.update(cx, |buffer, cx| buffer.undo(cx));
        } else {
            while let Some(transaction) = self.history.pop_undo() {
                let mut undone = false;
                for (buffer_id, buffer_transaction_id) in &mut transaction.buffer_transactions {
                    if let Some(BufferState { buffer, .. }) = self.buffers.borrow().get(buffer_id) {
                        undone |= buffer.update(cx, |buffer, cx| {
                            let undo_to = *buffer_transaction_id;
                            if let Some(entry) = buffer.peek_undo_stack() {
                                *buffer_transaction_id = entry.transaction_id();
                            }
                            buffer.undo_to_transaction(undo_to, cx)
                        });
                    }
                }

                if undone {
                    transaction_id = Some(transaction.id);
                    break;
                }
            }
        }

        if let Some(transaction_id) = transaction_id {
            cx.emit(Event::TransactionUndone { transaction_id });
        }

        transaction_id
    }

    pub fn redo(&mut self, cx: &mut ModelContext<Self>) -> Option<TransactionId> {
        if let Some(buffer) = self.as_singleton() {
            return buffer.update(cx, |buffer, cx| buffer.redo(cx));
        }

        while let Some(transaction) = self.history.pop_redo() {
            let mut redone = false;
            for (buffer_id, buffer_transaction_id) in &mut transaction.buffer_transactions {
                if let Some(BufferState { buffer, .. }) = self.buffers.borrow().get(buffer_id) {
                    redone |= buffer.update(cx, |buffer, cx| {
                        let redo_to = *buffer_transaction_id;
                        if let Some(entry) = buffer.peek_redo_stack() {
                            *buffer_transaction_id = entry.transaction_id();
                        }
                        buffer.redo_to_transaction(redo_to, cx)
                    });
                }
            }

            if redone {
                return Some(transaction.id);
            }
        }

        None
    }

    pub fn undo_transaction(&mut self, transaction_id: TransactionId, cx: &mut ModelContext<Self>) {
        if let Some(buffer) = self.as_singleton() {
            buffer.update(cx, |buffer, cx| buffer.undo_transaction(transaction_id, cx));
        } else if let Some(transaction) = self.history.remove_from_undo(transaction_id) {
            for (buffer_id, transaction_id) in &transaction.buffer_transactions {
                if let Some(BufferState { buffer, .. }) = self.buffers.borrow().get(buffer_id) {
                    buffer.update(cx, |buffer, cx| {
                        buffer.undo_transaction(*transaction_id, cx)
                    });
                }
            }
        }
    }

    pub fn forget_transaction(
        &mut self,
        transaction_id: TransactionId,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(buffer) = self.as_singleton() {
            buffer.update(cx, |buffer, _| {
                buffer.forget_transaction(transaction_id);
            });
        } else if let Some(transaction) = self.history.forget(transaction_id) {
            for (buffer_id, buffer_transaction_id) in transaction.buffer_transactions {
                if let Some(state) = self.buffers.borrow_mut().get_mut(&buffer_id) {
                    state.buffer.update(cx, |buffer, _| {
                        buffer.forget_transaction(buffer_transaction_id);
                    });
                }
            }
        }
    }

    pub fn push_excerpts<O>(
        &mut self,
        buffer: Model<Buffer>,
        ranges: impl IntoIterator<Item = ExcerptRange<O>>,
        cx: &mut ModelContext<Self>,
    ) -> Vec<ExcerptId>
    where
        O: text::ToOffset,
    {
        self.insert_excerpts_after(ExcerptId::max(), buffer, ranges, cx)
    }

    pub fn push_excerpts_with_context_lines<O>(
        &mut self,
        buffer: Model<Buffer>,
        ranges: Vec<Range<O>>,
        context_line_count: u32,
        cx: &mut ModelContext<Self>,
    ) -> Vec<Range<Anchor>>
    where
        O: text::ToPoint + text::ToOffset,
    {
        let buffer_id = buffer.read(cx).remote_id();
        let buffer_snapshot = buffer.read(cx).snapshot();
        let (excerpt_ranges, range_counts) =
            build_excerpt_ranges(&buffer_snapshot, &ranges, context_line_count);

        let excerpt_ids = self.push_excerpts(buffer, excerpt_ranges, cx);

        let mut anchor_ranges = Vec::new();
        let mut ranges = ranges.into_iter();
        for (excerpt_id, range_count) in excerpt_ids.into_iter().zip(range_counts.into_iter()) {
            anchor_ranges.extend(ranges.by_ref().take(range_count).map(|range| {
                let start = Anchor {
                    buffer_id: Some(buffer_id),
                    excerpt_id,
                    text_anchor: buffer_snapshot.anchor_after(range.start),
                };
                let end = Anchor {
                    buffer_id: Some(buffer_id),
                    excerpt_id,
                    text_anchor: buffer_snapshot.anchor_after(range.end),
                };
                start..end
            }))
        }
        anchor_ranges
    }

    pub fn push_multiple_excerpts_with_context_lines(
        &self,
        buffers_with_ranges: Vec<(Model<Buffer>, Vec<Range<text::Anchor>>)>,
        context_line_count: u32,
        cx: &mut ModelContext<Self>,
    ) -> Task<Vec<Range<Anchor>>> {
        use futures::StreamExt;

        let (excerpt_ranges_tx, mut excerpt_ranges_rx) = mpsc::channel(256);

        let mut buffer_ids = Vec::with_capacity(buffers_with_ranges.len());

        for (buffer, ranges) in buffers_with_ranges {
            let (buffer_id, buffer_snapshot) =
                buffer.update(cx, |buffer, _| (buffer.remote_id(), buffer.snapshot()));

            buffer_ids.push(buffer_id);

            cx.background_executor()
                .spawn({
                    let mut excerpt_ranges_tx = excerpt_ranges_tx.clone();

                    async move {
                        let (excerpt_ranges, counts) =
                            build_excerpt_ranges(&buffer_snapshot, &ranges, context_line_count);
                        excerpt_ranges_tx
                            .send((buffer_id, buffer.clone(), ranges, excerpt_ranges, counts))
                            .await
                            .ok();
                    }
                })
                .detach()
        }

        cx.spawn(move |this, mut cx| async move {
            let mut results_by_buffer_id = HashMap::default();
            while let Some((buffer_id, buffer, ranges, excerpt_ranges, range_counts)) =
                excerpt_ranges_rx.next().await
            {
                results_by_buffer_id
                    .insert(buffer_id, (buffer, ranges, excerpt_ranges, range_counts));
            }

            let mut multi_buffer_ranges = Vec::default();
            'outer: for buffer_id in buffer_ids {
                let Some((buffer, ranges, excerpt_ranges, range_counts)) =
                    results_by_buffer_id.remove(&buffer_id)
                else {
                    continue;
                };

                let mut ranges = ranges.into_iter();
                let mut range_counts = range_counts.into_iter();
                for excerpt_ranges in excerpt_ranges.chunks(100) {
                    let excerpt_ids = match this.update(&mut cx, |this, cx| {
                        this.push_excerpts(buffer.clone(), excerpt_ranges.iter().cloned(), cx)
                    }) {
                        Ok(excerpt_ids) => excerpt_ids,
                        Err(_) => continue 'outer,
                    };

                    for (excerpt_id, range_count) in
                        excerpt_ids.into_iter().zip(range_counts.by_ref())
                    {
                        for range in ranges.by_ref().take(range_count) {
                            let start = Anchor {
                                buffer_id: Some(buffer_id),
                                excerpt_id,
                                text_anchor: range.start,
                            };
                            let end = Anchor {
                                buffer_id: Some(buffer_id),
                                excerpt_id,
                                text_anchor: range.end,
                            };
                            multi_buffer_ranges.push(start..end);
                        }
                    }
                }
            }

            multi_buffer_ranges
        })
    }

    pub fn insert_excerpts_after<O>(
        &mut self,
        prev_excerpt_id: ExcerptId,
        buffer: Model<Buffer>,
        ranges: impl IntoIterator<Item = ExcerptRange<O>>,
        cx: &mut ModelContext<Self>,
    ) -> Vec<ExcerptId>
    where
        O: text::ToOffset,
    {
        let mut ids = Vec::new();
        let mut next_excerpt_id =
            if let Some(last_entry) = self.snapshot.borrow().excerpt_ids.last() {
                last_entry.id.0 + 1
            } else {
                1
            };
        self.insert_excerpts_with_ids_after(
            prev_excerpt_id,
            buffer,
            ranges.into_iter().map(|range| {
                let id = ExcerptId(post_inc(&mut next_excerpt_id));
                ids.push(id);
                (id, range)
            }),
            cx,
        );
        ids
    }

    pub fn insert_excerpts_with_ids_after<O>(
        &mut self,
        prev_excerpt_id: ExcerptId,
        buffer: Model<Buffer>,
        ranges: impl IntoIterator<Item = (ExcerptId, ExcerptRange<O>)>,
        cx: &mut ModelContext<Self>,
    ) where
        O: text::ToOffset,
    {
        assert_eq!(self.history.transaction_depth, 0);
        let mut ranges = ranges.into_iter().peekable();
        if ranges.peek().is_none() {
            return Default::default();
        }

        self.sync(cx);

        let buffer_id = buffer.read(cx).remote_id();
        let buffer_snapshot = buffer.read(cx).snapshot();

        let mut buffers = self.buffers.borrow_mut();
        let buffer_state = buffers.entry(buffer_id).or_insert_with(|| BufferState {
            last_version: buffer_snapshot.version().clone(),
            last_non_text_state_update_count: buffer_snapshot.non_text_state_update_count(),
            excerpts: Default::default(),
            _subscriptions: [
                cx.observe(&buffer, |_, _, cx| cx.notify()),
                cx.subscribe(&buffer, Self::on_buffer_event),
            ],
            buffer: buffer.clone(),
        });

        let mut snapshot = self.snapshot.borrow_mut();

        let mut prev_locator = snapshot.excerpt_locator_for_id(prev_excerpt_id).clone();
        let mut new_excerpt_ids = mem::take(&mut snapshot.excerpt_ids);
        let mut cursor = snapshot.excerpts.cursor::<Option<&Locator>>(&());
        let mut new_excerpts = cursor.slice(&prev_locator, Bias::Right, &());
        prev_locator = cursor.start().unwrap_or(Locator::min_ref()).clone();

        let edit_start = new_excerpts.summary().text.len;
        new_excerpts.update_last(
            |excerpt| {
                excerpt.has_trailing_newline = true;
            },
            &(),
        );

        let next_locator = if let Some(excerpt) = cursor.item() {
            excerpt.locator.clone()
        } else {
            Locator::max()
        };

        let mut excerpts = Vec::new();
        while let Some((id, range)) = ranges.next() {
            let locator = Locator::between(&prev_locator, &next_locator);
            if let Err(ix) = buffer_state.excerpts.binary_search(&locator) {
                buffer_state.excerpts.insert(ix, locator.clone());
            }
            let range = ExcerptRange {
                context: buffer_snapshot.anchor_before(&range.context.start)
                    ..buffer_snapshot.anchor_after(&range.context.end),
                primary: range.primary.map(|primary| {
                    buffer_snapshot.anchor_before(&primary.start)
                        ..buffer_snapshot.anchor_after(&primary.end)
                }),
            };
            excerpts.push((id, range.clone()));
            let excerpt = Excerpt::new(
                id,
                locator.clone(),
                buffer_id,
                buffer_snapshot.clone(),
                range,
                ranges.peek().is_some() || cursor.item().is_some(),
            );
            new_excerpts.push(excerpt, &());
            prev_locator = locator.clone();

            if let Some(last_mapping_entry) = new_excerpt_ids.last() {
                assert!(id > last_mapping_entry.id, "excerpt ids must be increasing");
            }
            new_excerpt_ids.push(ExcerptIdMapping { id, locator }, &());
        }

        let edit_end = new_excerpts.summary().text.len;

        let suffix = cursor.suffix(&());
        let changed_trailing_excerpt = suffix.is_empty();
        new_excerpts.append(suffix, &());
        drop(cursor);
        snapshot.excerpts = new_excerpts;
        snapshot.excerpt_ids = new_excerpt_ids;
        if changed_trailing_excerpt {
            snapshot.trailing_excerpt_update_count += 1;
        }

        self.subscriptions.publish_mut([Edit {
            old: edit_start..edit_start,
            new: edit_start..edit_end,
        }]);
        cx.emit(Event::Edited {
            singleton_buffer_edited: false,
            edited_buffer: None,
        });
        cx.emit(Event::ExcerptsAdded {
            buffer,
            predecessor: prev_excerpt_id,
            excerpts,
        });
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut ModelContext<Self>) {
        self.sync(cx);
        let ids = self.excerpt_ids();
        self.buffers.borrow_mut().clear();
        let mut snapshot = self.snapshot.borrow_mut();
        let prev_len = snapshot.len();
        snapshot.excerpts = Default::default();
        snapshot.trailing_excerpt_update_count += 1;
        snapshot.is_dirty = false;
        snapshot.has_deleted_file = false;
        snapshot.has_conflict = false;

        self.subscriptions.publish_mut([Edit {
            old: 0..prev_len,
            new: 0..0,
        }]);
        cx.emit(Event::Edited {
            singleton_buffer_edited: false,
            edited_buffer: None,
        });
        cx.emit(Event::ExcerptsRemoved { ids });
        cx.notify();
    }

    pub fn excerpts_for_buffer(
        &self,
        buffer: &Model<Buffer>,
        cx: &AppContext,
    ) -> Vec<(ExcerptId, ExcerptRange<text::Anchor>)> {
        let mut excerpts = Vec::new();
        let snapshot = self.read(cx);
        let buffers = self.buffers.borrow();
        let mut cursor = snapshot.excerpts.cursor::<Option<&Locator>>(&());
        for locator in buffers
            .get(&buffer.read(cx).remote_id())
            .map(|state| &state.excerpts)
            .into_iter()
            .flatten()
        {
            cursor.seek_forward(&Some(locator), Bias::Left, &());
            if let Some(excerpt) = cursor.item() {
                if excerpt.locator == *locator {
                    excerpts.push((excerpt.id, excerpt.range.clone()));
                }
            }
        }

        excerpts
    }

    pub fn excerpt_ranges_for_buffer(
        &self,
        buffer_id: BufferId,
        cx: &AppContext,
    ) -> Vec<Range<Point>> {
        let snapshot = self.read(cx);
        let buffers = self.buffers.borrow();
        let mut cursor = snapshot.excerpts.cursor::<(Option<&Locator>, Point)>(&());
        buffers
            .get(&buffer_id)
            .into_iter()
            .flat_map(|state| &state.excerpts)
            .filter_map(move |locator| {
                cursor.seek_forward(&Some(locator), Bias::Left, &());
                cursor.item().and_then(|excerpt| {
                    if excerpt.locator == *locator {
                        let excerpt_start = cursor.start().1;
                        let excerpt_end = excerpt_start + excerpt.text_summary.lines;
                        Some(excerpt_start..excerpt_end)
                    } else {
                        None
                    }
                })
            })
            .collect()
    }

    pub fn excerpt_buffer_ids(&self) -> Vec<BufferId> {
        self.snapshot
            .borrow()
            .excerpts
            .iter()
            .map(|entry| entry.buffer_id)
            .collect()
    }

    pub fn excerpt_ids(&self) -> Vec<ExcerptId> {
        self.snapshot
            .borrow()
            .excerpts
            .iter()
            .map(|entry| entry.id)
            .collect()
    }

    pub fn excerpt_containing(
        &self,
        position: impl ToOffset,
        cx: &AppContext,
    ) -> Option<(ExcerptId, Model<Buffer>, Range<text::Anchor>)> {
        let snapshot = self.read(cx);
        let position = position.to_offset(&snapshot);

        let mut cursor = snapshot.excerpts.cursor::<usize>(&());
        cursor.seek(&position, Bias::Right, &());
        cursor
            .item()
            .or_else(|| snapshot.excerpts.last())
            .map(|excerpt| {
                (
                    excerpt.id,
                    self.buffers
                        .borrow()
                        .get(&excerpt.buffer_id)
                        .unwrap()
                        .buffer
                        .clone(),
                    excerpt.range.context.clone(),
                )
            })
    }

    // If point is at the end of the buffer, the last excerpt is returned
    pub fn point_to_buffer_offset<T: ToOffset>(
        &self,
        point: T,
        cx: &AppContext,
    ) -> Option<(Model<Buffer>, usize, ExcerptId)> {
        let snapshot = self.read(cx);
        let offset = point.to_offset(&snapshot);
        let mut cursor = snapshot.excerpts.cursor::<usize>(&());
        cursor.seek(&offset, Bias::Right, &());
        if cursor.item().is_none() {
            cursor.prev(&());
        }

        cursor.item().map(|excerpt| {
            let excerpt_start = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let buffer_point = excerpt_start + offset - *cursor.start();
            let buffer = self.buffers.borrow()[&excerpt.buffer_id].buffer.clone();

            (buffer, buffer_point, excerpt.id)
        })
    }

    // If point is at the end of the buffer, the last excerpt is returned
    pub fn point_to_buffer_point<T: ToPoint>(
        &self,
        point: T,
        cx: &AppContext,
    ) -> Option<(Model<Buffer>, Point, ExcerptId)> {
        let snapshot = self.read(cx);
        let point = point.to_point(&snapshot);
        let mut cursor = snapshot.excerpts.cursor::<Point>(&());
        cursor.seek(&point, Bias::Right, &());
        if cursor.item().is_none() {
            cursor.prev(&());
        }

        cursor.item().map(|excerpt| {
            let excerpt_start = excerpt.range.context.start.to_point(&excerpt.buffer);
            let buffer_point = excerpt_start + point - *cursor.start();
            let buffer = self.buffers.borrow()[&excerpt.buffer_id].buffer.clone();

            (buffer, buffer_point, excerpt.id)
        })
    }

    pub fn range_to_buffer_ranges<T: ToOffset>(
        &self,
        range: Range<T>,
        cx: &AppContext,
    ) -> Vec<(Model<Buffer>, Range<usize>, ExcerptId)> {
        let snapshot = self.read(cx);
        let start = range.start.to_offset(&snapshot);
        let end = range.end.to_offset(&snapshot);

        let mut result = Vec::new();
        let mut cursor = snapshot.excerpts.cursor::<usize>(&());
        cursor.seek(&start, Bias::Right, &());
        if cursor.item().is_none() {
            cursor.prev(&());
        }

        while let Some(excerpt) = cursor.item() {
            if *cursor.start() > end {
                break;
            }

            let mut end_before_newline = cursor.end(&());
            if excerpt.has_trailing_newline {
                end_before_newline -= 1;
            }
            let excerpt_start = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let start = excerpt_start + (cmp::max(start, *cursor.start()) - *cursor.start());
            let end = excerpt_start + (cmp::min(end, end_before_newline) - *cursor.start());
            let buffer = self.buffers.borrow()[&excerpt.buffer_id].buffer.clone();
            result.push((buffer, start..end, excerpt.id));
            cursor.next(&());
        }

        result
    }

    pub fn remove_excerpts(
        &mut self,
        excerpt_ids: impl IntoIterator<Item = ExcerptId>,
        cx: &mut ModelContext<Self>,
    ) {
        self.sync(cx);
        let ids = excerpt_ids.into_iter().collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }

        let mut buffers = self.buffers.borrow_mut();
        let mut snapshot = self.snapshot.borrow_mut();
        let mut new_excerpts = SumTree::default();
        let mut cursor = snapshot.excerpts.cursor::<(Option<&Locator>, usize)>(&());
        let mut edits = Vec::new();
        let mut excerpt_ids = ids.iter().copied().peekable();

        while let Some(excerpt_id) = excerpt_ids.next() {
            // Seek to the next excerpt to remove, preserving any preceding excerpts.
            let locator = snapshot.excerpt_locator_for_id(excerpt_id);
            new_excerpts.append(cursor.slice(&Some(locator), Bias::Left, &()), &());

            if let Some(mut excerpt) = cursor.item() {
                if excerpt.id != excerpt_id {
                    continue;
                }
                let mut old_start = cursor.start().1;

                // Skip over the removed excerpt.
                'remove_excerpts: loop {
                    if let Some(buffer_state) = buffers.get_mut(&excerpt.buffer_id) {
                        buffer_state.excerpts.retain(|l| l != &excerpt.locator);
                        if buffer_state.excerpts.is_empty() {
                            buffers.remove(&excerpt.buffer_id);
                        }
                    }
                    cursor.next(&());

                    // Skip over any subsequent excerpts that are also removed.
                    if let Some(&next_excerpt_id) = excerpt_ids.peek() {
                        let next_locator = snapshot.excerpt_locator_for_id(next_excerpt_id);
                        if let Some(next_excerpt) = cursor.item() {
                            if next_excerpt.locator == *next_locator {
                                excerpt_ids.next();
                                excerpt = next_excerpt;
                                continue 'remove_excerpts;
                            }
                        }
                    }

                    break;
                }

                // When removing the last excerpt, remove the trailing newline from
                // the previous excerpt.
                if cursor.item().is_none() && old_start > 0 {
                    old_start -= 1;
                    new_excerpts.update_last(|e| e.has_trailing_newline = false, &());
                }

                // Push an edit for the removal of this run of excerpts.
                let old_end = cursor.start().1;
                let new_start = new_excerpts.summary().text.len;
                edits.push(Edit {
                    old: old_start..old_end,
                    new: new_start..new_start,
                });
            }
        }
        let suffix = cursor.suffix(&());
        let changed_trailing_excerpt = suffix.is_empty();
        new_excerpts.append(suffix, &());
        drop(cursor);
        snapshot.excerpts = new_excerpts;

        if changed_trailing_excerpt {
            snapshot.trailing_excerpt_update_count += 1;
        }

        self.subscriptions.publish_mut(edits);
        cx.emit(Event::Edited {
            singleton_buffer_edited: false,
            edited_buffer: None,
        });
        cx.emit(Event::ExcerptsRemoved { ids });
        cx.notify();
    }

    pub fn wait_for_anchors<'a>(
        &self,
        anchors: impl 'a + Iterator<Item = Anchor>,
        cx: &mut ModelContext<Self>,
    ) -> impl 'static + Future<Output = Result<()>> {
        let borrow = self.buffers.borrow();
        let mut error = None;
        let mut futures = Vec::new();
        for anchor in anchors {
            if let Some(buffer_id) = anchor.buffer_id {
                if let Some(buffer) = borrow.get(&buffer_id) {
                    buffer.buffer.update(cx, |buffer, _| {
                        futures.push(buffer.wait_for_anchors([anchor.text_anchor]))
                    });
                } else {
                    error = Some(anyhow!(
                        "buffer {buffer_id} is not part of this multi-buffer"
                    ));
                    break;
                }
            }
        }
        async move {
            if let Some(error) = error {
                Err(error)?;
            }
            for future in futures {
                future.await?;
            }
            Ok(())
        }
    }

    pub fn text_anchor_for_position<T: ToOffset>(
        &self,
        position: T,
        cx: &AppContext,
    ) -> Option<(Model<Buffer>, language::Anchor)> {
        let snapshot = self.read(cx);
        let anchor = snapshot.anchor_before(position);
        let buffer = self
            .buffers
            .borrow()
            .get(&anchor.buffer_id?)?
            .buffer
            .clone();
        Some((buffer, anchor.text_anchor))
    }

    fn on_buffer_event(
        &mut self,
        buffer: Model<Buffer>,
        event: &language::BufferEvent,
        cx: &mut ModelContext<Self>,
    ) {
        cx.emit(match event {
            language::BufferEvent::Edited => Event::Edited {
                singleton_buffer_edited: true,
                edited_buffer: Some(buffer.clone()),
            },
            language::BufferEvent::DirtyChanged => Event::DirtyChanged,
            language::BufferEvent::Saved => Event::Saved,
            language::BufferEvent::FileHandleChanged => Event::FileHandleChanged,
            language::BufferEvent::Reloaded => Event::Reloaded,
            language::BufferEvent::ReloadNeeded => Event::ReloadNeeded,
            language::BufferEvent::LanguageChanged => {
                Event::LanguageChanged(buffer.read(cx).remote_id())
            }
            language::BufferEvent::Reparsed => Event::Reparsed(buffer.read(cx).remote_id()),
            language::BufferEvent::DiagnosticsUpdated => Event::DiagnosticsUpdated,
            language::BufferEvent::Closed => Event::Closed,
            language::BufferEvent::Discarded => Event::Discarded,
            language::BufferEvent::CapabilityChanged => {
                self.capability = buffer.read(cx).capability();
                Event::CapabilityChanged
            }
            //
            language::BufferEvent::Operation { .. } => return,
        });
    }

    pub fn all_buffers(&self) -> HashSet<Model<Buffer>> {
        self.buffers
            .borrow()
            .values()
            .map(|state| state.buffer.clone())
            .collect()
    }

    pub fn buffer(&self, buffer_id: BufferId) -> Option<Model<Buffer>> {
        self.buffers
            .borrow()
            .get(&buffer_id)
            .map(|state| state.buffer.clone())
    }

    pub fn language_at<T: ToOffset>(&self, point: T, cx: &AppContext) -> Option<Arc<Language>> {
        self.point_to_buffer_offset(point, cx)
            .and_then(|(buffer, offset, _)| buffer.read(cx).language_at(offset))
    }

    pub fn settings_at<'a, T: ToOffset>(
        &self,
        point: T,
        cx: &'a AppContext,
    ) -> Cow<'a, LanguageSettings> {
        let mut language = None;
        let mut file = None;
        if let Some((buffer, offset, _)) = self.point_to_buffer_offset(point, cx) {
            let buffer = buffer.read(cx);
            language = buffer.language_at(offset);
            file = buffer.file();
        }
        language_settings(language.map(|l| l.name()), file, cx)
    }

    pub fn for_each_buffer(&self, mut f: impl FnMut(&Model<Buffer>)) {
        self.buffers
            .borrow()
            .values()
            .for_each(|state| f(&state.buffer))
    }

    pub fn title<'a>(&'a self, cx: &'a AppContext) -> Cow<'a, str> {
        if let Some(title) = self.title.as_ref() {
            return title.into();
        }

        if let Some(buffer) = self.as_singleton() {
            if let Some(file) = buffer.read(cx).file() {
                return file.file_name(cx).to_string_lossy();
            }
        }

        "untitled".into()
    }

    pub fn set_title(&mut self, title: String, cx: &mut ModelContext<Self>) {
        self.title = Some(title);
        cx.notify();
    }

    /// Preserve preview tabs containing this multibuffer until additional edits occur.
    pub fn refresh_preview(&self, cx: &mut ModelContext<Self>) {
        for buffer_state in self.buffers.borrow().values() {
            buffer_state
                .buffer
                .update(cx, |buffer, _cx| buffer.refresh_preview());
        }
    }

    /// Whether we should preserve the preview status of a tab containing this multi-buffer.
    pub fn preserve_preview(&self, cx: &AppContext) -> bool {
        self.buffers
            .borrow()
            .values()
            .all(|state| state.buffer.read(cx).preserve_preview())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn is_parsing(&self, cx: &AppContext) -> bool {
        self.as_singleton().unwrap().read(cx).is_parsing()
    }

    pub fn resize_excerpt(
        &mut self,
        id: ExcerptId,
        range: Range<text::Anchor>,
        cx: &mut ModelContext<Self>,
    ) {
        self.sync(cx);

        let snapshot = self.snapshot(cx);
        let locator = snapshot.excerpt_locator_for_id(id);
        let mut new_excerpts = SumTree::default();
        let mut cursor = snapshot.excerpts.cursor::<(Option<&Locator>, usize)>(&());
        let mut edits = Vec::<Edit<usize>>::new();

        let prefix = cursor.slice(&Some(locator), Bias::Left, &());
        new_excerpts.append(prefix, &());

        let mut excerpt = cursor.item().unwrap().clone();
        let old_text_len = excerpt.text_summary.len;

        excerpt.range.context.start = range.start;
        excerpt.range.context.end = range.end;
        excerpt.max_buffer_row = range.end.to_point(&excerpt.buffer).row;

        excerpt.text_summary = excerpt
            .buffer
            .text_summary_for_range(excerpt.range.context.clone());

        let new_start_offset = new_excerpts.summary().text.len;
        let old_start_offset = cursor.start().1;
        let edit = Edit {
            old: old_start_offset..old_start_offset + old_text_len,
            new: new_start_offset..new_start_offset + excerpt.text_summary.len,
        };

        if let Some(last_edit) = edits.last_mut() {
            if last_edit.old.end == edit.old.start {
                last_edit.old.end = edit.old.end;
                last_edit.new.end = edit.new.end;
            } else {
                edits.push(edit);
            }
        } else {
            edits.push(edit);
        }

        new_excerpts.push(excerpt, &());

        cursor.next(&());

        new_excerpts.append(cursor.suffix(&()), &());

        drop(cursor);
        self.snapshot.borrow_mut().excerpts = new_excerpts;

        self.subscriptions.publish_mut(edits);
        cx.emit(Event::Edited {
            singleton_buffer_edited: false,
            edited_buffer: None,
        });
        cx.emit(Event::ExcerptsExpanded { ids: vec![id] });
        cx.notify();
    }

    pub fn expand_excerpts(
        &mut self,
        ids: impl IntoIterator<Item = ExcerptId>,
        line_count: u32,
        direction: ExpandExcerptDirection,
        cx: &mut ModelContext<Self>,
    ) {
        if line_count == 0 {
            return;
        }
        self.sync(cx);

        let ids = ids.into_iter().collect::<Vec<_>>();
        let snapshot = self.snapshot(cx);
        let locators = snapshot.excerpt_locators_for_ids(ids.iter().copied());
        let mut new_excerpts = SumTree::default();
        let mut cursor = snapshot.excerpts.cursor::<(Option<&Locator>, usize)>(&());
        let mut edits = Vec::<Edit<usize>>::new();

        for locator in &locators {
            let prefix = cursor.slice(&Some(locator), Bias::Left, &());
            new_excerpts.append(prefix, &());

            let mut excerpt = cursor.item().unwrap().clone();
            let old_text_len = excerpt.text_summary.len;

            let up_line_count = if direction.should_expand_up() {
                line_count
            } else {
                0
            };

            let start_row = excerpt
                .range
                .context
                .start
                .to_point(&excerpt.buffer)
                .row
                .saturating_sub(up_line_count);
            let start_point = Point::new(start_row, 0);
            excerpt.range.context.start = excerpt.buffer.anchor_before(start_point);

            let down_line_count = if direction.should_expand_down() {
                line_count
            } else {
                0
            };

            let mut end_point = excerpt.buffer.clip_point(
                excerpt.range.context.end.to_point(&excerpt.buffer)
                    + Point::new(down_line_count, 0),
                Bias::Left,
            );
            end_point.column = excerpt.buffer.line_len(end_point.row);
            excerpt.range.context.end = excerpt.buffer.anchor_after(end_point);
            excerpt.max_buffer_row = end_point.row;

            excerpt.text_summary = excerpt
                .buffer
                .text_summary_for_range(excerpt.range.context.clone());

            let new_start_offset = new_excerpts.summary().text.len;
            let old_start_offset = cursor.start().1;
            let edit = Edit {
                old: old_start_offset..old_start_offset + old_text_len,
                new: new_start_offset..new_start_offset + excerpt.text_summary.len,
            };

            if let Some(last_edit) = edits.last_mut() {
                if last_edit.old.end == edit.old.start {
                    last_edit.old.end = edit.old.end;
                    last_edit.new.end = edit.new.end;
                } else {
                    edits.push(edit);
                }
            } else {
                edits.push(edit);
            }

            new_excerpts.push(excerpt, &());

            cursor.next(&());
        }

        new_excerpts.append(cursor.suffix(&()), &());

        drop(cursor);
        self.snapshot.borrow_mut().excerpts = new_excerpts;

        self.subscriptions.publish_mut(edits);
        cx.emit(Event::Edited {
            singleton_buffer_edited: false,
            edited_buffer: None,
        });
        cx.emit(Event::ExcerptsExpanded { ids });
        cx.notify();
    }

    fn sync(&self, cx: &AppContext) {
        let mut snapshot = self.snapshot.borrow_mut();
        let mut excerpts_to_edit = Vec::new();
        let mut non_text_state_updated = false;
        let mut is_dirty = false;
        let mut has_deleted_file = false;
        let mut has_conflict = false;
        let mut edited = false;
        let mut buffers = self.buffers.borrow_mut();
        for buffer_state in buffers.values_mut() {
            let buffer = buffer_state.buffer.read(cx);
            let version = buffer.version();
            let non_text_state_update_count = buffer.non_text_state_update_count();

            let buffer_edited = version.changed_since(&buffer_state.last_version);
            let buffer_non_text_state_updated =
                non_text_state_update_count > buffer_state.last_non_text_state_update_count;
            if buffer_edited || buffer_non_text_state_updated {
                buffer_state.last_version = version;
                buffer_state.last_non_text_state_update_count = non_text_state_update_count;
                excerpts_to_edit.extend(
                    buffer_state
                        .excerpts
                        .iter()
                        .map(|locator| (locator, buffer_state.buffer.clone(), buffer_edited)),
                );
            }

            edited |= buffer_edited;
            non_text_state_updated |= buffer_non_text_state_updated;
            is_dirty |= buffer.is_dirty();
            has_deleted_file |= buffer
                .file()
                .map_or(false, |file| file.disk_state() == DiskState::Deleted);
            has_conflict |= buffer.has_conflict();
        }
        if edited {
            snapshot.edit_count += 1;
        }
        if non_text_state_updated {
            snapshot.non_text_state_update_count += 1;
        }
        snapshot.is_dirty = is_dirty;
        snapshot.has_deleted_file = has_deleted_file;
        snapshot.has_conflict = has_conflict;

        excerpts_to_edit.sort_unstable_by_key(|(locator, _, _)| *locator);

        let mut edits = Vec::new();
        let mut new_excerpts = SumTree::default();
        let mut cursor = snapshot.excerpts.cursor::<(Option<&Locator>, usize)>(&());

        for (locator, buffer, buffer_edited) in excerpts_to_edit {
            new_excerpts.append(cursor.slice(&Some(locator), Bias::Left, &()), &());
            let old_excerpt = cursor.item().unwrap();
            let buffer = buffer.read(cx);
            let buffer_id = buffer.remote_id();

            let mut new_excerpt;
            if buffer_edited {
                edits.extend(
                    buffer
                        .edits_since_in_range::<usize>(
                            old_excerpt.buffer.version(),
                            old_excerpt.range.context.clone(),
                        )
                        .map(|mut edit| {
                            let excerpt_old_start = cursor.start().1;
                            let excerpt_new_start = new_excerpts.summary().text.len;
                            edit.old.start += excerpt_old_start;
                            edit.old.end += excerpt_old_start;
                            edit.new.start += excerpt_new_start;
                            edit.new.end += excerpt_new_start;
                            edit
                        }),
                );

                new_excerpt = Excerpt::new(
                    old_excerpt.id,
                    locator.clone(),
                    buffer_id,
                    buffer.snapshot(),
                    old_excerpt.range.clone(),
                    old_excerpt.has_trailing_newline,
                );
            } else {
                new_excerpt = old_excerpt.clone();
                new_excerpt.buffer = buffer.snapshot();
            }

            new_excerpts.push(new_excerpt, &());
            cursor.next(&());
        }
        new_excerpts.append(cursor.suffix(&()), &());

        drop(cursor);
        snapshot.excerpts = new_excerpts;

        self.subscriptions.publish(edits);
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MultiBuffer {
    pub fn build_simple(text: &str, cx: &mut gpui::AppContext) -> Model<Self> {
        let buffer = cx.new_model(|cx| Buffer::local(text, cx));
        cx.new_model(|cx| Self::singleton(buffer, cx))
    }

    pub fn build_multi<const COUNT: usize>(
        excerpts: [(&str, Vec<Range<Point>>); COUNT],
        cx: &mut gpui::AppContext,
    ) -> Model<Self> {
        let multi = cx.new_model(|_| Self::new(Capability::ReadWrite));
        for (text, ranges) in excerpts {
            let buffer = cx.new_model(|cx| Buffer::local(text, cx));
            let excerpt_ranges = ranges.into_iter().map(|range| ExcerptRange {
                context: range,
                primary: None,
            });
            multi.update(cx, |multi, cx| {
                multi.push_excerpts(buffer, excerpt_ranges, cx)
            });
        }

        multi
    }

    pub fn build_from_buffer(buffer: Model<Buffer>, cx: &mut gpui::AppContext) -> Model<Self> {
        cx.new_model(|cx| Self::singleton(buffer, cx))
    }

    pub fn build_random(rng: &mut impl rand::Rng, cx: &mut gpui::AppContext) -> Model<Self> {
        cx.new_model(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            let mutation_count = rng.gen_range(1..=5);
            multibuffer.randomly_edit_excerpts(rng, mutation_count, cx);
            multibuffer
        })
    }

    pub fn randomly_edit(
        &mut self,
        rng: &mut impl rand::Rng,
        edit_count: usize,
        cx: &mut ModelContext<Self>,
    ) {
        use util::RandomCharIter;

        let snapshot = self.read(cx);
        let mut edits: Vec<(Range<usize>, Arc<str>)> = Vec::new();
        let mut last_end = None;
        for _ in 0..edit_count {
            if last_end.map_or(false, |last_end| last_end >= snapshot.len()) {
                break;
            }

            let new_start = last_end.map_or(0, |last_end| last_end + 1);
            let end = snapshot.clip_offset(rng.gen_range(new_start..=snapshot.len()), Bias::Right);
            let start = snapshot.clip_offset(rng.gen_range(new_start..=end), Bias::Right);
            last_end = Some(end);

            let mut range = start..end;
            if rng.gen_bool(0.2) {
                mem::swap(&mut range.start, &mut range.end);
            }

            let new_text_len = rng.gen_range(0..10);
            let new_text: String = RandomCharIter::new(&mut *rng).take(new_text_len).collect();

            edits.push((range, new_text.into()));
        }
        log::info!("mutating multi-buffer with {:?}", edits);
        drop(snapshot);

        self.edit(edits, None, cx);
    }

    pub fn randomly_edit_excerpts(
        &mut self,
        rng: &mut impl rand::Rng,
        mutation_count: usize,
        cx: &mut ModelContext<Self>,
    ) {
        use rand::prelude::*;
        use std::env;
        use util::RandomCharIter;

        let max_excerpts = env::var("MAX_EXCERPTS")
            .map(|i| i.parse().expect("invalid `MAX_EXCERPTS` variable"))
            .unwrap_or(5);

        let mut buffers = Vec::new();
        for _ in 0..mutation_count {
            if rng.gen_bool(0.05) {
                log::info!("Clearing multi-buffer");
                self.clear(cx);
                continue;
            } else if rng.gen_bool(0.1) && !self.excerpt_ids().is_empty() {
                let ids = self.excerpt_ids();
                let mut excerpts = HashSet::default();
                for _ in 0..rng.gen_range(0..ids.len()) {
                    excerpts.extend(ids.choose(rng).copied());
                }

                let line_count = rng.gen_range(0..5);

                log::info!("Expanding excerpts {excerpts:?} by {line_count} lines");

                self.expand_excerpts(
                    excerpts.iter().cloned(),
                    line_count,
                    ExpandExcerptDirection::UpAndDown,
                    cx,
                );
                continue;
            }

            let excerpt_ids = self.excerpt_ids();
            if excerpt_ids.is_empty() || (rng.gen() && excerpt_ids.len() < max_excerpts) {
                let buffer_handle = if rng.gen() || self.buffers.borrow().is_empty() {
                    let text = RandomCharIter::new(&mut *rng).take(10).collect::<String>();
                    buffers.push(cx.new_model(|cx| Buffer::local(text, cx)));
                    let buffer = buffers.last().unwrap().read(cx);
                    log::info!(
                        "Creating new buffer {} with text: {:?}",
                        buffer.remote_id(),
                        buffer.text()
                    );
                    buffers.last().unwrap().clone()
                } else {
                    self.buffers
                        .borrow()
                        .values()
                        .choose(rng)
                        .unwrap()
                        .buffer
                        .clone()
                };

                let buffer = buffer_handle.read(cx);
                let buffer_text = buffer.text();
                let ranges = (0..rng.gen_range(0..5))
                    .map(|_| {
                        let end_ix =
                            buffer.clip_offset(rng.gen_range(0..=buffer.len()), Bias::Right);
                        let start_ix = buffer.clip_offset(rng.gen_range(0..=end_ix), Bias::Left);
                        ExcerptRange {
                            context: start_ix..end_ix,
                            primary: None,
                        }
                    })
                    .collect::<Vec<_>>();
                log::info!(
                    "Inserting excerpts from buffer {} and ranges {:?}: {:?}",
                    buffer_handle.read(cx).remote_id(),
                    ranges.iter().map(|r| &r.context).collect::<Vec<_>>(),
                    ranges
                        .iter()
                        .map(|r| &buffer_text[r.context.clone()])
                        .collect::<Vec<_>>()
                );

                let excerpt_id = self.push_excerpts(buffer_handle.clone(), ranges, cx);
                log::info!("Inserted with ids: {:?}", excerpt_id);
            } else {
                let remove_count = rng.gen_range(1..=excerpt_ids.len());
                let mut excerpts_to_remove = excerpt_ids
                    .choose_multiple(rng, remove_count)
                    .cloned()
                    .collect::<Vec<_>>();
                let snapshot = self.snapshot.borrow();
                excerpts_to_remove.sort_unstable_by(|a, b| a.cmp(b, &snapshot));
                drop(snapshot);
                log::info!("Removing excerpts {:?}", excerpts_to_remove);
                self.remove_excerpts(excerpts_to_remove, cx);
            }
        }
    }

    pub fn randomly_mutate(
        &mut self,
        rng: &mut impl rand::Rng,
        mutation_count: usize,
        cx: &mut ModelContext<Self>,
    ) {
        use rand::prelude::*;

        if rng.gen_bool(0.7) || self.singleton {
            let buffer = self
                .buffers
                .borrow()
                .values()
                .choose(rng)
                .map(|state| state.buffer.clone());

            if let Some(buffer) = buffer {
                buffer.update(cx, |buffer, cx| {
                    if rng.gen() {
                        buffer.randomly_edit(rng, mutation_count, cx);
                    } else {
                        buffer.randomly_undo_redo(rng, cx);
                    }
                });
            } else {
                self.randomly_edit(rng, mutation_count, cx);
            }
        } else {
            self.randomly_edit_excerpts(rng, mutation_count, cx);
        }

        self.check_invariants(cx);
    }

    fn check_invariants(&self, cx: &mut ModelContext<Self>) {
        let snapshot = self.read(cx);
        let excerpts = snapshot.excerpts.items(&());
        let excerpt_ids = snapshot.excerpt_ids.items(&());

        for (ix, excerpt) in excerpts.iter().enumerate() {
            if ix == 0 {
                if excerpt.locator <= Locator::min() {
                    panic!("invalid first excerpt locator {:?}", excerpt.locator);
                }
            } else if excerpt.locator <= excerpts[ix - 1].locator {
                panic!("excerpts are out-of-order: {:?}", excerpts);
            }
        }

        for (ix, entry) in excerpt_ids.iter().enumerate() {
            if ix == 0 {
                if entry.id.cmp(&ExcerptId::min(), &snapshot).is_le() {
                    panic!("invalid first excerpt id {:?}", entry.id);
                }
            } else if entry.id <= excerpt_ids[ix - 1].id {
                panic!("excerpt ids are out-of-order: {:?}", excerpt_ids);
            }
        }
    }
}

impl EventEmitter<Event> for MultiBuffer {}

impl MultiBufferSnapshot {
    pub fn text(&self) -> String {
        self.chunks(0..self.len(), false)
            .map(|chunk| chunk.text)
            .collect()
    }

    pub fn reversed_chars_at<T: ToOffset>(&self, position: T) -> impl Iterator<Item = char> + '_ {
        let mut offset = position.to_offset(self);
        let mut cursor = self.excerpts.cursor::<usize>(&());
        cursor.seek(&offset, Bias::Left, &());
        let mut excerpt_chunks = cursor.item().map(|excerpt| {
            let end_before_footer = cursor.start() + excerpt.text_summary.len;
            let start = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let end = start + (cmp::min(offset, end_before_footer) - cursor.start());
            excerpt.buffer.reversed_chunks_in_range(start..end)
        });
        iter::from_fn(move || {
            if offset == *cursor.start() {
                cursor.prev(&());
                let excerpt = cursor.item()?;
                excerpt_chunks = Some(
                    excerpt
                        .buffer
                        .reversed_chunks_in_range(excerpt.range.context.clone()),
                );
            }

            let excerpt = cursor.item().unwrap();
            if offset == cursor.end(&()) && excerpt.has_trailing_newline {
                offset -= 1;
                Some("\n")
            } else {
                let chunk = excerpt_chunks.as_mut().unwrap().next().unwrap();
                offset -= chunk.len();
                Some(chunk)
            }
        })
        .flat_map(|c| c.chars().rev())
    }

    pub fn chars_at<T: ToOffset>(&self, position: T) -> impl Iterator<Item = char> + '_ {
        let offset = position.to_offset(self);
        self.text_for_range(offset..self.len())
            .flat_map(|chunk| chunk.chars())
    }

    pub fn text_for_range<T: ToOffset>(&self, range: Range<T>) -> impl Iterator<Item = &str> + '_ {
        self.chunks(range, false).map(|chunk| chunk.text)
    }

    pub fn is_line_blank(&self, row: MultiBufferRow) -> bool {
        self.text_for_range(Point::new(row.0, 0)..Point::new(row.0, self.line_len(row)))
            .all(|chunk| chunk.matches(|c: char| !c.is_whitespace()).next().is_none())
    }

    pub fn contains_str_at<T>(&self, position: T, needle: &str) -> bool
    where
        T: ToOffset,
    {
        let position = position.to_offset(self);
        position == self.clip_offset(position, Bias::Left)
            && self
                .bytes_in_range(position..self.len())
                .flatten()
                .copied()
                .take(needle.len())
                .eq(needle.bytes())
    }

    pub fn surrounding_word<T: ToOffset>(
        &self,
        start: T,
        for_completion: bool,
    ) -> (Range<usize>, Option<CharKind>) {
        let mut start = start.to_offset(self);
        let mut end = start;
        let mut next_chars = self.chars_at(start).peekable();
        let mut prev_chars = self.reversed_chars_at(start).peekable();

        let classifier = self
            .char_classifier_at(start)
            .for_completion(for_completion);

        let word_kind = cmp::max(
            prev_chars.peek().copied().map(|c| classifier.kind(c)),
            next_chars.peek().copied().map(|c| classifier.kind(c)),
        );

        for ch in prev_chars {
            if Some(classifier.kind(ch)) == word_kind && ch != '\n' {
                start -= ch.len_utf8();
            } else {
                break;
            }
        }

        for ch in next_chars {
            if Some(classifier.kind(ch)) == word_kind && ch != '\n' {
                end += ch.len_utf8();
            } else {
                break;
            }
        }

        (start..end, word_kind)
    }

    pub fn as_singleton(&self) -> Option<(&ExcerptId, BufferId, &BufferSnapshot)> {
        if self.singleton {
            self.excerpts
                .iter()
                .next()
                .map(|e| (&e.id, e.buffer_id, &e.buffer))
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.excerpts.summary().text.len
    }

    pub fn is_empty(&self) -> bool {
        self.excerpts.summary().text.len == 0
    }

    pub fn widest_line_number(&self) -> u32 {
        self.excerpts.summary().widest_line_number + 1
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.clip_offset(offset, bias);
        }

        let mut cursor = self.excerpts.cursor::<usize>(&());
        cursor.seek(&offset, Bias::Right, &());
        let overshoot = if let Some(excerpt) = cursor.item() {
            let excerpt_start = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let buffer_offset = excerpt
                .buffer
                .clip_offset(excerpt_start + (offset - cursor.start()), bias);
            buffer_offset.saturating_sub(excerpt_start)
        } else {
            0
        };
        cursor.start() + overshoot
    }

    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.clip_point(point, bias);
        }

        let mut cursor = self.excerpts.cursor::<Point>(&());
        cursor.seek(&point, Bias::Right, &());
        let overshoot = if let Some(excerpt) = cursor.item() {
            let excerpt_start = excerpt.range.context.start.to_point(&excerpt.buffer);
            let buffer_point = excerpt
                .buffer
                .clip_point(excerpt_start + (point - cursor.start()), bias);
            buffer_point.saturating_sub(excerpt_start)
        } else {
            Point::zero()
        };
        *cursor.start() + overshoot
    }

    pub fn clip_offset_utf16(&self, offset: OffsetUtf16, bias: Bias) -> OffsetUtf16 {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.clip_offset_utf16(offset, bias);
        }

        let mut cursor = self.excerpts.cursor::<OffsetUtf16>(&());
        cursor.seek(&offset, Bias::Right, &());
        let overshoot = if let Some(excerpt) = cursor.item() {
            let excerpt_start = excerpt.range.context.start.to_offset_utf16(&excerpt.buffer);
            let buffer_offset = excerpt
                .buffer
                .clip_offset_utf16(excerpt_start + (offset - cursor.start()), bias);
            OffsetUtf16(buffer_offset.0.saturating_sub(excerpt_start.0))
        } else {
            OffsetUtf16(0)
        };
        *cursor.start() + overshoot
    }

    pub fn clip_point_utf16(&self, point: Unclipped<PointUtf16>, bias: Bias) -> PointUtf16 {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.clip_point_utf16(point, bias);
        }

        let mut cursor = self.excerpts.cursor::<PointUtf16>(&());
        cursor.seek(&point.0, Bias::Right, &());
        let overshoot = if let Some(excerpt) = cursor.item() {
            let excerpt_start = excerpt
                .buffer
                .offset_to_point_utf16(excerpt.range.context.start.to_offset(&excerpt.buffer));
            let buffer_point = excerpt
                .buffer
                .clip_point_utf16(Unclipped(excerpt_start + (point.0 - cursor.start())), bias);
            buffer_point.saturating_sub(excerpt_start)
        } else {
            PointUtf16::zero()
        };
        *cursor.start() + overshoot
    }

    pub fn bytes_in_range<T: ToOffset>(&self, range: Range<T>) -> MultiBufferBytes {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let mut excerpts = self.excerpts.cursor::<usize>(&());
        excerpts.seek(&range.start, Bias::Right, &());

        let mut chunk = &[][..];
        let excerpt_bytes = if let Some(excerpt) = excerpts.item() {
            let mut excerpt_bytes = excerpt
                .bytes_in_range(range.start - excerpts.start()..range.end - excerpts.start());
            chunk = excerpt_bytes.next().unwrap_or(&[][..]);
            Some(excerpt_bytes)
        } else {
            None
        };
        MultiBufferBytes {
            range,
            excerpts,
            excerpt_bytes,
            chunk,
        }
    }

    pub fn reversed_bytes_in_range<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> ReversedMultiBufferBytes {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let mut excerpts = self.excerpts.cursor::<usize>(&());
        excerpts.seek(&range.end, Bias::Left, &());

        let mut chunk = &[][..];
        let excerpt_bytes = if let Some(excerpt) = excerpts.item() {
            let mut excerpt_bytes = excerpt.reversed_bytes_in_range(
                range.start.saturating_sub(*excerpts.start())..range.end - *excerpts.start(),
            );
            chunk = excerpt_bytes.next().unwrap_or(&[][..]);
            Some(excerpt_bytes)
        } else {
            None
        };

        ReversedMultiBufferBytes {
            range,
            excerpts,
            excerpt_bytes,
            chunk,
        }
    }

    pub fn buffer_rows(&self, start_row: MultiBufferRow) -> MultiBufferRows {
        let mut result = MultiBufferRows {
            buffer_row_range: 0..0,
            excerpts: self.excerpts.cursor(&()),
        };
        result.seek(start_row);
        result
    }

    pub fn chunks<T: ToOffset>(&self, range: Range<T>, language_aware: bool) -> MultiBufferChunks {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let mut chunks = MultiBufferChunks {
            range: range.clone(),
            excerpts: self.excerpts.cursor(&()),
            excerpt_chunks: None,
            language_aware,
        };
        chunks.seek(range);
        chunks
    }

    pub fn offset_to_point(&self, offset: usize) -> Point {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.offset_to_point(offset);
        }

        let mut cursor = self.excerpts.cursor::<(usize, Point)>(&());
        cursor.seek(&offset, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_offset, start_point) = cursor.start();
            let overshoot = offset - start_offset;
            let excerpt_start_offset = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let excerpt_start_point = excerpt.range.context.start.to_point(&excerpt.buffer);
            let buffer_point = excerpt
                .buffer
                .offset_to_point(excerpt_start_offset + overshoot);
            *start_point + (buffer_point - excerpt_start_point)
        } else {
            self.excerpts.summary().text.lines
        }
    }

    pub fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.offset_to_point_utf16(offset);
        }

        let mut cursor = self.excerpts.cursor::<(usize, PointUtf16)>(&());
        cursor.seek(&offset, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_offset, start_point) = cursor.start();
            let overshoot = offset - start_offset;
            let excerpt_start_offset = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let excerpt_start_point = excerpt.range.context.start.to_point_utf16(&excerpt.buffer);
            let buffer_point = excerpt
                .buffer
                .offset_to_point_utf16(excerpt_start_offset + overshoot);
            *start_point + (buffer_point - excerpt_start_point)
        } else {
            self.excerpts.summary().text.lines_utf16()
        }
    }

    pub fn point_to_point_utf16(&self, point: Point) -> PointUtf16 {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.point_to_point_utf16(point);
        }

        let mut cursor = self.excerpts.cursor::<(Point, PointUtf16)>(&());
        cursor.seek(&point, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_offset, start_point) = cursor.start();
            let overshoot = point - start_offset;
            let excerpt_start_point = excerpt.range.context.start.to_point(&excerpt.buffer);
            let excerpt_start_point_utf16 =
                excerpt.range.context.start.to_point_utf16(&excerpt.buffer);
            let buffer_point = excerpt
                .buffer
                .point_to_point_utf16(excerpt_start_point + overshoot);
            *start_point + (buffer_point - excerpt_start_point_utf16)
        } else {
            self.excerpts.summary().text.lines_utf16()
        }
    }

    pub fn point_to_offset(&self, point: Point) -> usize {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.point_to_offset(point);
        }

        let mut cursor = self.excerpts.cursor::<(Point, usize)>(&());
        cursor.seek(&point, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_point, start_offset) = cursor.start();
            let overshoot = point - start_point;
            let excerpt_start_offset = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let excerpt_start_point = excerpt.range.context.start.to_point(&excerpt.buffer);
            let buffer_offset = excerpt
                .buffer
                .point_to_offset(excerpt_start_point + overshoot);
            *start_offset + buffer_offset - excerpt_start_offset
        } else {
            self.excerpts.summary().text.len
        }
    }

    pub fn offset_utf16_to_offset(&self, offset_utf16: OffsetUtf16) -> usize {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.offset_utf16_to_offset(offset_utf16);
        }

        let mut cursor = self.excerpts.cursor::<(OffsetUtf16, usize)>(&());
        cursor.seek(&offset_utf16, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_offset_utf16, start_offset) = cursor.start();
            let overshoot = offset_utf16 - start_offset_utf16;
            let excerpt_start_offset = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let excerpt_start_offset_utf16 =
                excerpt.buffer.offset_to_offset_utf16(excerpt_start_offset);
            let buffer_offset = excerpt
                .buffer
                .offset_utf16_to_offset(excerpt_start_offset_utf16 + overshoot);
            *start_offset + (buffer_offset - excerpt_start_offset)
        } else {
            self.excerpts.summary().text.len
        }
    }

    pub fn offset_to_offset_utf16(&self, offset: usize) -> OffsetUtf16 {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.offset_to_offset_utf16(offset);
        }

        let mut cursor = self.excerpts.cursor::<(usize, OffsetUtf16)>(&());
        cursor.seek(&offset, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_offset, start_offset_utf16) = cursor.start();
            let overshoot = offset - start_offset;
            let excerpt_start_offset_utf16 =
                excerpt.range.context.start.to_offset_utf16(&excerpt.buffer);
            let excerpt_start_offset = excerpt
                .buffer
                .offset_utf16_to_offset(excerpt_start_offset_utf16);
            let buffer_offset_utf16 = excerpt
                .buffer
                .offset_to_offset_utf16(excerpt_start_offset + overshoot);
            *start_offset_utf16 + (buffer_offset_utf16 - excerpt_start_offset_utf16)
        } else {
            self.excerpts.summary().text.len_utf16
        }
    }

    pub fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer.point_utf16_to_offset(point);
        }

        let mut cursor = self.excerpts.cursor::<(PointUtf16, usize)>(&());
        cursor.seek(&point, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let (start_point, start_offset) = cursor.start();
            let overshoot = point - start_point;
            let excerpt_start_offset = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let excerpt_start_point = excerpt
                .buffer
                .offset_to_point_utf16(excerpt.range.context.start.to_offset(&excerpt.buffer));
            let buffer_offset = excerpt
                .buffer
                .point_utf16_to_offset(excerpt_start_point + overshoot);
            *start_offset + (buffer_offset - excerpt_start_offset)
        } else {
            self.excerpts.summary().text.len
        }
    }

    pub fn point_to_buffer_offset<T: ToOffset>(
        &self,
        point: T,
    ) -> Option<(&BufferSnapshot, usize)> {
        let offset = point.to_offset(self);
        let mut cursor = self.excerpts.cursor::<usize>(&());
        cursor.seek(&offset, Bias::Right, &());
        if cursor.item().is_none() {
            cursor.prev(&());
        }

        cursor.item().map(|excerpt| {
            let excerpt_start = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let buffer_point = excerpt_start + offset - *cursor.start();
            (&excerpt.buffer, buffer_point)
        })
    }

    pub fn suggested_indents(
        &self,
        rows: impl IntoIterator<Item = u32>,
        cx: &AppContext,
    ) -> BTreeMap<MultiBufferRow, IndentSize> {
        let mut result = BTreeMap::new();

        let mut rows_for_excerpt = Vec::new();
        let mut cursor = self.excerpts.cursor::<Point>(&());
        let mut rows = rows.into_iter().peekable();
        let mut prev_row = u32::MAX;
        let mut prev_language_indent_size = IndentSize::default();

        while let Some(row) = rows.next() {
            cursor.seek(&Point::new(row, 0), Bias::Right, &());
            let excerpt = match cursor.item() {
                Some(excerpt) => excerpt,
                _ => continue,
            };

            // Retrieve the language and indent size once for each disjoint region being indented.
            let single_indent_size = if row.saturating_sub(1) == prev_row {
                prev_language_indent_size
            } else {
                excerpt
                    .buffer
                    .language_indent_size_at(Point::new(row, 0), cx)
            };
            prev_language_indent_size = single_indent_size;
            prev_row = row;

            let start_buffer_row = excerpt.range.context.start.to_point(&excerpt.buffer).row;
            let start_multibuffer_row = cursor.start().row;

            rows_for_excerpt.push(row);
            while let Some(next_row) = rows.peek().copied() {
                if cursor.end(&()).row > next_row {
                    rows_for_excerpt.push(next_row);
                    rows.next();
                } else {
                    break;
                }
            }

            let buffer_rows = rows_for_excerpt
                .drain(..)
                .map(|row| start_buffer_row + row - start_multibuffer_row);
            let buffer_indents = excerpt
                .buffer
                .suggested_indents(buffer_rows, single_indent_size);
            let multibuffer_indents = buffer_indents.into_iter().map(|(row, indent)| {
                (
                    MultiBufferRow(start_multibuffer_row + row - start_buffer_row),
                    indent,
                )
            });
            result.extend(multibuffer_indents);
        }

        result
    }

    pub fn indent_size_for_line(&self, row: MultiBufferRow) -> IndentSize {
        if let Some((buffer, range)) = self.buffer_line_for_row(row) {
            let mut size = buffer.indent_size_for_line(range.start.row);
            size.len = size
                .len
                .min(range.end.column)
                .saturating_sub(range.start.column);
            size
        } else {
            IndentSize::spaces(0)
        }
    }

    pub fn indent_and_comment_for_line(&self, row: MultiBufferRow, cx: &AppContext) -> String {
        let mut indent = self.indent_size_for_line(row).chars().collect::<String>();

        if self.settings_at(0, cx).extend_comment_on_newline {
            if let Some(language_scope) = self.language_scope_at(Point::new(row.0, 0)) {
                let delimiters = language_scope.line_comment_prefixes();
                for delimiter in delimiters {
                    if *self
                        .chars_at(Point::new(row.0, indent.len() as u32))
                        .take(delimiter.chars().count())
                        .collect::<String>()
                        .as_str()
                        == **delimiter
                    {
                        indent.push_str(&delimiter);
                        break;
                    }
                }
            }
        }

        indent
    }

    pub fn prev_non_blank_row(&self, mut row: MultiBufferRow) -> Option<MultiBufferRow> {
        while row.0 > 0 {
            row.0 -= 1;
            if !self.is_line_blank(row) {
                return Some(row);
            }
        }
        None
    }

    pub fn line_len(&self, row: MultiBufferRow) -> u32 {
        if let Some((_, range)) = self.buffer_line_for_row(row) {
            range.end.column - range.start.column
        } else {
            0
        }
    }

    pub fn buffer_line_for_row(
        &self,
        row: MultiBufferRow,
    ) -> Option<(&BufferSnapshot, Range<Point>)> {
        let mut cursor = self.excerpts.cursor::<Point>(&());
        let point = Point::new(row.0, 0);
        cursor.seek(&point, Bias::Right, &());
        if cursor.item().is_none() && *cursor.start() == point {
            cursor.prev(&());
        }
        if let Some(excerpt) = cursor.item() {
            let overshoot = row.0 - cursor.start().row;
            let excerpt_start = excerpt.range.context.start.to_point(&excerpt.buffer);
            let excerpt_end = excerpt.range.context.end.to_point(&excerpt.buffer);
            let buffer_row = excerpt_start.row + overshoot;
            let line_start = Point::new(buffer_row, 0);
            let line_end = Point::new(buffer_row, excerpt.buffer.line_len(buffer_row));
            return Some((
                &excerpt.buffer,
                line_start.max(excerpt_start)..line_end.min(excerpt_end),
            ));
        }
        None
    }

    pub fn max_point(&self) -> Point {
        self.text_summary().lines
    }

    pub fn max_row(&self) -> MultiBufferRow {
        MultiBufferRow(self.text_summary().lines.row)
    }

    pub fn text_summary(&self) -> TextSummary {
        self.excerpts.summary().text.clone()
    }

    pub fn text_summary_for_range<D, O>(&self, range: Range<O>) -> D
    where
        D: TextDimension,
        O: ToOffset,
    {
        let mut summary = D::zero(&());
        let mut range = range.start.to_offset(self)..range.end.to_offset(self);
        let mut cursor = self.excerpts.cursor::<usize>(&());
        cursor.seek(&range.start, Bias::Right, &());
        if let Some(excerpt) = cursor.item() {
            let mut end_before_newline = cursor.end(&());
            if excerpt.has_trailing_newline {
                end_before_newline -= 1;
            }

            let excerpt_start = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let start_in_excerpt = excerpt_start + (range.start - cursor.start());
            let end_in_excerpt =
                excerpt_start + (cmp::min(end_before_newline, range.end) - cursor.start());
            summary.add_assign(
                &excerpt
                    .buffer
                    .text_summary_for_range(start_in_excerpt..end_in_excerpt),
            );

            if range.end > end_before_newline {
                summary.add_assign(&D::from_text_summary(&TextSummary::from("\n")));
            }

            cursor.next(&());
        }

        if range.end > *cursor.start() {
            summary.add_assign(&D::from_text_summary(&cursor.summary::<_, TextSummary>(
                &range.end,
                Bias::Right,
                &(),
            )));
            if let Some(excerpt) = cursor.item() {
                range.end = cmp::max(*cursor.start(), range.end);

                let excerpt_start = excerpt.range.context.start.to_offset(&excerpt.buffer);
                let end_in_excerpt = excerpt_start + (range.end - cursor.start());
                summary.add_assign(
                    &excerpt
                        .buffer
                        .text_summary_for_range(excerpt_start..end_in_excerpt),
                );
            }
        }

        summary
    }

    pub fn summary_for_anchor<D>(&self, anchor: &Anchor) -> D
    where
        D: TextDimension + Ord + Sub<D, Output = D>,
    {
        let mut cursor = self.excerpts.cursor::<ExcerptSummary>(&());
        let locator = self.excerpt_locator_for_id(anchor.excerpt_id);

        cursor.seek(locator, Bias::Left, &());
        if cursor.item().is_none() {
            cursor.next(&());
        }

        let mut position = D::from_text_summary(&cursor.start().text);
        if let Some(excerpt) = cursor.item() {
            if excerpt.id == anchor.excerpt_id {
                let excerpt_buffer_start =
                    excerpt.range.context.start.summary::<D>(&excerpt.buffer);
                let excerpt_buffer_end = excerpt.range.context.end.summary::<D>(&excerpt.buffer);
                let buffer_position = cmp::min(
                    excerpt_buffer_end,
                    anchor.text_anchor.summary::<D>(&excerpt.buffer),
                );
                if buffer_position > excerpt_buffer_start {
                    position.add_assign(&(buffer_position - excerpt_buffer_start));
                }
            }
        }
        position
    }

    pub fn summaries_for_anchors<'a, D, I>(&'a self, anchors: I) -> Vec<D>
    where
        D: TextDimension + Ord + Sub<D, Output = D>,
        I: 'a + IntoIterator<Item = &'a Anchor>,
    {
        if let Some((_, _, buffer)) = self.as_singleton() {
            return buffer
                .summaries_for_anchors(anchors.into_iter().map(|a| &a.text_anchor))
                .collect();
        }

        let mut anchors = anchors.into_iter().peekable();
        let mut cursor = self.excerpts.cursor::<ExcerptSummary>(&());
        let mut summaries = Vec::new();
        while let Some(anchor) = anchors.peek() {
            let excerpt_id = anchor.excerpt_id;
            let excerpt_anchors = iter::from_fn(|| {
                let anchor = anchors.peek()?;
                if anchor.excerpt_id == excerpt_id {
                    Some(&anchors.next().unwrap().text_anchor)
                } else {
                    None
                }
            });

            let locator = self.excerpt_locator_for_id(excerpt_id);
            cursor.seek_forward(locator, Bias::Left, &());
            if cursor.item().is_none() {
                cursor.next(&());
            }

            let position = D::from_text_summary(&cursor.start().text);
            if let Some(excerpt) = cursor.item() {
                if excerpt.id == excerpt_id {
                    let excerpt_buffer_start =
                        excerpt.range.context.start.summary::<D>(&excerpt.buffer);
                    let excerpt_buffer_end =
                        excerpt.range.context.end.summary::<D>(&excerpt.buffer);
                    summaries.extend(
                        excerpt
                            .buffer
                            .summaries_for_anchors::<D, _>(excerpt_anchors)
                            .map(move |summary| {
                                let summary = cmp::min(excerpt_buffer_end.clone(), summary);
                                let mut position = position.clone();
                                let excerpt_buffer_start = excerpt_buffer_start.clone();
                                if summary > excerpt_buffer_start {
                                    position.add_assign(&(summary - excerpt_buffer_start));
                                }
                                position
                            }),
                    );
                    continue;
                }
            }

            summaries.extend(excerpt_anchors.map(|_| position.clone()));
        }

        summaries
    }

    pub fn dimensions_from_points<'a, D>(
        &'a self,
        points: impl 'a + IntoIterator<Item = Point>,
    ) -> impl 'a + Iterator<Item = D>
    where
        D: TextDimension,
    {
        let mut cursor = self.excerpts.cursor::<TextSummary>(&());
        let mut memoized_source_start: Option<Point> = None;
        let mut points = points.into_iter();
        std::iter::from_fn(move || {
            let point = points.next()?;

            // Clear the memoized source start if the point is in a different excerpt than previous.
            if memoized_source_start.map_or(false, |_| point >= cursor.end(&()).lines) {
                memoized_source_start = None;
            }

            // Now determine where the excerpt containing the point starts in its source buffer.
            // We'll use this value to calculate overshoot next.
            let source_start = if let Some(source_start) = memoized_source_start {
                source_start
            } else {
                cursor.seek_forward(&point, Bias::Right, &());
                if let Some(excerpt) = cursor.item() {
                    let source_start = excerpt.range.context.start.to_point(&excerpt.buffer);
                    memoized_source_start = Some(source_start);
                    source_start
                } else {
                    return Some(D::from_text_summary(cursor.start()));
                }
            };

            // First, assume the output dimension is at least the start of the excerpt containing the point
            let mut output = D::from_text_summary(cursor.start());

            // If the point lands within its excerpt, calculate and add the overshoot in dimension D.
            if let Some(excerpt) = cursor.item() {
                let overshoot = point - cursor.start().lines;
                if !overshoot.is_zero() {
                    let end_in_excerpt = source_start + overshoot;
                    output.add_assign(
                        &excerpt
                            .buffer
                            .text_summary_for_range::<D, _>(source_start..end_in_excerpt),
                    );
                }
            }
            Some(output)
        })
    }

    pub fn refresh_anchors<'a, I>(&'a self, anchors: I) -> Vec<(usize, Anchor, bool)>
    where
        I: 'a + IntoIterator<Item = &'a Anchor>,
    {
        let mut anchors = anchors.into_iter().enumerate().peekable();
        let mut cursor = self.excerpts.cursor::<Option<&Locator>>(&());
        cursor.next(&());

        let mut result = Vec::new();

        while let Some((_, anchor)) = anchors.peek() {
            let old_excerpt_id = anchor.excerpt_id;

            // Find the location where this anchor's excerpt should be.
            let old_locator = self.excerpt_locator_for_id(old_excerpt_id);
            cursor.seek_forward(&Some(old_locator), Bias::Left, &());

            if cursor.item().is_none() {
                cursor.next(&());
            }

            let next_excerpt = cursor.item();
            let prev_excerpt = cursor.prev_item();

            // Process all of the anchors for this excerpt.
            while let Some((_, anchor)) = anchors.peek() {
                if anchor.excerpt_id != old_excerpt_id {
                    break;
                }
                let (anchor_ix, anchor) = anchors.next().unwrap();
                let mut anchor = *anchor;

                // Leave min and max anchors unchanged if invalid or
                // if the old excerpt still exists at this location
                let mut kept_position = next_excerpt
                    .map_or(false, |e| e.id == old_excerpt_id && e.contains(&anchor))
                    || old_excerpt_id == ExcerptId::max()
                    || old_excerpt_id == ExcerptId::min();

                // If the old excerpt no longer exists at this location, then attempt to
                // find an equivalent position for this anchor in an adjacent excerpt.
                if !kept_position {
                    for excerpt in [next_excerpt, prev_excerpt].iter().filter_map(|e| *e) {
                        if excerpt.contains(&anchor) {
                            anchor.excerpt_id = excerpt.id;
                            kept_position = true;
                            break;
                        }
                    }
                }

                // If there's no adjacent excerpt that contains the anchor's position,
                // then report that the anchor has lost its position.
                if !kept_position {
                    anchor = if let Some(excerpt) = next_excerpt {
                        let mut text_anchor = excerpt
                            .range
                            .context
                            .start
                            .bias(anchor.text_anchor.bias, &excerpt.buffer);
                        if text_anchor
                            .cmp(&excerpt.range.context.end, &excerpt.buffer)
                            .is_gt()
                        {
                            text_anchor = excerpt.range.context.end;
                        }
                        Anchor {
                            buffer_id: Some(excerpt.buffer_id),
                            excerpt_id: excerpt.id,
                            text_anchor,
                        }
                    } else if let Some(excerpt) = prev_excerpt {
                        let mut text_anchor = excerpt
                            .range
                            .context
                            .end
                            .bias(anchor.text_anchor.bias, &excerpt.buffer);
                        if text_anchor
                            .cmp(&excerpt.range.context.start, &excerpt.buffer)
                            .is_lt()
                        {
                            text_anchor = excerpt.range.context.start;
                        }
                        Anchor {
                            buffer_id: Some(excerpt.buffer_id),
                            excerpt_id: excerpt.id,
                            text_anchor,
                        }
                    } else if anchor.text_anchor.bias == Bias::Left {
                        Anchor::min()
                    } else {
                        Anchor::max()
                    };
                }

                result.push((anchor_ix, anchor, kept_position));
            }
        }
        result.sort_unstable_by(|a, b| a.1.cmp(&b.1, self));
        result
    }

    pub fn anchor_before<T: ToOffset>(&self, position: T) -> Anchor {
        self.anchor_at(position, Bias::Left)
    }

    pub fn anchor_after<T: ToOffset>(&self, position: T) -> Anchor {
        self.anchor_at(position, Bias::Right)
    }

    pub fn anchor_at<T: ToOffset>(&self, position: T, mut bias: Bias) -> Anchor {
        let offset = position.to_offset(self);
        if let Some((excerpt_id, buffer_id, buffer)) = self.as_singleton() {
            return Anchor {
                buffer_id: Some(buffer_id),
                excerpt_id: *excerpt_id,
                text_anchor: buffer.anchor_at(offset, bias),
            };
        }

        let mut cursor = self.excerpts.cursor::<(usize, Option<ExcerptId>)>(&());
        cursor.seek(&offset, Bias::Right, &());
        if cursor.item().is_none() && offset == cursor.start().0 && bias == Bias::Left {
            cursor.prev(&());
        }
        if let Some(excerpt) = cursor.item() {
            let mut overshoot = offset.saturating_sub(cursor.start().0);
            if excerpt.has_trailing_newline && offset == cursor.end(&()).0 {
                overshoot -= 1;
                bias = Bias::Right;
            }

            let buffer_start = excerpt.range.context.start.to_offset(&excerpt.buffer);
            let text_anchor =
                excerpt.clip_anchor(excerpt.buffer.anchor_at(buffer_start + overshoot, bias));
            Anchor {
                buffer_id: Some(excerpt.buffer_id),
                excerpt_id: excerpt.id,
                text_anchor,
            }
        } else if offset == 0 && bias == Bias::Left {
            Anchor::min()
        } else {
            Anchor::max()
        }
    }

    /// Returns an anchor for the given excerpt and text anchor,
    /// returns None if the excerpt_id is no longer valid.
    pub fn anchor_in_excerpt(
        &self,
        excerpt_id: ExcerptId,
        text_anchor: text::Anchor,
    ) -> Option<Anchor> {
        let locator = self.excerpt_locator_for_id(excerpt_id);
        let mut cursor = self.excerpts.cursor::<Option<&Locator>>(&());
        cursor.seek(locator, Bias::Left, &());
        if let Some(excerpt) = cursor.item() {
            if excerpt.id == excerpt_id {
                let text_anchor = excerpt.clip_anchor(text_anchor);
                drop(cursor);
                return Some(Anchor {
                    buffer_id: Some(excerpt.buffer_id),
                    excerpt_id,
                    text_anchor,
                });
            }
        }
        None
    }

    pub fn context_range_for_excerpt(&self, excerpt_id: ExcerptId) -> Option<Range<text::Anchor>> {
        Some(self.excerpt(excerpt_id)?.range.context.clone())
    }

    pub fn can_resolve(&self, anchor: &Anchor) -> bool {
        if anchor.excerpt_id == ExcerptId::min() || anchor.excerpt_id == ExcerptId::max() {
            true
        } else if let Some(excerpt) = self.excerpt(anchor.excerpt_id) {
            excerpt.buffer.can_resolve(&anchor.text_anchor)
        } else {
            false
        }
    }

    pub fn buffer_ids_in_selected_rows(
        &self,
        selection: Selection<Point>,
    ) -> impl Iterator<Item = BufferId> + '_ {
        let mut cursor = self.excerpts.cursor::<Point>(&());
        cursor.seek(&Point::new(selection.start.row, 0), Bias::Right, &());
        cursor.prev(&());

        iter::from_fn(move || {
            cursor.next(&());
            if cursor.start().row <= selection.end.row {
                cursor.item().map(|item| item.buffer_id)
            } else {
                None
            }
        })
    }

    pub fn excerpts(
        &self,
    ) -> impl Iterator<Item = (ExcerptId, &BufferSnapshot, ExcerptRange<text::Anchor>)> {
        self.excerpts
            .iter()
            .map(|excerpt| (excerpt.id, &excerpt.buffer, excerpt.range.clone()))
    }

    pub fn all_excerpts(&self) -> impl Iterator<Item = MultiBufferExcerpt> {
        let mut cursor = self.excerpts.cursor::<(usize, Point)>(&());
        cursor.next(&());
        std::iter::from_fn(move || {
            let excerpt = cursor.item()?;
            let excerpt = MultiBufferExcerpt::new(excerpt, *cursor.start());
            cursor.next(&());
            Some(excerpt)
        })
    }

    pub fn excerpts_for_range<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> impl Iterator<Item = MultiBufferExcerpt> + '_ {
        let range = range.start.to_offset(self)..range.end.to_offset(self);

        let mut cursor = self.excerpts.cursor::<(usize, Point)>(&());
        cursor.seek(&range.start, Bias::Right, &());
        cursor.prev(&());

        iter::from_fn(move || {
            cursor.next(&());
            if cursor.start().0 < range.end {
                cursor
                    .item()
                    .map(|item| MultiBufferExcerpt::new(item, *cursor.start()))
            } else {
                None
            }
        })
    }

    pub fn excerpts_for_range_rev<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> impl Iterator<Item = MultiBufferExcerpt> + '_ {
        let range = range.start.to_offset(self)..range.end.to_offset(self);

        let mut cursor = self.excerpts.cursor::<(usize, Point)>(&());
        cursor.seek(&range.end, Bias::Left, &());
        if cursor.item().is_none() {
            cursor.prev(&());
        }

        std::iter::from_fn(move || {
            let excerpt = cursor.item()?;
            let excerpt = MultiBufferExcerpt::new(excerpt, *cursor.start());
            cursor.prev(&());
            Some(excerpt)
        })
    }

    pub fn excerpt_before(&self, id: ExcerptId) -> Option<MultiBufferExcerpt<'_>> {
        let start_locator = self.excerpt_locator_for_id(id);
        let mut cursor = self.excerpts.cursor::<ExcerptSummary>(&());
        cursor.seek(start_locator, Bias::Left, &());
        cursor.prev(&());
        let excerpt = cursor.item()?;
        let excerpt_offset = cursor.start().text.len;
        let excerpt_position = cursor.start().text.lines;
        Some(MultiBufferExcerpt {
            excerpt,
            excerpt_offset,
            excerpt_position,
        })
    }

    pub fn excerpt_after(&self, id: ExcerptId) -> Option<MultiBufferExcerpt<'_>> {
        let start_locator = self.excerpt_locator_for_id(id);
        let mut cursor = self.excerpts.cursor::<ExcerptSummary>(&());
        cursor.seek(start_locator, Bias::Left, &());
        cursor.next(&());
        let excerpt = cursor.item()?;
        let excerpt_offset = cursor.start().text.len;
        let excerpt_position = cursor.start().text.lines;
        Some(MultiBufferExcerpt {
            excerpt,
            excerpt_offset,
            excerpt_position,
        })
    }

    pub fn excerpt_boundaries_in_range<R, T>(
        &self,
        range: R,
    ) -> impl Iterator<Item = ExcerptBoundary> + '_
    where
        R: RangeBounds<T>,
        T: ToOffset,
    {
        let start_offset;
        let start = match range.start_bound() {
            Bound::Included(start) => {
                start_offset = start.to_offset(self);
                Bound::Included(start_offset)
            }
            Bound::Excluded(start) => {
                start_offset = start.to_offset(self);
                Bound::Excluded(start_offset)
            }
            Bound::Unbounded => {
                start_offset = 0;
                Bound::Unbounded
            }
        };
        let end = match range.end_bound() {
            Bound::Included(end) => Bound::Included(end.to_offset(self)),
            Bound::Excluded(end) => Bound::Excluded(end.to_offset(self)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let bounds = (start, end);

        let mut cursor = self.excerpts.cursor::<(usize, Point)>(&());
        cursor.seek(&start_offset, Bias::Right, &());
        if cursor.item().is_none() {
            cursor.prev(&());
        }
        if !bounds.contains(&cursor.start().0) {
            cursor.next(&());
        }

        let mut visited_end = false;
        std::iter::from_fn(move || {
            if self.singleton {
                None
            } else if bounds.contains(&cursor.start().0) {
                let next = cursor.item().map(|excerpt| ExcerptInfo {
                    id: excerpt.id,
                    buffer: excerpt.buffer.clone(),
                    buffer_id: excerpt.buffer_id,
                    range: excerpt.range.clone(),
                    text_summary: excerpt.text_summary.clone(),
                });

                if next.is_none() {
                    if visited_end {
                        return None;
                    } else {
                        visited_end = true;
                    }
                }

                let prev = cursor.prev_item().map(|prev_excerpt| ExcerptInfo {
                    id: prev_excerpt.id,
                    buffer: prev_excerpt.buffer.clone(),
                    buffer_id: prev_excerpt.buffer_id,
                    range: prev_excerpt.range.clone(),
                    text_summary: prev_excerpt.text_summary.clone(),
                });
                let row = MultiBufferRow(cursor.start().1.row);

                cursor.next(&());

                Some(ExcerptBoundary { row, prev, next })
            } else {
                None
            }
        })
    }

    pub fn edit_count(&self) -> usize {
        self.edit_count
    }

    pub fn non_text_state_update_count(&self) -> usize {
        self.non_text_state_update_count
    }

    /// Returns the smallest enclosing bracket ranges containing the given range or
    /// None if no brackets contain range or the range is not contained in a single
    /// excerpt
    ///
    /// Can optionally pass a range_filter to filter the ranges of brackets to consider
    pub fn innermost_enclosing_bracket_ranges<T: ToOffset>(
        &self,
        range: Range<T>,
        range_filter: Option<&dyn Fn(Range<usize>, Range<usize>) -> bool>,
    ) -> Option<(Range<usize>, Range<usize>)> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let excerpt = self.excerpt_containing(range.clone())?;

        // Filter to ranges contained in the excerpt
        let range_filter = |open: Range<usize>, close: Range<usize>| -> bool {
            excerpt.contains_buffer_range(open.start..close.end)
                && range_filter.map_or(true, |filter| {
                    filter(
                        excerpt.map_range_from_buffer(open),
                        excerpt.map_range_from_buffer(close),
                    )
                })
        };

        let (open, close) = excerpt.buffer().innermost_enclosing_bracket_ranges(
            excerpt.map_range_to_buffer(range),
            Some(&range_filter),
        )?;

        Some((
            excerpt.map_range_from_buffer(open),
            excerpt.map_range_from_buffer(close),
        ))
    }

    /// Returns enclosing bracket ranges containing the given range or returns None if the range is
    /// not contained in a single excerpt
    pub fn enclosing_bracket_ranges<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> Option<impl Iterator<Item = (Range<usize>, Range<usize>)> + '_> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let excerpt = self.excerpt_containing(range.clone())?;

        Some(
            excerpt
                .buffer()
                .enclosing_bracket_ranges(excerpt.map_range_to_buffer(range))
                .filter_map(move |(open, close)| {
                    if excerpt.contains_buffer_range(open.start..close.end) {
                        Some((
                            excerpt.map_range_from_buffer(open),
                            excerpt.map_range_from_buffer(close),
                        ))
                    } else {
                        None
                    }
                }),
        )
    }

    /// Returns bracket range pairs overlapping the given `range` or returns None if the `range` is
    /// not contained in a single excerpt
    pub fn bracket_ranges<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> Option<impl Iterator<Item = (Range<usize>, Range<usize>)> + '_> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let excerpt = self.excerpt_containing(range.clone())?;

        Some(
            excerpt
                .buffer()
                .bracket_ranges(excerpt.map_range_to_buffer(range))
                .filter_map(move |(start_bracket_range, close_bracket_range)| {
                    let buffer_range = start_bracket_range.start..close_bracket_range.end;
                    if excerpt.contains_buffer_range(buffer_range) {
                        Some((
                            excerpt.map_range_from_buffer(start_bracket_range),
                            excerpt.map_range_from_buffer(close_bracket_range),
                        ))
                    } else {
                        None
                    }
                }),
        )
    }

    pub fn redacted_ranges<'a, T: ToOffset>(
        &'a self,
        range: Range<T>,
        redaction_enabled: impl Fn(Option<&Arc<dyn File>>) -> bool + 'a,
    ) -> impl Iterator<Item = Range<usize>> + 'a {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        self.excerpts_for_range(range.clone())
            .filter(move |excerpt| redaction_enabled(excerpt.buffer().file()))
            .flat_map(move |excerpt| {
                excerpt
                    .buffer()
                    .redacted_ranges(excerpt.buffer_range().clone())
                    .map(move |redacted_range| excerpt.map_range_from_buffer(redacted_range))
                    .skip_while(move |redacted_range| redacted_range.end < range.start)
                    .take_while(move |redacted_range| redacted_range.start < range.end)
            })
    }

    pub fn runnable_ranges(
        &self,
        range: Range<Anchor>,
    ) -> impl Iterator<Item = language::RunnableRange> + '_ {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        self.excerpts_for_range(range.clone())
            .flat_map(move |excerpt| {
                let excerpt_buffer_start =
                    excerpt.buffer_range().start.to_offset(&excerpt.buffer());

                excerpt
                    .buffer()
                    .runnable_ranges(excerpt.buffer_range())
                    .filter_map(move |mut runnable| {
                        // Re-base onto the excerpts coordinates in the multibuffer
                        //
                        // The node matching our runnables query might partially overlap with
                        // the provided range. If the run indicator is outside of excerpt bounds, do not actually show it.
                        if runnable.run_range.start < excerpt_buffer_start {
                            return None;
                        }
                        if language::ToPoint::to_point(&runnable.run_range.end, &excerpt.buffer())
                            .row
                            > excerpt.max_buffer_row()
                        {
                            return None;
                        }
                        runnable.run_range = excerpt.map_range_from_buffer(runnable.run_range);

                        Some(runnable)
                    })
                    .skip_while(move |runnable| runnable.run_range.end < range.start)
                    .take_while(move |runnable| runnable.run_range.start < range.end)
            })
    }

    pub fn indent_guides_in_range(
        &self,
        range: Range<Anchor>,
        ignore_disabled_for_language: bool,
        cx: &AppContext,
    ) -> Vec<MultiBufferIndentGuide> {
        // Fast path for singleton buffers, we can skip the conversion between offsets.
        if let Some((_, _, snapshot)) = self.as_singleton() {
            return snapshot
                .indent_guides_in_range(
                    range.start.text_anchor..range.end.text_anchor,
                    ignore_disabled_for_language,
                    cx,
                )
                .into_iter()
                .map(|guide| MultiBufferIndentGuide {
                    multibuffer_row_range: MultiBufferRow(guide.start_row)
                        ..MultiBufferRow(guide.end_row),
                    buffer: guide,
                })
                .collect();
        }

        let range = range.start.to_offset(self)..range.end.to_offset(self);

        self.excerpts_for_range(range.clone())
            .flat_map(move |excerpt| {
                let excerpt_buffer_start_row =
                    excerpt.buffer_range().start.to_point(&excerpt.buffer()).row;
                let excerpt_offset_row = excerpt.start_point().row;

                excerpt
                    .buffer()
                    .indent_guides_in_range(
                        excerpt.buffer_range(),
                        ignore_disabled_for_language,
                        cx,
                    )
                    .into_iter()
                    .map(move |indent_guide| {
                        let start_row = excerpt_offset_row
                            + (indent_guide.start_row - excerpt_buffer_start_row);
                        let end_row =
                            excerpt_offset_row + (indent_guide.end_row - excerpt_buffer_start_row);

                        MultiBufferIndentGuide {
                            multibuffer_row_range: MultiBufferRow(start_row)
                                ..MultiBufferRow(end_row),
                            buffer: indent_guide,
                        }
                    })
            })
            .collect()
    }

    pub fn trailing_excerpt_update_count(&self) -> usize {
        self.trailing_excerpt_update_count
    }

    pub fn file_at<T: ToOffset>(&self, point: T) -> Option<&Arc<dyn File>> {
        self.point_to_buffer_offset(point)
            .and_then(|(buffer, _)| buffer.file())
    }

    pub fn language_at<T: ToOffset>(&self, point: T) -> Option<&Arc<Language>> {
        self.point_to_buffer_offset(point)
            .and_then(|(buffer, offset)| buffer.language_at(offset))
    }

    pub fn settings_at<'a, T: ToOffset>(
        &'a self,
        point: T,
        cx: &'a AppContext,
    ) -> Cow<'a, LanguageSettings> {
        let mut language = None;
        let mut file = None;
        if let Some((buffer, offset)) = self.point_to_buffer_offset(point) {
            language = buffer.language_at(offset);
            file = buffer.file();
        }
        language_settings(language.map(|l| l.name()), file, cx)
    }

    pub fn language_scope_at<T: ToOffset>(&self, point: T) -> Option<LanguageScope> {
        self.point_to_buffer_offset(point)
            .and_then(|(buffer, offset)| buffer.language_scope_at(offset))
    }

    pub fn char_classifier_at<T: ToOffset>(&self, point: T) -> CharClassifier {
        self.point_to_buffer_offset(point)
            .map(|(buffer, offset)| buffer.char_classifier_at(offset))
            .unwrap_or_default()
    }

    pub fn language_indent_size_at<T: ToOffset>(
        &self,
        position: T,
        cx: &AppContext,
    ) -> Option<IndentSize> {
        let (buffer_snapshot, offset) = self.point_to_buffer_offset(position)?;
        Some(buffer_snapshot.language_indent_size_at(offset, cx))
    }

    pub fn is_dirty(&self) -> bool {
        self.is_dirty
    }

    pub fn has_deleted_file(&self) -> bool {
        self.has_deleted_file
    }

    pub fn has_conflict(&self) -> bool {
        self.has_conflict
    }

    pub fn has_diagnostics(&self) -> bool {
        self.excerpts
            .iter()
            .any(|excerpt| excerpt.buffer.has_diagnostics())
    }

    pub fn diagnostic_group<'a, O>(
        &'a self,
        group_id: usize,
    ) -> impl Iterator<Item = DiagnosticEntry<O>> + 'a
    where
        O: text::FromAnchor + 'a,
    {
        self.as_singleton()
            .into_iter()
            .flat_map(move |(_, _, buffer)| buffer.diagnostic_group(group_id))
    }

    pub fn diagnostics_in_range<'a, T, O>(
        &'a self,
        range: Range<T>,
        reversed: bool,
    ) -> impl Iterator<Item = DiagnosticEntry<O>> + 'a
    where
        T: 'a + ToOffset,
        O: 'a + text::FromAnchor + Ord,
    {
        self.as_singleton()
            .into_iter()
            .flat_map(move |(_, _, buffer)| {
                buffer.diagnostics_in_range(
                    range.start.to_offset(self)..range.end.to_offset(self),
                    reversed,
                )
            })
    }

    pub fn syntax_ancestor<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> Option<(tree_sitter::Node, Range<usize>)> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        let excerpt = self.excerpt_containing(range.clone())?;
        let node = excerpt
            .buffer()
            .syntax_ancestor(excerpt.map_range_to_buffer(range))?;
        Some((node, excerpt.map_range_from_buffer(node.byte_range())))
    }

    pub fn outline(&self, theme: Option<&SyntaxTheme>) -> Option<Outline<Anchor>> {
        let (excerpt_id, _, buffer) = self.as_singleton()?;
        let outline = buffer.outline(theme)?;
        Some(Outline::new(
            outline
                .items
                .into_iter()
                .flat_map(|item| {
                    Some(OutlineItem {
                        depth: item.depth,
                        range: self.anchor_in_excerpt(*excerpt_id, item.range.start)?
                            ..self.anchor_in_excerpt(*excerpt_id, item.range.end)?,
                        text: item.text,
                        highlight_ranges: item.highlight_ranges,
                        name_ranges: item.name_ranges,
                        body_range: item.body_range.and_then(|body_range| {
                            Some(
                                self.anchor_in_excerpt(*excerpt_id, body_range.start)?
                                    ..self.anchor_in_excerpt(*excerpt_id, body_range.end)?,
                            )
                        }),
                        annotation_range: item.annotation_range.and_then(|annotation_range| {
                            Some(
                                self.anchor_in_excerpt(*excerpt_id, annotation_range.start)?
                                    ..self.anchor_in_excerpt(*excerpt_id, annotation_range.end)?,
                            )
                        }),
                    })
                })
                .collect(),
        ))
    }

    pub fn symbols_containing<T: ToOffset>(
        &self,
        offset: T,
        theme: Option<&SyntaxTheme>,
    ) -> Option<(BufferId, Vec<OutlineItem<Anchor>>)> {
        let anchor = self.anchor_before(offset);
        let excerpt_id = anchor.excerpt_id;
        let excerpt = self.excerpt(excerpt_id)?;
        Some((
            excerpt.buffer_id,
            excerpt
                .buffer
                .symbols_containing(anchor.text_anchor, theme)
                .into_iter()
                .flatten()
                .flat_map(|item| {
                    Some(OutlineItem {
                        depth: item.depth,
                        range: self.anchor_in_excerpt(excerpt_id, item.range.start)?
                            ..self.anchor_in_excerpt(excerpt_id, item.range.end)?,
                        text: item.text,
                        highlight_ranges: item.highlight_ranges,
                        name_ranges: item.name_ranges,
                        body_range: item.body_range.and_then(|body_range| {
                            Some(
                                self.anchor_in_excerpt(excerpt_id, body_range.start)?
                                    ..self.anchor_in_excerpt(excerpt_id, body_range.end)?,
                            )
                        }),
                        annotation_range: item.annotation_range.and_then(|body_range| {
                            Some(
                                self.anchor_in_excerpt(excerpt_id, body_range.start)?
                                    ..self.anchor_in_excerpt(excerpt_id, body_range.end)?,
                            )
                        }),
                    })
                })
                .collect(),
        ))
    }

    fn excerpt_locator_for_id(&self, id: ExcerptId) -> &Locator {
        if id == ExcerptId::min() {
            Locator::min_ref()
        } else if id == ExcerptId::max() {
            Locator::max_ref()
        } else {
            let mut cursor = self.excerpt_ids.cursor::<ExcerptId>(&());
            cursor.seek(&id, Bias::Left, &());
            if let Some(entry) = cursor.item() {
                if entry.id == id {
                    return &entry.locator;
                }
            }
            panic!("invalid excerpt id {:?}", id)
        }
    }

    /// Returns the locators referenced by the given excerpt IDs, sorted by locator.
    fn excerpt_locators_for_ids(
        &self,
        ids: impl IntoIterator<Item = ExcerptId>,
    ) -> SmallVec<[Locator; 1]> {
        let mut sorted_ids = ids.into_iter().collect::<SmallVec<[_; 1]>>();
        sorted_ids.sort_unstable();
        let mut locators = SmallVec::new();

        while sorted_ids.last() == Some(&ExcerptId::max()) {
            sorted_ids.pop();
            if let Some(mapping) = self.excerpt_ids.last() {
                locators.push(mapping.locator.clone());
            }
        }

        let mut sorted_ids = sorted_ids.into_iter().dedup().peekable();
        if sorted_ids.peek() == Some(&ExcerptId::min()) {
            sorted_ids.next();
            if let Some(mapping) = self.excerpt_ids.first() {
                locators.push(mapping.locator.clone());
            }
        }

        let mut cursor = self.excerpt_ids.cursor::<ExcerptId>(&());
        for id in sorted_ids {
            if cursor.seek_forward(&id, Bias::Left, &()) {
                locators.push(cursor.item().unwrap().locator.clone());
            } else {
                panic!("invalid excerpt id {:?}", id);
            }
        }

        locators.sort_unstable();
        locators
    }

    pub fn buffer_id_for_excerpt(&self, excerpt_id: ExcerptId) -> Option<BufferId> {
        Some(self.excerpt(excerpt_id)?.buffer_id)
    }

    pub fn buffer_for_excerpt(&self, excerpt_id: ExcerptId) -> Option<&BufferSnapshot> {
        Some(&self.excerpt(excerpt_id)?.buffer)
    }

    pub fn range_for_excerpt<'a, T: sum_tree::Dimension<'a, ExcerptSummary>>(
        &'a self,
        excerpt_id: ExcerptId,
    ) -> Option<Range<T>> {
        let mut cursor = self.excerpts.cursor::<(Option<&Locator>, T)>(&());
        let locator = self.excerpt_locator_for_id(excerpt_id);
        if cursor.seek(&Some(locator), Bias::Left, &()) {
            let start = cursor.start().1.clone();
            let end = cursor.end(&()).1;
            Some(start..end)
        } else {
            None
        }
    }

    fn excerpt(&self, excerpt_id: ExcerptId) -> Option<&Excerpt> {
        let mut cursor = self.excerpts.cursor::<Option<&Locator>>(&());
        let locator = self.excerpt_locator_for_id(excerpt_id);
        cursor.seek(&Some(locator), Bias::Left, &());
        if let Some(excerpt) = cursor.item() {
            if excerpt.id == excerpt_id {
                return Some(excerpt);
            }
        }
        None
    }

    /// Returns the excerpt containing range and its offset start within the multibuffer or none if `range` spans multiple excerpts
    pub fn excerpt_containing<T: ToOffset>(&self, range: Range<T>) -> Option<MultiBufferExcerpt> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);

        let mut cursor = self.excerpts.cursor::<(usize, Point)>(&());
        cursor.seek(&range.start, Bias::Right, &());
        let start_excerpt = cursor.item()?;

        if range.start == range.end {
            return Some(MultiBufferExcerpt::new(start_excerpt, *cursor.start()));
        }

        cursor.seek(&range.end, Bias::Right, &());
        let end_excerpt = cursor.item()?;

        if start_excerpt.id == end_excerpt.id {
            Some(MultiBufferExcerpt::new(start_excerpt, *cursor.start()))
        } else {
            None
        }
    }

    // Takes an iterator over anchor ranges and returns a new iterator over anchor ranges that don't
    // span across excerpt boundaries.
    pub fn split_ranges<'a, I>(&'a self, ranges: I) -> impl Iterator<Item = Range<Anchor>> + 'a
    where
        I: IntoIterator<Item = Range<Anchor>> + 'a,
    {
        let mut ranges = ranges.into_iter().map(|range| range.to_offset(self));
        let mut cursor = self.excerpts.cursor::<(usize, Point)>(&());
        cursor.next(&());
        let mut current_range = ranges.next();
        iter::from_fn(move || {
            let range = current_range.clone()?;
            if range.start >= cursor.end(&()).0 {
                cursor.seek_forward(&range.start, Bias::Right, &());
                if range.start == self.len() {
                    cursor.prev(&());
                }
            }

            let excerpt = cursor.item()?;
            let range_start_in_excerpt = cmp::max(range.start, cursor.start().0);
            let range_end_in_excerpt = if excerpt.has_trailing_newline {
                cmp::min(range.end, cursor.end(&()).0 - 1)
            } else {
                cmp::min(range.end, cursor.end(&()).0)
            };
            let buffer_range = MultiBufferExcerpt::new(excerpt, *cursor.start())
                .map_range_to_buffer(range_start_in_excerpt..range_end_in_excerpt);

            let subrange_start_anchor = Anchor {
                buffer_id: Some(excerpt.buffer_id),
                excerpt_id: excerpt.id,
                text_anchor: excerpt.buffer.anchor_before(buffer_range.start),
            };
            let subrange_end_anchor = Anchor {
                buffer_id: Some(excerpt.buffer_id),
                excerpt_id: excerpt.id,
                text_anchor: excerpt.buffer.anchor_after(buffer_range.end),
            };

            if range.end > cursor.end(&()).0 {
                cursor.next(&());
            } else {
                current_range = ranges.next();
            }

            Some(subrange_start_anchor..subrange_end_anchor)
        })
    }

    /// Returns excerpts overlapping the given ranges. If range spans multiple excerpts returns one range for each excerpt
    ///
    /// The ranges are specified in the coordinate space of the multibuffer, not the individual excerpted buffers.
    /// Each returned excerpt's range is in the coordinate space of its source buffer.
    pub fn excerpts_in_ranges(
        &self,
        ranges: impl IntoIterator<Item = Range<Anchor>>,
    ) -> impl Iterator<Item = (ExcerptId, &BufferSnapshot, Range<usize>)> {
        let mut ranges = ranges.into_iter().map(|range| range.to_offset(self));
        let mut cursor = self.excerpts.cursor::<(usize, Point)>(&());
        cursor.next(&());
        let mut current_range = ranges.next();
        iter::from_fn(move || {
            let range = current_range.clone()?;
            if range.start >= cursor.end(&()).0 {
                cursor.seek_forward(&range.start, Bias::Right, &());
                if range.start == self.len() {
                    cursor.prev(&());
                }
            }

            let excerpt = cursor.item()?;
            let range_start_in_excerpt = cmp::max(range.start, cursor.start().0);
            let range_end_in_excerpt = if excerpt.has_trailing_newline {
                cmp::min(range.end, cursor.end(&()).0 - 1)
            } else {
                cmp::min(range.end, cursor.end(&()).0)
            };
            let buffer_range = MultiBufferExcerpt::new(excerpt, *cursor.start())
                .map_range_to_buffer(range_start_in_excerpt..range_end_in_excerpt);

            if range.end > cursor.end(&()).0 {
                cursor.next(&());
            } else {
                current_range = ranges.next();
            }

            Some((excerpt.id, &excerpt.buffer, buffer_range))
        })
    }

    pub fn selections_in_range<'a>(
        &'a self,
        range: &'a Range<Anchor>,
        include_local: bool,
    ) -> impl 'a + Iterator<Item = (ReplicaId, bool, CursorShape, Selection<Anchor>)> {
        let mut cursor = self.excerpts.cursor::<ExcerptSummary>(&());
        let start_locator = self.excerpt_locator_for_id(range.start.excerpt_id);
        let end_locator = self.excerpt_locator_for_id(range.end.excerpt_id);
        cursor.seek(start_locator, Bias::Left, &());
        cursor
            .take_while(move |excerpt| excerpt.locator <= *end_locator)
            .flat_map(move |excerpt| {
                let mut query_range = excerpt.range.context.start..excerpt.range.context.end;
                if excerpt.id == range.start.excerpt_id {
                    query_range.start = range.start.text_anchor;
                }
                if excerpt.id == range.end.excerpt_id {
                    query_range.end = range.end.text_anchor;
                }

                excerpt
                    .buffer
                    .selections_in_range(query_range, include_local)
                    .flat_map(move |(replica_id, line_mode, cursor_shape, selections)| {
                        selections.map(move |selection| {
                            let mut start = Anchor {
                                buffer_id: Some(excerpt.buffer_id),
                                excerpt_id: excerpt.id,
                                text_anchor: selection.start,
                            };
                            let mut end = Anchor {
                                buffer_id: Some(excerpt.buffer_id),
                                excerpt_id: excerpt.id,
                                text_anchor: selection.end,
                            };
                            if range.start.cmp(&start, self).is_gt() {
                                start = range.start;
                            }
                            if range.end.cmp(&end, self).is_lt() {
                                end = range.end;
                            }

                            (
                                replica_id,
                                line_mode,
                                cursor_shape,
                                Selection {
                                    id: selection.id,
                                    start,
                                    end,
                                    reversed: selection.reversed,
                                    goal: selection.goal,
                                },
                            )
                        })
                    })
            })
    }

    pub fn show_headers(&self) -> bool {
        self.show_headers
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MultiBufferSnapshot {
    pub fn random_byte_range(&self, start_offset: usize, rng: &mut impl rand::Rng) -> Range<usize> {
        let end = self.clip_offset(rng.gen_range(start_offset..=self.len()), Bias::Right);
        let start = self.clip_offset(rng.gen_range(start_offset..=end), Bias::Right);
        start..end
    }
}

impl History {
    fn start_transaction(&mut self, now: Instant) -> Option<TransactionId> {
        self.transaction_depth += 1;
        if self.transaction_depth == 1 {
            let id = self.next_transaction_id.tick();
            self.undo_stack.push(Transaction {
                id,
                buffer_transactions: Default::default(),
                first_edit_at: now,
                last_edit_at: now,
                suppress_grouping: false,
            });
            Some(id)
        } else {
            None
        }
    }

    fn end_transaction(
        &mut self,
        now: Instant,
        buffer_transactions: HashMap<BufferId, TransactionId>,
    ) -> bool {
        assert_ne!(self.transaction_depth, 0);
        self.transaction_depth -= 1;
        if self.transaction_depth == 0 {
            if buffer_transactions.is_empty() {
                self.undo_stack.pop();
                false
            } else {
                self.redo_stack.clear();
                let transaction = self.undo_stack.last_mut().unwrap();
                transaction.last_edit_at = now;
                for (buffer_id, transaction_id) in buffer_transactions {
                    transaction
                        .buffer_transactions
                        .entry(buffer_id)
                        .or_insert(transaction_id);
                }
                true
            }
        } else {
            false
        }
    }

    fn push_transaction<'a, T>(
        &mut self,
        buffer_transactions: T,
        now: Instant,
        cx: &ModelContext<MultiBuffer>,
    ) where
        T: IntoIterator<Item = (&'a Model<Buffer>, &'a language::Transaction)>,
    {
        assert_eq!(self.transaction_depth, 0);
        let transaction = Transaction {
            id: self.next_transaction_id.tick(),
            buffer_transactions: buffer_transactions
                .into_iter()
                .map(|(buffer, transaction)| (buffer.read(cx).remote_id(), transaction.id))
                .collect(),
            first_edit_at: now,
            last_edit_at: now,
            suppress_grouping: false,
        };
        if !transaction.buffer_transactions.is_empty() {
            self.undo_stack.push(transaction);
            self.redo_stack.clear();
        }
    }

    fn finalize_last_transaction(&mut self) {
        if let Some(transaction) = self.undo_stack.last_mut() {
            transaction.suppress_grouping = true;
        }
    }

    fn forget(&mut self, transaction_id: TransactionId) -> Option<Transaction> {
        if let Some(ix) = self
            .undo_stack
            .iter()
            .rposition(|transaction| transaction.id == transaction_id)
        {
            Some(self.undo_stack.remove(ix))
        } else if let Some(ix) = self
            .redo_stack
            .iter()
            .rposition(|transaction| transaction.id == transaction_id)
        {
            Some(self.redo_stack.remove(ix))
        } else {
            None
        }
    }

    fn transaction(&self, transaction_id: TransactionId) -> Option<&Transaction> {
        self.undo_stack
            .iter()
            .find(|transaction| transaction.id == transaction_id)
            .or_else(|| {
                self.redo_stack
                    .iter()
                    .find(|transaction| transaction.id == transaction_id)
            })
    }

    fn transaction_mut(&mut self, transaction_id: TransactionId) -> Option<&mut Transaction> {
        self.undo_stack
            .iter_mut()
            .find(|transaction| transaction.id == transaction_id)
            .or_else(|| {
                self.redo_stack
                    .iter_mut()
                    .find(|transaction| transaction.id == transaction_id)
            })
    }

    fn pop_undo(&mut self) -> Option<&mut Transaction> {
        assert_eq!(self.transaction_depth, 0);
        if let Some(transaction) = self.undo_stack.pop() {
            self.redo_stack.push(transaction);
            self.redo_stack.last_mut()
        } else {
            None
        }
    }

    fn pop_redo(&mut self) -> Option<&mut Transaction> {
        assert_eq!(self.transaction_depth, 0);
        if let Some(transaction) = self.redo_stack.pop() {
            self.undo_stack.push(transaction);
            self.undo_stack.last_mut()
        } else {
            None
        }
    }

    fn remove_from_undo(&mut self, transaction_id: TransactionId) -> Option<&Transaction> {
        let ix = self
            .undo_stack
            .iter()
            .rposition(|transaction| transaction.id == transaction_id)?;
        let transaction = self.undo_stack.remove(ix);
        self.redo_stack.push(transaction);
        self.redo_stack.last()
    }

    fn group(&mut self) -> Option<TransactionId> {
        let mut count = 0;
        let mut transactions = self.undo_stack.iter();
        if let Some(mut transaction) = transactions.next_back() {
            while let Some(prev_transaction) = transactions.next_back() {
                if !prev_transaction.suppress_grouping
                    && transaction.first_edit_at - prev_transaction.last_edit_at
                        <= self.group_interval
                {
                    transaction = prev_transaction;
                    count += 1;
                } else {
                    break;
                }
            }
        }
        self.group_trailing(count)
    }

    fn group_until(&mut self, transaction_id: TransactionId) {
        let mut count = 0;
        for transaction in self.undo_stack.iter().rev() {
            if transaction.id == transaction_id {
                self.group_trailing(count);
                break;
            } else if transaction.suppress_grouping {
                break;
            } else {
                count += 1;
            }
        }
    }

    fn group_trailing(&mut self, n: usize) -> Option<TransactionId> {
        let new_len = self.undo_stack.len() - n;
        let (transactions_to_keep, transactions_to_merge) = self.undo_stack.split_at_mut(new_len);
        if let Some(last_transaction) = transactions_to_keep.last_mut() {
            if let Some(transaction) = transactions_to_merge.last() {
                last_transaction.last_edit_at = transaction.last_edit_at;
            }
            for to_merge in transactions_to_merge {
                for (buffer_id, transaction_id) in &to_merge.buffer_transactions {
                    last_transaction
                        .buffer_transactions
                        .entry(*buffer_id)
                        .or_insert(*transaction_id);
                }
            }
        }

        self.undo_stack.truncate(new_len);
        self.undo_stack.last().map(|t| t.id)
    }
}

impl Excerpt {
    fn new(
        id: ExcerptId,
        locator: Locator,
        buffer_id: BufferId,
        buffer: BufferSnapshot,
        range: ExcerptRange<text::Anchor>,
        has_trailing_newline: bool,
    ) -> Self {
        Excerpt {
            id,
            locator,
            max_buffer_row: range.context.end.to_point(&buffer).row,
            text_summary: buffer
                .text_summary_for_range::<TextSummary, _>(range.context.to_offset(&buffer)),
            buffer_id,
            buffer,
            range,
            has_trailing_newline,
        }
    }

    fn chunks_in_range(&self, range: Range<usize>, language_aware: bool) -> ExcerptChunks {
        let content_start = self.range.context.start.to_offset(&self.buffer);
        let chunks_start = content_start + range.start;
        let chunks_end = content_start + cmp::min(range.end, self.text_summary.len);

        let footer_height = if self.has_trailing_newline
            && range.start <= self.text_summary.len
            && range.end > self.text_summary.len
        {
            1
        } else {
            0
        };

        let content_chunks = self.buffer.chunks(chunks_start..chunks_end, language_aware);

        ExcerptChunks {
            excerpt_id: self.id,
            content_chunks,
            footer_height,
        }
    }

    fn seek_chunks(&self, excerpt_chunks: &mut ExcerptChunks, range: Range<usize>) {
        let content_start = self.range.context.start.to_offset(&self.buffer);
        let chunks_start = content_start + range.start;
        let chunks_end = content_start + cmp::min(range.end, self.text_summary.len);
        excerpt_chunks.content_chunks.seek(chunks_start..chunks_end);
        excerpt_chunks.footer_height = if self.has_trailing_newline
            && range.start <= self.text_summary.len
            && range.end > self.text_summary.len
        {
            1
        } else {
            0
        };
    }

    fn bytes_in_range(&self, range: Range<usize>) -> ExcerptBytes {
        let content_start = self.range.context.start.to_offset(&self.buffer);
        let bytes_start = content_start + range.start;
        let bytes_end = content_start + cmp::min(range.end, self.text_summary.len);
        let footer_height = if self.has_trailing_newline
            && range.start <= self.text_summary.len
            && range.end > self.text_summary.len
        {
            1
        } else {
            0
        };
        let content_bytes = self.buffer.bytes_in_range(bytes_start..bytes_end);

        ExcerptBytes {
            content_bytes,
            padding_height: footer_height,
            reversed: false,
        }
    }

    fn reversed_bytes_in_range(&self, range: Range<usize>) -> ExcerptBytes {
        let content_start = self.range.context.start.to_offset(&self.buffer);
        let bytes_start = content_start + range.start;
        let bytes_end = content_start + cmp::min(range.end, self.text_summary.len);
        let footer_height = if self.has_trailing_newline
            && range.start <= self.text_summary.len
            && range.end > self.text_summary.len
        {
            1
        } else {
            0
        };
        let content_bytes = self.buffer.reversed_bytes_in_range(bytes_start..bytes_end);

        ExcerptBytes {
            content_bytes,
            padding_height: footer_height,
            reversed: true,
        }
    }

    fn clip_anchor(&self, text_anchor: text::Anchor) -> text::Anchor {
        if text_anchor
            .cmp(&self.range.context.start, &self.buffer)
            .is_lt()
        {
            self.range.context.start
        } else if text_anchor
            .cmp(&self.range.context.end, &self.buffer)
            .is_gt()
        {
            self.range.context.end
        } else {
            text_anchor
        }
    }

    fn contains(&self, anchor: &Anchor) -> bool {
        Some(self.buffer_id) == anchor.buffer_id
            && self
                .range
                .context
                .start
                .cmp(&anchor.text_anchor, &self.buffer)
                .is_le()
            && self
                .range
                .context
                .end
                .cmp(&anchor.text_anchor, &self.buffer)
                .is_ge()
    }

    /// The [`Excerpt`]'s start offset in its [`Buffer`]
    fn buffer_start_offset(&self) -> usize {
        self.range.context.start.to_offset(&self.buffer)
    }

    /// The [`Excerpt`]'s start point in its [`Buffer`]
    fn buffer_start_point(&self) -> Point {
        self.range.context.start.to_point(&self.buffer)
    }

    /// The [`Excerpt`]'s end offset in its [`Buffer`]
    fn buffer_end_offset(&self) -> usize {
        self.buffer_start_offset() + self.text_summary.len
    }
}

impl<'a> MultiBufferExcerpt<'a> {
    fn new(excerpt: &'a Excerpt, (excerpt_offset, excerpt_position): (usize, Point)) -> Self {
        MultiBufferExcerpt {
            excerpt,
            excerpt_offset,
            excerpt_position,
        }
    }

    pub fn id(&self) -> ExcerptId {
        self.excerpt.id
    }

    pub fn start_anchor(&self) -> Anchor {
        Anchor {
            buffer_id: Some(self.excerpt.buffer_id),
            excerpt_id: self.excerpt.id,
            text_anchor: self.excerpt.range.context.start,
        }
    }

    pub fn end_anchor(&self) -> Anchor {
        Anchor {
            buffer_id: Some(self.excerpt.buffer_id),
            excerpt_id: self.excerpt.id,
            text_anchor: self.excerpt.range.context.end,
        }
    }

    pub fn buffer(&self) -> &'a BufferSnapshot {
        &self.excerpt.buffer
    }

    pub fn buffer_range(&self) -> Range<text::Anchor> {
        self.excerpt.range.context.clone()
    }

    pub fn start_offset(&self) -> usize {
        self.excerpt_offset
    }

    pub fn start_point(&self) -> Point {
        self.excerpt_position
    }

    /// Maps an offset within the [`MultiBuffer`] to an offset within the [`Buffer`]
    pub fn map_offset_to_buffer(&self, offset: usize) -> usize {
        self.excerpt.buffer_start_offset()
            + offset
                .saturating_sub(self.excerpt_offset)
                .min(self.excerpt.text_summary.len)
    }

    /// Maps a point within the [`MultiBuffer`] to a point within the [`Buffer`]
    pub fn map_point_to_buffer(&self, point: Point) -> Point {
        self.excerpt.buffer_start_point()
            + point
                .saturating_sub(self.excerpt_position)
                .min(self.excerpt.text_summary.lines)
    }

    /// Maps a range within the [`MultiBuffer`] to a range within the [`Buffer`]
    pub fn map_range_to_buffer(&self, range: Range<usize>) -> Range<usize> {
        self.map_offset_to_buffer(range.start)..self.map_offset_to_buffer(range.end)
    }

    /// Map an offset within the [`Buffer`] to an offset within the [`MultiBuffer`]
    pub fn map_offset_from_buffer(&self, buffer_offset: usize) -> usize {
        let buffer_offset_in_excerpt = buffer_offset
            .saturating_sub(self.excerpt.buffer_start_offset())
            .min(self.excerpt.text_summary.len);
        self.excerpt_offset + buffer_offset_in_excerpt
    }

    /// Map a point within the [`Buffer`] to a point within the [`MultiBuffer`]
    pub fn map_point_from_buffer(&self, buffer_position: Point) -> Point {
        let position_in_excerpt = buffer_position.saturating_sub(self.excerpt.buffer_start_point());
        let position_in_excerpt =
            position_in_excerpt.min(self.excerpt.text_summary.lines + Point::new(1, 0));
        self.excerpt_position + position_in_excerpt
    }

    /// Map a range within the [`Buffer`] to a range within the [`MultiBuffer`]
    pub fn map_range_from_buffer(&self, buffer_range: Range<usize>) -> Range<usize> {
        self.map_offset_from_buffer(buffer_range.start)
            ..self.map_offset_from_buffer(buffer_range.end)
    }

    /// Returns true if the entirety of the given range is in the buffer's excerpt
    pub fn contains_buffer_range(&self, range: Range<usize>) -> bool {
        range.start >= self.excerpt.buffer_start_offset()
            && range.end <= self.excerpt.buffer_end_offset()
    }

    pub fn max_buffer_row(&self) -> u32 {
        self.excerpt.max_buffer_row
    }
}

impl ExcerptId {
    pub fn min() -> Self {
        Self(0)
    }

    pub fn max() -> Self {
        Self(usize::MAX)
    }

    pub fn to_proto(&self) -> u64 {
        self.0 as _
    }

    pub fn from_proto(proto: u64) -> Self {
        Self(proto as _)
    }

    pub fn cmp(&self, other: &Self, snapshot: &MultiBufferSnapshot) -> cmp::Ordering {
        let a = snapshot.excerpt_locator_for_id(*self);
        let b = snapshot.excerpt_locator_for_id(*other);
        a.cmp(b).then_with(|| self.0.cmp(&other.0))
    }
}

impl From<ExcerptId> for usize {
    fn from(val: ExcerptId) -> Self {
        val.0
    }
}

impl fmt::Debug for Excerpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Excerpt")
            .field("id", &self.id)
            .field("locator", &self.locator)
            .field("buffer_id", &self.buffer_id)
            .field("range", &self.range)
            .field("text_summary", &self.text_summary)
            .field("has_trailing_newline", &self.has_trailing_newline)
            .finish()
    }
}

impl sum_tree::Item for Excerpt {
    type Summary = ExcerptSummary;

    fn summary(&self, _cx: &()) -> Self::Summary {
        let mut text = self.text_summary.clone();
        if self.has_trailing_newline {
            text += TextSummary::from("\n");
        }
        ExcerptSummary {
            excerpt_id: self.id,
            excerpt_locator: self.locator.clone(),
            widest_line_number: self.max_buffer_row,
            text,
        }
    }
}

impl sum_tree::Item for ExcerptIdMapping {
    type Summary = ExcerptId;

    fn summary(&self, _cx: &()) -> Self::Summary {
        self.id
    }
}

impl sum_tree::KeyedItem for ExcerptIdMapping {
    type Key = ExcerptId;

    fn key(&self) -> Self::Key {
        self.id
    }
}

impl sum_tree::Summary for ExcerptId {
    type Context = ();

    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, other: &Self, _: &()) {
        *self = *other;
    }
}

impl sum_tree::Summary for ExcerptSummary {
    type Context = ();

    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &Self, _: &()) {
        debug_assert!(summary.excerpt_locator > self.excerpt_locator);
        self.excerpt_locator = summary.excerpt_locator.clone();
        self.text.add_summary(&summary.text, &());
        self.widest_line_number = cmp::max(self.widest_line_number, summary.widest_line_number);
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for TextSummary {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += &summary.text;
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for usize {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += summary.text.len;
    }
}

impl<'a> sum_tree::SeekTarget<'a, ExcerptSummary, ExcerptSummary> for usize {
    fn cmp(&self, cursor_location: &ExcerptSummary, _: &()) -> cmp::Ordering {
        Ord::cmp(self, &cursor_location.text.len)
    }
}

impl<'a> sum_tree::SeekTarget<'a, ExcerptSummary, TextSummary> for Point {
    fn cmp(&self, cursor_location: &TextSummary, _: &()) -> cmp::Ordering {
        Ord::cmp(self, &cursor_location.lines)
    }
}

impl<'a> sum_tree::SeekTarget<'a, ExcerptSummary, Option<&'a Locator>> for Locator {
    fn cmp(&self, cursor_location: &Option<&'a Locator>, _: &()) -> cmp::Ordering {
        Ord::cmp(&Some(self), cursor_location)
    }
}

impl<'a> sum_tree::SeekTarget<'a, ExcerptSummary, ExcerptSummary> for Locator {
    fn cmp(&self, cursor_location: &ExcerptSummary, _: &()) -> cmp::Ordering {
        Ord::cmp(self, &cursor_location.excerpt_locator)
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for OffsetUtf16 {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += summary.text.len_utf16;
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for Point {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += summary.text.lines;
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for PointUtf16 {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self += summary.text.lines_utf16()
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for Option<&'a Locator> {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self = Some(&summary.excerpt_locator);
    }
}

impl<'a> sum_tree::Dimension<'a, ExcerptSummary> for Option<ExcerptId> {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ExcerptSummary, _: &()) {
        *self = Some(summary.excerpt_id);
    }
}

impl<'a> MultiBufferRows<'a> {
    pub fn seek(&mut self, row: MultiBufferRow) {
        self.buffer_row_range = 0..0;

        self.excerpts
            .seek_forward(&Point::new(row.0, 0), Bias::Right, &());
        if self.excerpts.item().is_none() {
            self.excerpts.prev(&());

            if self.excerpts.item().is_none() && row.0 == 0 {
                self.buffer_row_range = 0..1;
                return;
            }
        }

        if let Some(excerpt) = self.excerpts.item() {
            let overshoot = row.0 - self.excerpts.start().row;
            let excerpt_start = excerpt.range.context.start.to_point(&excerpt.buffer).row;
            self.buffer_row_range.start = excerpt_start + overshoot;
            self.buffer_row_range.end = excerpt_start + excerpt.text_summary.lines.row + 1;
        }
    }
}

impl<'a> Iterator for MultiBufferRows<'a> {
    type Item = Option<u32>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if !self.buffer_row_range.is_empty() {
                let row = Some(self.buffer_row_range.start);
                self.buffer_row_range.start += 1;
                return Some(row);
            }
            self.excerpts.item()?;
            self.excerpts.next(&());
            let excerpt = self.excerpts.item()?;
            self.buffer_row_range.start = excerpt.range.context.start.to_point(&excerpt.buffer).row;
            self.buffer_row_range.end =
                self.buffer_row_range.start + excerpt.text_summary.lines.row + 1;
        }
    }
}

impl<'a> MultiBufferChunks<'a> {
    pub fn offset(&self) -> usize {
        self.range.start
    }

    pub fn seek(&mut self, new_range: Range<usize>) {
        self.range = new_range.clone();
        self.excerpts.seek(&new_range.start, Bias::Right, &());
        if let Some(excerpt) = self.excerpts.item() {
            let excerpt_start = self.excerpts.start();
            if let Some(excerpt_chunks) = self
                .excerpt_chunks
                .as_mut()
                .filter(|chunks| excerpt.id == chunks.excerpt_id)
            {
                excerpt.seek_chunks(
                    excerpt_chunks,
                    self.range.start - excerpt_start..self.range.end - excerpt_start,
                );
            } else {
                self.excerpt_chunks = Some(excerpt.chunks_in_range(
                    self.range.start - excerpt_start..self.range.end - excerpt_start,
                    self.language_aware,
                ));
            }
        } else {
            self.excerpt_chunks = None;
        }
    }
}

impl<'a> Iterator for MultiBufferChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.range.is_empty() {
            None
        } else if let Some(chunk) = self.excerpt_chunks.as_mut()?.next() {
            self.range.start += chunk.text.len();
            Some(chunk)
        } else {
            self.excerpts.next(&());
            let excerpt = self.excerpts.item()?;
            self.excerpt_chunks = Some(excerpt.chunks_in_range(
                0..self.range.end - self.excerpts.start(),
                self.language_aware,
            ));
            self.next()
        }
    }
}

impl<'a> MultiBufferBytes<'a> {
    fn consume(&mut self, len: usize) {
        self.range.start += len;
        self.chunk = &self.chunk[len..];

        if !self.range.is_empty() && self.chunk.is_empty() {
            if let Some(chunk) = self.excerpt_bytes.as_mut().and_then(|bytes| bytes.next()) {
                self.chunk = chunk;
            } else {
                self.excerpts.next(&());
                if let Some(excerpt) = self.excerpts.item() {
                    let mut excerpt_bytes =
                        excerpt.bytes_in_range(0..self.range.end - self.excerpts.start());
                    self.chunk = excerpt_bytes.next().unwrap();
                    self.excerpt_bytes = Some(excerpt_bytes);
                }
            }
        }
    }
}

impl<'a> Iterator for MultiBufferBytes<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let chunk = self.chunk;
        if chunk.is_empty() {
            None
        } else {
            self.consume(chunk.len());
            Some(chunk)
        }
    }
}

impl<'a> io::Read for MultiBufferBytes<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = cmp::min(buf.len(), self.chunk.len());
        buf[..len].copy_from_slice(&self.chunk[..len]);
        if len > 0 {
            self.consume(len);
        }
        Ok(len)
    }
}

impl<'a> ReversedMultiBufferBytes<'a> {
    fn consume(&mut self, len: usize) {
        self.range.end -= len;
        self.chunk = &self.chunk[..self.chunk.len() - len];

        if !self.range.is_empty() && self.chunk.is_empty() {
            if let Some(chunk) = self.excerpt_bytes.as_mut().and_then(|bytes| bytes.next()) {
                self.chunk = chunk;
            } else {
                self.excerpts.prev(&());
                if let Some(excerpt) = self.excerpts.item() {
                    let mut excerpt_bytes = excerpt.reversed_bytes_in_range(
                        self.range.start.saturating_sub(*self.excerpts.start())..usize::MAX,
                    );
                    self.chunk = excerpt_bytes.next().unwrap();
                    self.excerpt_bytes = Some(excerpt_bytes);
                }
            }
        }
    }
}

impl<'a> io::Read for ReversedMultiBufferBytes<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = cmp::min(buf.len(), self.chunk.len());
        buf[..len].copy_from_slice(&self.chunk[..len]);
        buf[..len].reverse();
        if len > 0 {
            self.consume(len);
        }
        Ok(len)
    }
}
impl<'a> Iterator for ExcerptBytes<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.reversed && self.padding_height > 0 {
            let result = &NEWLINES[..self.padding_height];
            self.padding_height = 0;
            return Some(result);
        }

        if let Some(chunk) = self.content_bytes.next() {
            if !chunk.is_empty() {
                return Some(chunk);
            }
        }

        if self.padding_height > 0 {
            let result = &NEWLINES[..self.padding_height];
            self.padding_height = 0;
            return Some(result);
        }

        None
    }
}

impl<'a> Iterator for ExcerptChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(chunk) = self.content_chunks.next() {
            return Some(chunk);
        }

        if self.footer_height > 0 {
            let text = unsafe { str::from_utf8_unchecked(&NEWLINES[..self.footer_height]) };
            self.footer_height = 0;
            return Some(Chunk {
                text,
                ..Default::default()
            });
        }

        None
    }
}

impl ToOffset for Point {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> usize {
        snapshot.point_to_offset(*self)
    }
}

impl ToOffset for usize {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> usize {
        assert!(*self <= snapshot.len(), "offset is out of range");
        *self
    }
}

impl ToOffset for OffsetUtf16 {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> usize {
        snapshot.offset_utf16_to_offset(*self)
    }
}

impl ToOffset for PointUtf16 {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> usize {
        snapshot.point_utf16_to_offset(*self)
    }
}

impl ToOffsetUtf16 for OffsetUtf16 {
    fn to_offset_utf16(&self, _snapshot: &MultiBufferSnapshot) -> OffsetUtf16 {
        *self
    }
}

impl ToOffsetUtf16 for usize {
    fn to_offset_utf16(&self, snapshot: &MultiBufferSnapshot) -> OffsetUtf16 {
        snapshot.offset_to_offset_utf16(*self)
    }
}

impl ToPoint for usize {
    fn to_point<'a>(&self, snapshot: &MultiBufferSnapshot) -> Point {
        snapshot.offset_to_point(*self)
    }
}

impl ToPoint for Point {
    fn to_point<'a>(&self, _: &MultiBufferSnapshot) -> Point {
        *self
    }
}

impl ToPointUtf16 for usize {
    fn to_point_utf16<'a>(&self, snapshot: &MultiBufferSnapshot) -> PointUtf16 {
        snapshot.offset_to_point_utf16(*self)
    }
}

impl ToPointUtf16 for Point {
    fn to_point_utf16<'a>(&self, snapshot: &MultiBufferSnapshot) -> PointUtf16 {
        snapshot.point_to_point_utf16(*self)
    }
}

impl ToPointUtf16 for PointUtf16 {
    fn to_point_utf16<'a>(&self, _: &MultiBufferSnapshot) -> PointUtf16 {
        *self
    }
}

pub fn build_excerpt_ranges<T>(
    buffer: &BufferSnapshot,
    ranges: &[Range<T>],
    context_line_count: u32,
) -> (Vec<ExcerptRange<Point>>, Vec<usize>)
where
    T: text::ToPoint,
{
    let max_point = buffer.max_point();
    let mut range_counts = Vec::new();
    let mut excerpt_ranges = Vec::new();
    let mut range_iter = ranges
        .iter()
        .map(|range| range.start.to_point(buffer)..range.end.to_point(buffer))
        .peekable();
    while let Some(range) = range_iter.next() {
        let excerpt_start = Point::new(range.start.row.saturating_sub(context_line_count), 0);
        let row = (range.end.row + context_line_count).min(max_point.row);
        let mut excerpt_end = Point::new(row, buffer.line_len(row));

        let mut ranges_in_excerpt = 1;

        while let Some(next_range) = range_iter.peek() {
            if next_range.start.row <= excerpt_end.row + context_line_count {
                let row = (next_range.end.row + context_line_count).min(max_point.row);
                excerpt_end = Point::new(row, buffer.line_len(row));

                ranges_in_excerpt += 1;
                range_iter.next();
            } else {
                break;
            }
        }

        excerpt_ranges.push(ExcerptRange {
            context: excerpt_start..excerpt_end,
            primary: Some(range),
        });
        range_counts.push(ranges_in_excerpt);
    }

    (excerpt_ranges, range_counts)
}
