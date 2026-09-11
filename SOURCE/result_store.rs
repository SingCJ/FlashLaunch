use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use crate::*;

pub(crate) const RESULT_STORE_CHUNK_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const RESULT_STORE_MERGE_FAN_IN: usize = 32;
pub(crate) const RESULT_STORE_SPARSE_STEP: usize = 256;
pub(crate) const RESULT_STORE_PAGE_SIZE: usize = 128;
pub(crate) const RESULT_STORE_PAGE_CACHE_LIMIT: usize = 3;
pub(crate) const RESULT_STORE_CHANNEL_CAPACITY: usize = 256;
const RESULT_STORE_PREFIX: &str = "flashlaunch-search-";
const RUN_MAGIC: &[u8; 8] = b"FLRUN002";
const STORE_MAGIC: &[u8; 8] = b"FLSTR002";
const HEADER_BYTES: u64 = 16;
static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
enum StoredResultPayload {
    Heuristic(HeuristicScoreComponents),
    Prebuilt(String),
}

#[derive(Clone, Debug)]
pub(crate) struct StoredResultRecord {
    sequence: u64,
    ranking_kind: ResultRankingKind,
    display_score: i32,
    score: i32,
    is_dir: bool,
    from_history: bool,
    from_query_launch_rule: bool,
    title: String,
    subtitle: String,
    path: PathBuf,
    payload: StoredResultPayload,
}

impl StoredResultRecord {
    fn from_search_result(result: SearchResult, sequence: u64) -> io::Result<Self> {
        let LaunchTarget::Path(path) = result.target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "plugin results cannot be stored in the filesystem result store",
            ));
        };
        let payload = StoredResultPayload::Prebuilt(result.score_detail);
        Ok(Self {
            sequence,
            ranking_kind: result.ranking_kind,
            display_score: result.display_score,
            score: result.score,
            is_dir: result.is_dir,
            from_history: result.from_history,
            from_query_launch_rule: result.from_query_launch_rule,
            title: result.title,
            subtitle: result.subtitle,
            path,
            payload,
        })
    }

    fn from_ranked_candidate(candidate: RankedCandidate, sequence: u64) -> io::Result<Self> {
        match candidate.payload {
            RankedCandidatePayload::Heuristic {
                item,
                components,
                from_history,
            } => Ok(Self {
                sequence,
                ranking_kind: candidate.ranking_kind,
                display_score: candidate.display_score,
                score: candidate.score,
                is_dir: item.is_dir,
                from_history,
                from_query_launch_rule: false,
                title: item.title,
                subtitle: item.subtitle,
                path: item.path,
                payload: StoredResultPayload::Heuristic(components),
            }),
            RankedCandidatePayload::Prebuilt(result) => Self::from_search_result(result, sequence),
        }
    }

    fn into_search_result(self) -> SearchResult {
        let score_detail = match self.payload {
            StoredResultPayload::Heuristic(components) => {
                score_detail_text_from_components(&components)
            }
            StoredResultPayload::Prebuilt(detail) => detail,
        };
        SearchResult {
            title: self.title,
            subtitle: self.subtitle,
            target: LaunchTarget::Path(self.path),
            is_dir: self.is_dir,
            from_history: self.from_history,
            from_query_launch_rule: self.from_query_launch_rule,
            ranking_kind: self.ranking_kind,
            explanation: None,
            display_score: self.display_score,
            score_detail,
            score: self.score,
        }
    }

    fn encoded_len(&self) -> usize {
        let payload_len = match &self.payload {
            StoredResultPayload::Heuristic(_) => 1 + 9 * 4,
            StoredResultPayload::Prebuilt(detail) => 1 + encoded_string_len(detail),
        };
        8 + 1
            + 1
            + 4
            + 4
            + encoded_string_len(&self.title)
            + encoded_string_len(&self.subtitle)
            + encoded_string_len(&self.path.to_string_lossy())
            + payload_len
    }
}

fn encoded_string_len(value: &str) -> usize {
    4usize.saturating_add(value.len())
}

fn ranking_priority(kind: ResultRankingKind) -> u8 {
    match kind {
        ResultRankingKind::QueryLaunchRule => 3,
        ResultRankingKind::Plugin => 2,
        ResultRankingKind::RecentOrder => 1,
        ResultRankingKind::Heuristic => 0,
    }
}

fn compare_stored(left: &StoredResultRecord, right: &StoredResultRecord) -> Ordering {
    ranking_priority(right.ranking_kind)
        .cmp(&ranking_priority(left.ranking_kind))
        .then_with(|| right.score.cmp(&left.score))
        .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
        .then_with(|| {
            left.subtitle
                .to_lowercase()
                .cmp(&right.subtitle.to_lowercase())
        })
        .then_with(|| left.sequence.cmp(&right.sequence))
}

pub(crate) struct ResultStoreManifest {
    pub(crate) generation: u64,
    pub(crate) count: usize,
    path: Option<PathBuf>,
    pub(crate) sparse_offsets: Vec<u64>,
}

impl ResultStoreManifest {
    #[cfg(test)]
    fn path(&self) -> &Path {
        self.path.as_deref().expect("manifest path")
    }
}

impl Drop for ResultStoreManifest {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

enum WriterMessage {
    Result(SearchResult),
    Ranked(RankedCandidate),
    Finish,
}

pub(crate) struct ResultStoreWriter {
    sender: Option<SyncSender<WriterMessage>>,
    handle: Option<JoinHandle<io::Result<ResultStoreManifest>>>,
}

impl ResultStoreWriter {
    pub(crate) fn start(generation: u64) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(RESULT_STORE_CHANNEL_CAPACITY);
        let handle = thread::Builder::new()
            .name("flashlaunch-result-store".to_string())
            .spawn(move || {
                let mut builder = ResultStoreBuilder::new(generation)?;
                loop {
                    match receiver.recv() {
                        Ok(WriterMessage::Result(result)) => builder.push(result)?,
                        Ok(WriterMessage::Ranked(candidate)) => builder.push_ranked(candidate)?,
                        Ok(WriterMessage::Finish) => return builder.finish(),
                        Err(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "result store writer cancelled",
                            ));
                        }
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            handle: Some(handle),
        })
    }

    pub(crate) fn sender(&self) -> Option<ResultStoreSender> {
        self.sender
            .as_ref()
            .map(|sender| ResultStoreSender(sender.clone()))
    }

    pub(crate) fn finish(mut self) -> io::Result<ResultStoreManifest> {
        let Some(sender) = self.sender.take() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer closed"));
        };
        sender
            .send(WriterMessage::Finish)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "result store writer failed"))?;
        drop(sender);
        let handle = self
            .handle
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "writer thread missing"))?;
        handle
            .join()
            .map_err(|_| io::Error::other("result store writer panicked"))?
    }
}

impl Drop for ResultStoreWriter {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct ResultStoreSender(SyncSender<WriterMessage>);

impl ResultStoreSender {
    pub(crate) fn send(&self, result: SearchResult) -> io::Result<()> {
        self.0
            .send(WriterMessage::Result(result))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "result store writer failed"))
    }

    pub(crate) fn send_ranked(&self, candidate: RankedCandidate) -> io::Result<()> {
        self.0
            .send(WriterMessage::Ranked(candidate))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "result store writer failed"))
    }
}

pub(crate) struct ResultStoreBuilder {
    generation: u64,
    directory: PathBuf,
    id: u64,
    chunk: Vec<StoredResultRecord>,
    chunk_bytes: usize,
    peak_chunk_bytes: usize,
    next_sequence: u64,
    runs: Vec<PathBuf>,
    next_run_index: usize,
}

impl ResultStoreBuilder {
    pub(crate) fn new(generation: u64) -> io::Result<Self> {
        let directory = result_store_directory();
        fs::create_dir_all(&directory)?;
        Ok(Self::new_in_directory(generation, directory))
    }

    fn new_in_directory(generation: u64, directory: PathBuf) -> Self {
        Self {
            generation,
            directory,
            id: NEXT_STORE_ID.fetch_add(1, AtomicOrdering::Relaxed),
            chunk: Vec::new(),
            chunk_bytes: 0,
            peak_chunk_bytes: 0,
            next_sequence: 0,
            runs: Vec::new(),
            next_run_index: 0,
        }
    }

    pub(crate) fn push(&mut self, result: SearchResult) -> io::Result<()> {
        let stored = StoredResultRecord::from_search_result(result, self.next_sequence)?;
        self.push_stored(stored)
    }

    pub(crate) fn push_ranked(&mut self, candidate: RankedCandidate) -> io::Result<()> {
        let stored = StoredResultRecord::from_ranked_candidate(candidate, self.next_sequence)?;
        self.push_stored(stored)
    }

    fn push_stored(&mut self, stored: StoredResultRecord) -> io::Result<()> {
        self.next_sequence = self.next_sequence.wrapping_add(1);
        let encoded_len = stored.encoded_len();
        if !self.chunk.is_empty()
            && self.chunk_bytes.saturating_add(encoded_len) > RESULT_STORE_CHUNK_BYTES
        {
            self.flush_chunk()?;
        }
        self.chunk_bytes = self.chunk_bytes.saturating_add(encoded_len);
        self.peak_chunk_bytes = self.peak_chunk_bytes.max(self.chunk_bytes);
        self.chunk.push(stored);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<ResultStoreManifest> {
        self.flush_chunk()?;
        while self.runs.len() > RESULT_STORE_MERGE_FAN_IN {
            let old_runs = std::mem::take(&mut self.runs);
            let mut merged_runs = Vec::new();
            for group in old_runs.chunks(RESULT_STORE_MERGE_FAN_IN) {
                let output = self.next_run_path("merge");
                if let Err(error) = merge_run_group(group, &output, false) {
                    let _ = fs::remove_file(&output);
                    return Err(error);
                }
                merged_runs.push(output);
            }
            for path in old_runs {
                let _ = fs::remove_file(path);
            }
            self.runs = merged_runs;
        }

        let final_path = self.final_path();
        let (count, sparse_offsets) = match merge_run_group(&self.runs, &final_path, true) {
            Ok(result) => result,
            Err(error) => {
                let _ = fs::remove_file(&final_path);
                return Err(error);
            }
        };
        for path in self.runs.drain(..) {
            let _ = fs::remove_file(path);
        }
        Ok(ResultStoreManifest {
            generation: self.generation,
            count,
            path: Some(final_path),
            sparse_offsets,
        })
    }

    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        self.chunk.sort_by(compare_stored);
        let path = self.next_run_path("run");
        write_run(&path, &self.chunk)?;
        self.runs.push(path);
        self.chunk.clear();
        self.chunk_bytes = 0;
        Ok(())
    }

    fn next_run_path(&mut self, kind: &str) -> PathBuf {
        let index = self.next_run_index;
        self.next_run_index = self.next_run_index.saturating_add(1);
        self.directory.join(format!(
            "{RESULT_STORE_PREFIX}{}-{}-{kind}-{index}.bin",
            self.generation, self.id
        ))
    }

    fn final_path(&self) -> PathBuf {
        self.directory.join(format!(
            "{RESULT_STORE_PREFIX}{}-{}-final.bin",
            self.generation, self.id
        ))
    }

    #[cfg(test)]
    fn peak_chunk_bytes(&self) -> usize {
        self.peak_chunk_bytes
    }
}

impl Drop for ResultStoreBuilder {
    fn drop(&mut self) {
        for path in self.runs.drain(..) {
            let _ = fs::remove_file(path);
        }
    }
}

fn write_run(path: &Path, records: &[StoredResultRecord]) -> io::Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    writer.write_all(RUN_MAGIC)?;
    writer.write_all(&(records.len() as u64).to_le_bytes())?;
    for record in records {
        write_record(&mut writer, record)?;
    }
    writer.flush()
}

struct RunReader {
    reader: BufReader<File>,
    remaining: u64,
}

impl RunReader {
    fn open(path: &Path) -> io::Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != RUN_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid run file",
            ));
        }
        let remaining = read_u64(&mut reader)?;
        Ok(Self { reader, remaining })
    }

    fn next(&mut self) -> io::Result<Option<StoredResultRecord>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        read_record(&mut self.reader).map(Some)
    }
}

struct HeapEntry {
    record: StoredResultRecord,
    run_index: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        compare_stored(&self.record, &other.record) == Ordering::Equal
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_stored(&other.record, &self.record)
    }
}

fn merge_run_group(
    runs: &[PathBuf],
    output: &Path,
    final_store: bool,
) -> io::Result<(usize, Vec<u64>)> {
    let mut readers = runs
        .iter()
        .map(|path| RunReader::open(path))
        .collect::<io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::new();
    for (run_index, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = reader.next()? {
            heap.push(HeapEntry { record, run_index });
        }
    }

    let mut writer = BufWriter::new(File::create(output)?);
    writer.write_all(if final_store { STORE_MAGIC } else { RUN_MAGIC })?;
    writer.write_all(&0u64.to_le_bytes())?;
    let mut count = 0usize;
    let mut sparse_offsets = Vec::new();
    while let Some(entry) = heap.pop() {
        if final_store && count.is_multiple_of(RESULT_STORE_SPARSE_STEP) {
            sparse_offsets.push(writer.stream_position()?);
        }
        write_record(&mut writer, &entry.record)?;
        count = count.saturating_add(1);
        if let Some(record) = readers[entry.run_index].next()? {
            heap.push(HeapEntry {
                record,
                run_index: entry.run_index,
            });
        }
    }
    writer.flush()?;
    let mut file = writer.into_inner().map_err(|error| error.into_error())?;
    file.seek(SeekFrom::Start(8))?;
    file.write_all(&(count as u64).to_le_bytes())?;
    file.flush()?;
    Ok((count, sparse_offsets))
}

fn write_record(writer: &mut impl Write, record: &StoredResultRecord) -> io::Result<()> {
    let payload_len = record.encoded_len();
    writer.write_all(&(payload_len as u32).to_le_bytes())?;
    writer.write_all(&record.sequence.to_le_bytes())?;
    writer.write_all(&[ranking_kind_to_byte(record.ranking_kind)])?;
    let flags = u8::from(record.is_dir)
        | (u8::from(record.from_history) << 1)
        | (u8::from(record.from_query_launch_rule) << 2);
    writer.write_all(&[flags])?;
    writer.write_all(&record.display_score.to_le_bytes())?;
    writer.write_all(&record.score.to_le_bytes())?;
    write_string(writer, &record.title)?;
    write_string(writer, &record.subtitle)?;
    write_string(writer, &record.path.to_string_lossy())?;
    match &record.payload {
        StoredResultPayload::Heuristic(components) => {
            writer.write_all(&[0])?;
            for value in [
                components.text_score,
                components.path_score,
                components.index_score,
                components.pattern_score,
                components.history_score,
                components.recency_score,
                components.folder_score,
                components.path_penalty,
                components.final_score,
            ] {
                writer.write_all(&value.to_le_bytes())?;
            }
            Ok(())
        }
        StoredResultPayload::Prebuilt(detail) => {
            writer.write_all(&[1])?;
            write_string(writer, detail)
        }
    }
}

fn read_record(reader: &mut impl Read) -> io::Result<StoredResultRecord> {
    let payload_len = read_u32(reader)? as usize;
    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;
    let mut cursor = io::Cursor::new(payload);
    let sequence = read_u64(&mut cursor)?;
    let ranking_kind = ranking_kind_from_byte(read_u8(&mut cursor)?)?;
    let flags = read_u8(&mut cursor)?;
    let display_score = read_i32(&mut cursor)?;
    let score = read_i32(&mut cursor)?;
    let title = read_string(&mut cursor)?;
    let subtitle = read_string(&mut cursor)?;
    let path = PathBuf::from(read_string(&mut cursor)?);
    let stored_payload = match read_u8(&mut cursor)? {
        0 => StoredResultPayload::Heuristic(HeuristicScoreComponents {
            text_score: read_i32(&mut cursor)?,
            path_score: read_i32(&mut cursor)?,
            index_score: read_i32(&mut cursor)?,
            pattern_score: read_i32(&mut cursor)?,
            history_score: read_i32(&mut cursor)?,
            recency_score: read_i32(&mut cursor)?,
            folder_score: read_i32(&mut cursor)?,
            path_penalty: read_i32(&mut cursor)?,
            final_score: read_i32(&mut cursor)?,
        }),
        1 => StoredResultPayload::Prebuilt(read_string(&mut cursor)?),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid stored result payload",
            ));
        }
    };
    if cursor.position() as usize != payload_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "result record has trailing bytes",
        ));
    }
    Ok(StoredResultRecord {
        sequence,
        ranking_kind,
        display_score,
        score,
        is_dir: flags & 1 != 0,
        from_history: flags & 2 != 0,
        from_query_launch_rule: flags & 4 != 0,
        title,
        subtitle,
        path,
        payload: stored_payload,
    })
}

fn write_string(writer: &mut impl Write, value: &str) -> io::Result<()> {
    let bytes = value.as_bytes();
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(bytes)
}

fn read_string(reader: &mut impl Read) -> io::Result<String> {
    let len = read_u32(reader)? as usize;
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 string"))
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut bytes = [0u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_i32(reader: &mut impl Read) -> io::Result<i32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(i32::from_le_bytes(bytes))
}

fn ranking_kind_to_byte(kind: ResultRankingKind) -> u8 {
    match kind {
        ResultRankingKind::Heuristic => 0,
        ResultRankingKind::RecentOrder => 1,
        ResultRankingKind::QueryLaunchRule => 2,
        ResultRankingKind::Plugin => 3,
    }
}

fn ranking_kind_from_byte(value: u8) -> io::Result<ResultRankingKind> {
    match value {
        0 => Ok(ResultRankingKind::Heuristic),
        1 => Ok(ResultRankingKind::RecentOrder),
        2 => Ok(ResultRankingKind::QueryLaunchRule),
        3 => Ok(ResultRankingKind::Plugin),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid ranking kind",
        )),
    }
}

struct CachedPage {
    start: usize,
    results: Vec<SearchResult>,
}

pub(crate) struct ResultStoreReader {
    count: usize,
    path: PathBuf,
    sparse_offsets: Vec<u64>,
    reader: RefCell<Option<BufReader<File>>>,
    pages: RefCell<VecDeque<CachedPage>>,
}

impl ResultStoreReader {
    pub(crate) fn open(mut manifest: ResultStoreManifest) -> io::Result<Self> {
        let manifest_path = manifest
            .path
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing result store path"))?;
        let mut reader = BufReader::new(File::open(manifest_path)?);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != STORE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid result store",
            ));
        }
        let count = read_u64(&mut reader)? as usize;
        if count != manifest.count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "result store count mismatch",
            ));
        }
        let path = manifest.path.take().expect("validated manifest path");
        Ok(Self {
            count,
            path,
            sparse_offsets: std::mem::take(&mut manifest.sparse_offsets),
            reader: RefCell::new(Some(reader)),
            pages: RefCell::new(VecDeque::new()),
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    pub(crate) fn get(&self, index: usize) -> io::Result<Option<SearchResult>> {
        if index >= self.count {
            return Ok(None);
        }
        let page_start = index / RESULT_STORE_PAGE_SIZE * RESULT_STORE_PAGE_SIZE;
        if let Some(result) = self.cached_result(page_start, index) {
            return Ok(Some(result));
        }
        let page = self.load_page(page_start)?;
        let result = page.results.get(index - page_start).cloned();
        let mut pages = self.pages.borrow_mut();
        pages.push_front(page);
        while pages.len() > RESULT_STORE_PAGE_CACHE_LIMIT {
            pages.pop_back();
        }
        Ok(result)
    }

    fn cached_result(&self, page_start: usize, index: usize) -> Option<SearchResult> {
        let mut pages = self.pages.borrow_mut();
        let position = pages.iter().position(|page| page.start == page_start)?;
        let page = pages.remove(position)?;
        let result = page.results.get(index - page_start).cloned();
        pages.push_front(page);
        result
    }

    fn load_page(&self, page_start: usize) -> io::Result<CachedPage> {
        let sparse_index = page_start / RESULT_STORE_SPARSE_STEP;
        let sparse_record = sparse_index * RESULT_STORE_SPARSE_STEP;
        let offset = self
            .sparse_offsets
            .get(sparse_index)
            .copied()
            .unwrap_or(HEADER_BYTES);
        let mut reader_slot = self.reader.borrow_mut();
        let reader = reader_slot
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "result store is closed"))?;
        reader.seek(SeekFrom::Start(offset))?;
        for _ in sparse_record..page_start {
            let _ = read_record(&mut *reader)?;
        }
        let page_end = page_start
            .saturating_add(RESULT_STORE_PAGE_SIZE)
            .min(self.count);
        let mut results = Vec::with_capacity(page_end - page_start);
        for _ in page_start..page_end {
            results.push(read_record(&mut *reader)?.into_search_result());
        }
        Ok(CachedPage {
            start: page_start,
            results,
        })
    }

    #[cfg(test)]
    fn cached_page_starts(&self) -> Vec<usize> {
        self.pages.borrow().iter().map(|page| page.start).collect()
    }
}

impl Drop for ResultStoreReader {
    fn drop(&mut self) {
        self.pages.get_mut().clear();
        self.reader.get_mut().take();
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn result_store_directory() -> PathBuf {
    app_dir().join("TEMP").join("search-cache")
}

pub(crate) fn cleanup_stale_result_store_files() {
    let directory = result_store_directory();
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        if file_name.to_string_lossy().starts_with(RESULT_STORE_PREFIX) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        let path = app_dir().join("TEMP").join("result-store-tests").join(name);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn result(index: usize, title: &str) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            subtitle: format!(r"C:\Thư mục\Nhánh {index}"),
            target: LaunchTarget::Path(PathBuf::from(format!(
                r"C:\Dữ liệu\Ứng dụng {index}\{title}.exe"
            ))),
            is_dir: false,
            from_history: index % 5 == 0,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: Some(special_score_explanation(
                ResultRankingKind::Heuristic,
                "t",
                title,
                index as i32,
            )),
            display_score: index as i32,
            score_detail: format!("text {index} + path 0 = {index}"),
            score: index as i32,
        }
    }

    #[test]
    fn codec_round_trips_unicode_windows_paths_without_explanation() {
        let original = result(7, "Ứng dụng_日本語");
        let stored = StoredResultRecord::from_search_result(original, 9).unwrap();
        let mut bytes = Vec::new();
        write_record(&mut bytes, &stored).unwrap();
        let decoded = read_record(&mut io::Cursor::new(bytes))
            .unwrap()
            .into_search_result();
        assert_eq!(decoded.title, "Ứng dụng_日本語");
        assert_eq!(decoded.score, 7);
        assert!(decoded.explanation.is_none());
        assert_eq!(
            result_target_path(&decoded).unwrap(),
            PathBuf::from(r"C:\Dữ liệu\Ứng dụng 7\Ứng dụng_日本語.exe")
        );
    }

    fn ranked_heuristic(index: usize, title: &str) -> RankedCandidate {
        let components = HeuristicScoreComponents {
            text_score: 100 + index as i32,
            path_score: 5,
            index_score: 7,
            pattern_score: 11,
            history_score: 13,
            recency_score: 17,
            folder_score: 0,
            path_penalty: 3,
            final_score: 150 + index as i32,
        };
        RankedCandidate {
            payload: RankedCandidatePayload::Heuristic {
                item: LaunchItem {
                    title: title.to_string(),
                    subtitle: format!(r"C:\Numeric\{index}"),
                    path: PathBuf::from(format!(r"C:\Numeric\{title}.exe")),
                    is_dir: false,
                    folded_title: fold_text(title),
                    folded_stem: fold_text(title),
                    folded_parent: fold_text(r"C:\Numeric"),
                    index_score: components.index_score,
                    modified_at_unix_seconds: None,
                    search_root: Some(PathBuf::from(r"C:\Numeric")),
                    relative_depth: 1,
                },
                components,
                from_history: false,
            },
            ranking_kind: ResultRankingKind::Heuristic,
            display_score: components.final_score,
            score: components.final_score,
        }
    }

    #[test]
    fn numeric_heuristic_codec_builds_detail_when_page_is_read() {
        let stored =
            StoredResultRecord::from_ranked_candidate(ranked_heuristic(2, "Numeric_日本語"), 9)
                .unwrap();
        assert!(matches!(stored.payload, StoredResultPayload::Heuristic(_)));
        let mut bytes = Vec::new();
        write_record(&mut bytes, &stored).unwrap();
        let decoded = read_record(&mut io::Cursor::new(bytes))
            .unwrap()
            .into_search_result();
        assert_eq!(decoded.score, 152);
        assert_eq!(
            decoded.score_detail,
            "text 102 + path 5 + index 7 + pattern 11 + history 13 + recency 17 + folder 0 - path_penalty 3 = 152"
        );
    }

    #[test]
    fn special_record_preserves_prebuilt_detail() {
        let mut special = result(4, "Special");
        special.ranking_kind = ResultRankingKind::QueryLaunchRule;
        special.from_query_launch_rule = true;
        special.score_detail = "special prebuilt detail".to_string();
        let stored = StoredResultRecord::from_search_result(special, 1).unwrap();
        assert!(matches!(stored.payload, StoredResultPayload::Prebuilt(_)));
        let decoded = stored.into_search_result();
        assert_eq!(decoded.score_detail, "special prebuilt detail");
    }

    #[test]
    fn reader_rejects_previous_store_magic() {
        let directory = test_directory("old-magic");
        let path = directory.join("old.bin");
        let mut writer = BufWriter::new(File::create(&path).unwrap());
        writer.write_all(b"FLSTR001").unwrap();
        writer.write_all(&0u64.to_le_bytes()).unwrap();
        writer.flush().unwrap();
        drop(writer);
        let manifest = ResultStoreManifest {
            generation: 1,
            count: 0,
            path: Some(path),
            sparse_offsets: Vec::new(),
        };
        assert!(ResultStoreReader::open(manifest).is_err());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn numeric_records_round_trip_through_page_and_cache() {
        let directory = test_directory("numeric-page");
        let mut builder = ResultStoreBuilder::new_in_directory(12, directory.clone());
        for index in 0..300 {
            builder
                .push_ranked(ranked_heuristic(index, &format!("Numeric {index:03}")))
                .unwrap();
        }
        let reader = ResultStoreReader::open(builder.finish().unwrap()).unwrap();
        for index in [0, 127, 128, 255, 299] {
            let result = reader.get(index).unwrap().unwrap();
            assert!(!result.score_detail.is_empty());
        }
        reader.get(0).unwrap();
        reader.get(128).unwrap();
        reader.get(256).unwrap();
        assert!(reader.cached_page_starts().len() <= RESULT_STORE_PAGE_CACHE_LIMIT);
        drop(reader);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn sparse_seek_crosses_page_and_sparse_boundaries() {
        let directory = test_directory("sparse");
        let mut builder = ResultStoreBuilder::new_in_directory(1, directory.clone());
        for index in 0..700 {
            builder
                .push(result(index, &format!("Item {index:04}")))
                .unwrap();
        }
        let manifest = builder.finish().unwrap();
        let reader = ResultStoreReader::open(manifest).unwrap();
        for index in [0, 127, 128, 255, 256, 383, 512, 699] {
            assert_eq!(
                reader.get(index).unwrap().unwrap().score,
                699 - index as i32
            );
        }
        drop(reader);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn page_cache_evicts_least_recently_used_page() {
        let directory = test_directory("cache");
        let mut builder = ResultStoreBuilder::new_in_directory(2, directory.clone());
        for index in 0..600 {
            builder
                .push(result(index, &format!("Item {index:04}")))
                .unwrap();
        }
        let reader = ResultStoreReader::open(builder.finish().unwrap()).unwrap();
        reader.get(0).unwrap();
        reader.get(128).unwrap();
        reader.get(256).unwrap();
        reader.get(0).unwrap();
        reader.get(384).unwrap();
        assert_eq!(reader.cached_page_starts(), vec![384, 0, 256]);
        drop(reader);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn external_sort_matches_in_memory_comparator_with_ties_and_query_rule() {
        let directory = test_directory("sort");
        let mut input = (0..3000)
            .map(|index| {
                let mut item = result(index % 71, &format!("Title {:03}", index % 113));
                item.subtitle = format!("Root {:02}", index % 17);
                item
            })
            .collect::<Vec<_>>();
        input[177].ranking_kind = ResultRankingKind::QueryLaunchRule;
        input[177].from_query_launch_rule = true;
        input[177].score = -999;
        let mut expected = input.clone();
        sort_search_results(&mut expected);

        let mut builder = ResultStoreBuilder::new_in_directory(3, directory.clone());
        for item in input {
            builder.push(item).unwrap();
        }
        let reader = ResultStoreReader::open(builder.finish().unwrap()).unwrap();
        let actual = (0..reader.len())
            .map(|index| reader.get(index).unwrap().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            visible_results_signature(&actual, actual.len()),
            visible_results_signature(&expected, expected.len())
        );
        drop(reader);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn chunk_budget_and_manifest_cleanup_are_bounded() {
        let directory = test_directory("chunk");
        let mut builder = ResultStoreBuilder::new_in_directory(4, directory.clone());
        for index in 0..50_000 {
            builder
                .push(result(index, &format!("Item {index:05}")))
                .unwrap();
        }
        assert!(builder.peak_chunk_bytes() <= RESULT_STORE_CHUNK_BYTES);
        let manifest = builder.finish().unwrap();
        let final_path = manifest.path().to_path_buf();
        assert!(final_path.exists());
        drop(manifest);
        assert!(!final_path.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn cancelled_writer_removes_generation_files() {
        let generation = 0xfeed_cafe_u64;
        let directory = result_store_directory();
        let prefix = format!("{RESULT_STORE_PREFIX}{generation}-");
        let writer = ResultStoreWriter::start(generation).unwrap();
        let sender = writer.sender().unwrap();
        sender.send(result(1, "Cancelled")).unwrap();
        drop(sender);
        drop(writer);
        let leftovers = fs::read_dir(directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn stale_completion_drop_removes_final_store() {
        let directory = test_directory("stale-completion");
        let mut builder = ResultStoreBuilder::new_in_directory(9, directory.clone());
        builder.push(result(1, "Stale")).unwrap();
        let manifest = builder.finish().unwrap();
        let final_path = manifest.path().to_path_buf();
        let batch = SearchBatch {
            generation: 9,
            results: Vec::new(),
            scanned_total: 1,
            stage: SearchStage::Done,
            done: true,
            effective_limit: usize::MAX,
            result_store: Some(ResultStoreCompletion::Ready(manifest)),
        };
        drop(batch);
        assert!(!final_path.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn disk_error_stops_before_unbounded_chunk_growth() {
        let directory = test_directory("disk-error");
        let invalid_directory = directory.join("not-a-directory");
        fs::write(&invalid_directory, b"file").unwrap();
        let mut builder = ResultStoreBuilder::new_in_directory(10, invalid_directory);
        let large_detail = "x".repeat(1024 * 1024);
        let mut failed = false;
        for index in 0..20 {
            let mut item = result(index, "Disk failure");
            item.score_detail = large_detail.clone();
            if builder.push(item).is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert!(builder.peak_chunk_bytes() <= RESULT_STORE_CHUNK_BYTES);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn merge_uses_at_most_configured_fan_in() {
        assert_eq!(RESULT_STORE_MERGE_FAN_IN, 32);
        assert!(RESULT_STORE_CHANNEL_CAPACITY > 0);
    }
}
