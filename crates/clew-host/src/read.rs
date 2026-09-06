use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, Metadata, Permissions},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

use clew_core::{ReadPolicy, RequestId};
use clew_transport::{
    FsGlobPage, FsGrepMatch, FsGrepPage, FsMutationErrorCode, FsMutationReply, FsMutationRequest,
    FsMutationResult, FsPathInfo, FsPathKind, FsQueryErrorCode, FsQueryReply, FsQueryRequest,
    FsWritePrecondition, HARD_MAX_FS_SCAN_ENTRIES, HARD_MAX_GREP_LINE_BYTES,
    HARD_MAX_WRITE_TEXT_BYTES, ReadErrorCode, ReadReply, ReadRequest, normalize_sha256_hex,
};
use regex::bytes::{Regex, RegexBuilder};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader},
    sync::watch,
    time::timeout,
};

use crate::target_path::expand_target_path;

const HARD_MAX_MUTATION_REPLAY_ENTRIES: usize = 128;

#[derive(Clone, Debug)]
pub struct HostReadService {
    policy: ReadPolicy,
    managed_root: Option<PathBuf>,
    mutation_replay: Arc<Mutex<MutationReplayCache>>,
}

#[derive(Debug, Default)]
struct MutationReplayCache {
    next_sequence: u64,
    entries: BTreeMap<RequestId, MutationReplayEntry>,
}

#[derive(Debug)]
struct MutationReplayEntry {
    sequence: u64,
    fingerprint: [u8; 32],
    reply: Option<FsMutationReply>,
    completion: watch::Sender<bool>,
}

impl HostReadService {
    pub fn new(policy: ReadPolicy) -> Result<Self, clew_core::ControlModelError> {
        policy.validate()?;
        Ok(Self {
            policy,
            managed_root: None,
            mutation_replay: Arc::new(Mutex::new(MutationReplayCache::default())),
        })
    }

    pub fn with_managed_root(
        policy: ReadPolicy,
        managed_root: PathBuf,
    ) -> Result<Self, clew_core::ControlModelError> {
        policy.validate()?;
        Ok(Self {
            policy,
            managed_root: Some(managed_root),
            mutation_replay: Arc::new(Mutex::new(MutationReplayCache::default())),
        })
    }

    #[must_use]
    pub fn policy(&self) -> &ReadPolicy {
        &self.policy
    }

    pub async fn execute(&self, request: ReadRequest) -> ReadReply {
        if request.validate().is_err() {
            return ReadReply::error(
                ReadErrorCode::InvalidRequest,
                "invalid bounded Read request",
            );
        }
        if !self.policy.allows_read() {
            return ReadReply::error(ReadErrorCode::Denied, "Read is outside the allowed policy");
        }
        if request.limit > self.policy.max_result_bytes {
            return ReadReply::error(
                ReadErrorCode::InvalidRequest,
                "Read byte limit exceeds the signed Site policy",
            );
        }
        match timeout(
            Duration::from_millis(self.policy.timeout_ms as u64),
            self.read_once(request),
        )
        .await
        {
            Ok(reply) => reply,
            Err(_) => ReadReply::error(ReadErrorCode::Timeout, "Read timed out"),
        }
    }

    pub async fn execute_fs_query(&self, request: FsQueryRequest) -> FsQueryReply {
        if request.validate().is_err() {
            return FsQueryReply::error(
                FsQueryErrorCode::InvalidRequest,
                "invalid bounded filesystem query",
            );
        }
        if !self.policy.allows_read() {
            return FsQueryReply::error(
                FsQueryErrorCode::Denied,
                "filesystem query is outside the allowed policy",
            );
        }
        let requested_max_bytes = match &request {
            FsQueryRequest::PathInfo { .. } => None,
            FsQueryRequest::Glob { max_bytes, .. } | FsQueryRequest::Grep { max_bytes, .. } => {
                Some(*max_bytes)
            }
        };
        if requested_max_bytes.is_some_and(|max_bytes| max_bytes > self.policy.max_result_bytes) {
            return FsQueryReply::error(
                FsQueryErrorCode::InvalidRequest,
                "filesystem query byte limit exceeds the signed Site policy",
            );
        }
        let operation = async {
            match request {
                FsQueryRequest::PathInfo { path } => self.path_info_once(path).await,
                FsQueryRequest::Glob {
                    root,
                    pattern,
                    cursor,
                    limit,
                    max_bytes,
                } => {
                    self.glob_once(root, pattern, cursor, limit, max_bytes)
                        .await
                }
                FsQueryRequest::Grep {
                    root,
                    pattern,
                    include,
                    cursor,
                    limit,
                    max_bytes,
                    max_scan_bytes,
                } => {
                    self.grep_once(
                        root,
                        pattern,
                        include,
                        cursor,
                        limit,
                        max_bytes,
                        max_scan_bytes,
                    )
                    .await
                }
            }
        };
        match timeout(
            Duration::from_millis(self.policy.timeout_ms as u64),
            operation,
        )
        .await
        {
            Ok(reply) => reply,
            Err(_) => FsQueryReply::error(FsQueryErrorCode::Timeout, "filesystem query timed out"),
        }
    }

    pub async fn execute_fs_mutation(
        &self,
        request: FsMutationRequest,
        allow_write: bool,
    ) -> FsMutationReply {
        self.execute_fs_mutation_rpc(RequestId::new(), request, allow_write)
            .await
    }

    pub async fn execute_fs_mutation_rpc(
        &self,
        request_id: RequestId,
        request: FsMutationRequest,
        allow_write: bool,
    ) -> FsMutationReply {
        if request.validate().is_err() {
            return FsMutationReply::error(
                FsMutationErrorCode::InvalidRequest,
                "invalid bounded filesystem mutation",
            );
        }
        if !allow_write || !self.policy.allows_read() {
            return FsMutationReply::error(
                FsMutationErrorCode::Denied,
                "filesystem mutation is outside the allowed policy",
            );
        }
        let fingerprint = match mutation_request_fingerprint(&request) {
            Ok(fingerprint) => fingerprint,
            Err(reply) => return reply,
        };
        let (mut completion, launch_worker) = {
            let mut replay = match self.mutation_replay.lock() {
                Ok(replay) => replay,
                Err(_) => {
                    return FsMutationReply::error(
                        FsMutationErrorCode::Io,
                        "mutation replay state is unavailable",
                    );
                }
            };
            if let Some(entry) = replay.entries.get(&request_id) {
                if entry.fingerprint != fingerprint {
                    return FsMutationReply::error(
                        FsMutationErrorCode::InvalidRequest,
                        "mutation request id was reused with different contents",
                    );
                }
                if let Some(reply) = &entry.reply {
                    return reply.clone();
                }
                (entry.completion.subscribe(), false)
            } else {
                prune_completed_mutation_replay(&mut replay);
                if replay.entries.len() >= HARD_MAX_MUTATION_REPLAY_ENTRIES {
                    return FsMutationReply::error(
                        FsMutationErrorCode::Capacity,
                        "mutation replay capacity is exhausted",
                    );
                }
                replay.next_sequence = replay.next_sequence.saturating_add(1);
                let sequence = replay.next_sequence;
                let (completion, receiver) = watch::channel(false);
                replay.entries.insert(
                    request_id,
                    MutationReplayEntry {
                        sequence,
                        fingerprint,
                        reply: None,
                        completion,
                    },
                );
                (receiver, true)
            }
        };

        if launch_worker {
            let policy = self.policy.clone();
            let managed_root = self.managed_root.clone();
            let replay = Arc::clone(&self.mutation_replay);
            tokio::spawn(async move {
                let reply = match tokio::task::spawn_blocking(move || {
                    execute_mutation_blocking(&policy, managed_root.as_deref(), request_id, request)
                })
                .await
                {
                    Ok(reply) => reply,
                    Err(_) => FsMutationReply::error(
                        FsMutationErrorCode::Io,
                        "filesystem mutation worker failed",
                    ),
                };
                if let Ok(mut replay) = replay.lock()
                    && let Some(entry) = replay.entries.get_mut(&request_id)
                    && entry.fingerprint == fingerprint
                {
                    entry.reply = Some(reply);
                    entry.completion.send_replace(true);
                }
            });
        }

        let wait_for_completion = async {
            loop {
                let cached = match self.mutation_replay.lock() {
                    Ok(replay) => replay
                        .entries
                        .get(&request_id)
                        .and_then(|entry| {
                            (entry.fingerprint == fingerprint).then(|| entry.reply.clone())
                        })
                        .flatten(),
                    Err(_) => {
                        return FsMutationReply::error(
                            FsMutationErrorCode::Io,
                            "mutation replay state is unavailable",
                        );
                    }
                };
                if let Some(reply) = cached {
                    return reply;
                }
                if completion.changed().await.is_err() {
                    return FsMutationReply::error(
                        FsMutationErrorCode::Io,
                        "mutation replay completion channel closed",
                    );
                }
            }
        };
        match timeout(
            Duration::from_millis(self.policy.timeout_ms as u64),
            wait_for_completion,
        )
        .await
        {
            Ok(reply) => reply,
            Err(_) => FsMutationReply::error(
                FsMutationErrorCode::Timeout,
                "filesystem mutation is still in progress",
            ),
        }
    }

    async fn read_once(&self, request: ReadRequest) -> ReadReply {
        let target = match self.canonical_allowed(&request.path).await {
            Ok(path) => path,
            Err(PathAccessError::NotAbsolute | PathAccessError::OutsideRoots) => {
                return ReadReply::error(
                    ReadErrorCode::Denied,
                    "Read target is outside allowed roots",
                );
            }
            Err(PathAccessError::NotFound) => {
                return ReadReply::error(ReadErrorCode::NotFound, "Read target was not found");
            }
            Err(PathAccessError::Io) => {
                return ReadReply::error(ReadErrorCode::Io, "Read target could not be opened");
            }
        };

        let metadata = match tokio::fs::metadata(&target).await {
            Ok(metadata) => metadata,
            Err(_) => return ReadReply::error(ReadErrorCode::Io, "Read metadata failed"),
        };
        if !metadata.is_file() {
            return ReadReply::error(ReadErrorCode::NotFile, "Read target is not a regular file");
        }

        let mut file = match tokio::fs::File::open(&target).await {
            Ok(file) => file,
            Err(_) => {
                return ReadReply::error(ReadErrorCode::Io, "Read target could not be opened");
            }
        };
        if file
            .seek(std::io::SeekFrom::Start(request.offset))
            .await
            .is_err()
        {
            return ReadReply::error(ReadErrorCode::Io, "Read seek failed");
        }
        let mut data = vec![0_u8; request.limit as usize];
        let read = match file.read(&mut data).await {
            Ok(read) => read,
            Err(_) => return ReadReply::error(ReadErrorCode::Io, "Read failed"),
        };
        data.truncate(read);
        ReadReply::data(data)
            .unwrap_or_else(|_| ReadReply::error(ReadErrorCode::Io, "Read result bound failed"))
    }

    async fn path_info_once(&self, path: String) -> FsQueryReply {
        let target = match self.canonical_allowed(&path).await {
            Ok(path) => path,
            Err(error) => return fs_access_error(error),
        };
        let metadata = match tokio::fs::metadata(&target).await {
            Ok(metadata) => metadata,
            Err(_) => return FsQueryReply::error(FsQueryErrorCode::Io, "metadata failed"),
        };
        let Some(path) = target.to_str() else {
            return FsQueryReply::error(FsQueryErrorCode::Io, "filesystem path is not valid UTF-8");
        };
        FsQueryReply::PathInfo(path_info(path.to_owned(), &metadata, None))
    }

    async fn glob_once(
        &self,
        root: String,
        pattern: String,
        cursor: u64,
        limit: u32,
        max_bytes: u32,
    ) -> FsQueryReply {
        let root = match self.canonical_allowed(&root).await {
            Ok(path) => path,
            Err(error) => return fs_access_error(error),
        };
        let metadata = match tokio::fs::metadata(&root).await {
            Ok(metadata) => metadata,
            Err(_) => {
                return FsQueryReply::error(FsQueryErrorCode::Io, "glob root metadata failed");
            }
        };
        if !metadata.is_dir() {
            return FsQueryReply::error(
                FsQueryErrorCode::NotDirectory,
                "glob root is not a directory",
            );
        }
        let pattern = pattern.replace('\\', "/");
        if pattern.starts_with('/') || pattern.split('/').count() > 128 {
            return FsQueryReply::error(
                FsQueryErrorCode::InvalidRequest,
                "glob pattern must be a bounded relative pattern",
            );
        }

        let mut queue = VecDeque::from([(root.clone(), String::new())]);
        let mut visited_directories = BTreeSet::from([root.clone()]);
        let mut scanned = 0_usize;
        let mut matched = 0_u64;
        let mut entries = Vec::new();
        while let Some((directory, prefix)) = queue.pop_front() {
            let mut reader = match tokio::fs::read_dir(&directory).await {
                Ok(reader) => reader,
                Err(_) => {
                    return FsQueryReply::error(FsQueryErrorCode::Io, "glob directory read failed");
                }
            };
            let mut children = Vec::new();
            loop {
                match reader.next_entry().await {
                    Ok(Some(entry)) => children.push(entry),
                    Ok(None) => break,
                    Err(_) => {
                        return FsQueryReply::error(
                            FsQueryErrorCode::Io,
                            "glob directory iteration failed",
                        );
                    }
                }
            }
            children.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
            for entry in children {
                scanned = scanned.saturating_add(1);
                if scanned > HARD_MAX_FS_SCAN_ENTRIES {
                    return FsQueryReply::error(
                        FsQueryErrorCode::ScanLimit,
                        "glob scan exceeded its hard entry bound",
                    );
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let relative = if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}/{name}")
                };
                let file_type = match entry.file_type().await {
                    Ok(file_type) => file_type,
                    Err(_) => {
                        return FsQueryReply::error(
                            FsQueryErrorCode::Io,
                            "glob file type lookup failed",
                        );
                    }
                };
                if file_type.is_dir()
                    && !file_type.is_symlink()
                    && let Ok(canonical_child) = tokio::fs::canonicalize(entry.path()).await
                    && canonical_child.starts_with(&root)
                    && visited_directories.insert(canonical_child.clone())
                {
                    queue.push_back((canonical_child, relative.clone()));
                }
                if !glob_matches(&pattern, &relative) {
                    continue;
                }
                matched = matched.saturating_add(1);
                if matched <= cursor {
                    continue;
                }
                if entries.len() >= limit as usize {
                    return glob_page(entries, cursor, true);
                }
                let metadata = match tokio::fs::symlink_metadata(entry.path()).await {
                    Ok(metadata) => metadata,
                    Err(_) => {
                        return FsQueryReply::error(
                            FsQueryErrorCode::Io,
                            "glob metadata lookup failed",
                        );
                    }
                };
                let Some(path) = entry.path().to_str().map(str::to_owned) else {
                    continue;
                };
                let info = path_info(path, &metadata, Some(file_type.is_symlink()));
                let mut candidate = entries.clone();
                candidate.push(info.clone());
                let candidate_reply = FsQueryReply::Glob(FsGlobPage {
                    entries: candidate,
                    next_cursor: Some(cursor.saturating_add(entries.len() as u64 + 1)),
                    truncated: true,
                });
                let encoded_len = serde_json::to_vec(&candidate_reply)
                    .map(|encoded| encoded.len())
                    .unwrap_or(usize::MAX);
                if encoded_len > max_bytes as usize {
                    if entries.is_empty() {
                        return FsQueryReply::error(
                            FsQueryErrorCode::InvalidRequest,
                            "glob max_bytes is too small for the next result entry",
                        );
                    }
                    return glob_page(entries, cursor, true);
                }
                entries.push(info);
            }
        }
        glob_page(entries, cursor, false)
    }

    async fn grep_once(
        &self,
        root: String,
        pattern: String,
        include: Option<String>,
        cursor: u64,
        limit: u32,
        max_bytes: u32,
        max_scan_bytes: u64,
    ) -> FsQueryReply {
        let root = match self.canonical_allowed(&root).await {
            Ok(path) => path,
            Err(error) => return fs_access_error(error),
        };
        let metadata = match tokio::fs::metadata(&root).await {
            Ok(metadata) => metadata,
            Err(_) => {
                return FsQueryReply::error(FsQueryErrorCode::Io, "grep root metadata failed");
            }
        };
        let regex = match RegexBuilder::new(&pattern)
            .size_limit(1 * 1024 * 1024)
            .dfa_size_limit(2 * 1024 * 1024)
            .build()
        {
            Ok(regex) => regex,
            Err(_) => {
                return FsQueryReply::error(FsQueryErrorCode::InvalidRequest, "invalid grep regex");
            }
        };
        let include = include.map(|pattern| pattern.replace('\\', "/"));
        if include
            .as_ref()
            .is_some_and(|pattern| pattern.starts_with('/') || pattern.split('/').count() > 128)
        {
            return FsQueryReply::error(
                FsQueryErrorCode::InvalidRequest,
                "grep include must be a bounded relative glob",
            );
        }

        let mut state = GrepState {
            cursor,
            limit,
            max_bytes,
            max_scan_bytes,
            scanned_bytes: 0,
            matched: 0,
            matches: Vec::new(),
        };

        if metadata.is_file() {
            let relative = root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_owned();
            if include
                .as_ref()
                .is_none_or(|pattern| glob_matches(pattern, &relative))
            {
                if let Some(reply) = grep_file(&root, &regex, &mut state).await {
                    return reply;
                }
            }
            return grep_page(state, false);
        }
        if !metadata.is_dir() {
            return FsQueryReply::error(
                FsQueryErrorCode::NotDirectory,
                "grep root is neither a regular file nor a directory",
            );
        }

        let mut queue = VecDeque::from([(root.clone(), String::new())]);
        let mut visited_directories = BTreeSet::from([root.clone()]);
        let mut scanned_entries = 0_usize;
        while let Some((directory, prefix)) = queue.pop_front() {
            let mut reader = match tokio::fs::read_dir(&directory).await {
                Ok(reader) => reader,
                Err(_) => {
                    return FsQueryReply::error(FsQueryErrorCode::Io, "grep directory read failed");
                }
            };
            let mut children = Vec::new();
            loop {
                match reader.next_entry().await {
                    Ok(Some(entry)) => children.push(entry),
                    Ok(None) => break,
                    Err(_) => {
                        return FsQueryReply::error(
                            FsQueryErrorCode::Io,
                            "grep directory iteration failed",
                        );
                    }
                }
            }
            children.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
            for entry in children {
                scanned_entries = scanned_entries.saturating_add(1);
                if scanned_entries > HARD_MAX_FS_SCAN_ENTRIES {
                    return FsQueryReply::error(
                        FsQueryErrorCode::ScanLimit,
                        "grep scan exceeded its hard entry bound",
                    );
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let relative = if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}/{name}")
                };
                let file_type = match entry.file_type().await {
                    Ok(file_type) => file_type,
                    Err(_) => {
                        return FsQueryReply::error(
                            FsQueryErrorCode::Io,
                            "grep file type lookup failed",
                        );
                    }
                };
                if file_type.is_dir()
                    && !file_type.is_symlink()
                    && let Ok(canonical_child) = tokio::fs::canonicalize(entry.path()).await
                    && canonical_child.starts_with(&root)
                    && visited_directories.insert(canonical_child.clone())
                {
                    queue.push_back((canonical_child, relative.clone()));
                    continue;
                }
                if !file_type.is_file() || file_type.is_symlink() {
                    continue;
                }
                if include
                    .as_ref()
                    .is_some_and(|pattern| !glob_matches(pattern, &relative))
                {
                    continue;
                }
                if let Some(reply) = grep_file(&entry.path(), &regex, &mut state).await {
                    return reply;
                }
            }
        }
        grep_page(state, false)
    }

    async fn canonical_allowed(&self, requested: &str) -> Result<PathBuf, PathAccessError> {
        let requested = expand_target_path(requested).map_err(|_| PathAccessError::NotAbsolute)?;
        if !requested.is_absolute() {
            return Err(PathAccessError::NotAbsolute);
        }
        let target = tokio::fs::canonicalize(&requested).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PathAccessError::NotFound
            } else {
                PathAccessError::Io
            }
        })?;
        if self.policy.all_filesystem {
            return Ok(target);
        }
        for root in &self.policy.roots {
            let Ok(root) = expand_target_path(root) else {
                continue;
            };
            let Ok(root) = tokio::fs::canonicalize(root).await else {
                continue;
            };
            if target.starts_with(&root) {
                return Ok(target);
            }
        }
        Err(PathAccessError::OutsideRoots)
    }
}

#[derive(Debug, Default)]
struct GrepState {
    cursor: u64,
    limit: u32,
    max_bytes: u32,
    max_scan_bytes: u64,
    scanned_bytes: u64,
    matched: u64,
    matches: Vec<FsGrepMatch>,
}

async fn grep_file(path: &Path, regex: &Regex, state: &mut GrepState) -> Option<FsQueryReply> {
    let path_string = match path.to_str() {
        Some(path) if !path.is_empty() && path.len() <= clew_core::HARD_MAX_READ_ROOT_BYTES => {
            path.to_owned()
        }
        _ => {
            return Some(FsQueryReply::error(
                FsQueryErrorCode::ContentLimit,
                "grep path exceeds the hard UTF-8 path bound",
            ));
        }
    };
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(_) => {
            return Some(FsQueryReply::error(
                FsQueryErrorCode::Io,
                "grep file could not be opened",
            ));
        }
    };
    let mut reader = BufReader::new(file);
    let mut line_number = 0_u64;
    loop {
        let remaining = state.max_scan_bytes.saturating_sub(state.scanned_bytes);
        let (mut line, consumed) = match read_bounded_line(&mut reader, remaining).await {
            Ok(Some(line)) => line,
            Ok(None) => return None,
            Err(GrepLineReadError::Io) => {
                return Some(FsQueryReply::error(
                    FsQueryErrorCode::Io,
                    "grep file read failed",
                ));
            }
            Err(GrepLineReadError::ScanLimit) => {
                return Some(FsQueryReply::error(
                    FsQueryErrorCode::ScanLimit,
                    "grep content scan exceeded its byte budget",
                ));
            }
            Err(GrepLineReadError::LineLimit) => {
                return Some(FsQueryReply::error(
                    FsQueryErrorCode::ContentLimit,
                    "grep encountered a line longer than the hard line bound",
                ));
            }
        };
        state.scanned_bytes = state.scanned_bytes.saturating_add(consumed as u64);
        line_number = line_number.saturating_add(1);
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if !regex.is_match(&line) {
            continue;
        }
        state.matched = state.matched.saturating_add(1);
        if state.matched <= state.cursor {
            continue;
        }
        if state.matches.len() >= state.limit as usize {
            return Some(grep_page(std::mem::take(state), true));
        }
        let line = match String::from_utf8(line) {
            Ok(line) => line,
            Err(_) => {
                return Some(FsQueryReply::error(
                    FsQueryErrorCode::ContentLimit,
                    "grep matched non-UTF-8 text that cannot be returned safely",
                ));
            }
        };
        let item = FsGrepMatch {
            path: path_string.clone(),
            line_number,
            line,
        };
        let mut candidate = state.matches.clone();
        candidate.push(item.clone());
        let candidate_reply = FsQueryReply::Grep(FsGrepPage {
            matches: candidate,
            next_cursor: Some(state.cursor.saturating_add(state.matches.len() as u64 + 1)),
            truncated: true,
            scanned_bytes: state.scanned_bytes,
        });
        let encoded_len = serde_json::to_vec(&candidate_reply)
            .map(|encoded| encoded.len())
            .unwrap_or(usize::MAX);
        if encoded_len > state.max_bytes as usize {
            if state.matches.is_empty() {
                return Some(FsQueryReply::error(
                    FsQueryErrorCode::InvalidRequest,
                    "grep max_bytes is too small for the next match",
                ));
            }
            return Some(grep_page(std::mem::take(state), true));
        }
        state.matches.push(item);
    }
}

#[derive(Clone, Copy, Debug)]
enum GrepLineReadError {
    Io,
    ScanLimit,
    LineLimit,
}

async fn read_bounded_line(
    reader: &mut BufReader<tokio::fs::File>,
    scan_remaining: u64,
) -> Result<Option<(Vec<u8>, usize)>, GrepLineReadError> {
    let mut line = Vec::with_capacity(256);
    let mut consumed = 0_usize;
    loop {
        let buffer = reader.fill_buf().await.map_err(|_| GrepLineReadError::Io)?;
        if buffer.is_empty() {
            return if consumed == 0 {
                Ok(None)
            } else {
                Ok(Some((line, consumed)))
            };
        }
        let remaining = scan_remaining.saturating_sub(consumed as u64);
        if remaining == 0 {
            return Err(GrepLineReadError::ScanLimit);
        }
        let buffer_len = buffer.len();
        let permitted = buffer_len.min(remaining.min(usize::MAX as u64) as usize);
        let newline = buffer[..permitted].iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(permitted, |index| index + 1);
        if line.len().saturating_add(take) > HARD_MAX_GREP_LINE_BYTES {
            return Err(GrepLineReadError::LineLimit);
        }
        line.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        consumed = consumed.saturating_add(take);
        if newline.is_some() {
            return Ok(Some((line, consumed)));
        }
        if permitted < buffer_len {
            return Err(GrepLineReadError::ScanLimit);
        }
    }
}

fn grep_page(state: GrepState, truncated: bool) -> FsQueryReply {
    let next_cursor = truncated.then(|| state.cursor.saturating_add(state.matches.len() as u64));
    FsQueryReply::Grep(FsGrepPage {
        matches: state.matches,
        next_cursor,
        truncated,
        scanned_bytes: state.scanned_bytes,
    })
}

fn prune_completed_mutation_replay(replay: &mut MutationReplayCache) {
    while replay.entries.len() >= HARD_MAX_MUTATION_REPLAY_ENTRIES {
        let oldest_completed = replay
            .entries
            .iter()
            .filter_map(|(request_id, entry)| {
                entry
                    .reply
                    .is_some()
                    .then_some((*request_id, entry.sequence))
            })
            .min_by_key(|(_, sequence)| *sequence)
            .map(|(request_id, _)| request_id);
        let Some(request_id) = oldest_completed else {
            break;
        };
        replay.entries.remove(&request_id);
    }
}

fn mutation_request_fingerprint(request: &FsMutationRequest) -> Result<[u8; 32], FsMutationReply> {
    let encoded = serde_json::to_vec(request).map_err(|_| {
        FsMutationReply::error(
            FsMutationErrorCode::InvalidRequest,
            "filesystem mutation could not be fingerprinted",
        )
    })?;
    let digest = Sha256::digest(encoded);
    let mut fingerprint = [0_u8; 32];
    fingerprint.copy_from_slice(&digest);
    Ok(fingerprint)
}

fn execute_mutation_blocking(
    policy: &ReadPolicy,
    managed_root: Option<&Path>,
    request_id: RequestId,
    request: FsMutationRequest,
) -> FsMutationReply {
    match request {
        FsMutationRequest::Write {
            path,
            contents,
            precondition,
        } => match precondition {
            FsWritePrecondition::CreateOnly => {
                let target = match prepare_create_target(policy, &path) {
                    Ok(target) => target,
                    Err(reply) => return reply,
                };
                match persist_mutation(&target, contents.as_bytes(), None, true) {
                    Ok(()) => mutation_result(contents.as_bytes(), true),
                    Err(reply) => reply,
                }
            }
            FsWritePrecondition::MatchSha256(expected) => {
                let existing = match read_existing_mutation_target(policy, &path) {
                    Ok(existing) => existing,
                    Err(reply) => return reply,
                };
                let expected = match normalize_sha256_hex(&expected) {
                    Ok(expected) => expected,
                    Err(_) => {
                        return FsMutationReply::error(
                            FsMutationErrorCode::InvalidRequest,
                            "invalid SHA-256 precondition",
                        );
                    }
                };
                if existing.sha256 != expected {
                    return FsMutationReply::error(
                        FsMutationErrorCode::Conflict,
                        "write precondition SHA-256 does not match current file",
                    );
                }
                match persist_mutation(
                    &existing.path,
                    contents.as_bytes(),
                    Some(existing.metadata.permissions()),
                    false,
                ) {
                    Ok(()) => mutation_result(contents.as_bytes(), false),
                    Err(reply) => reply,
                }
            }
        },
        FsMutationRequest::Edit {
            path,
            expected_sha256,
            old,
            new,
        } => {
            let existing = match read_existing_mutation_target(policy, &path) {
                Ok(existing) => existing,
                Err(reply) => return reply,
            };
            let expected = match normalize_sha256_hex(&expected_sha256) {
                Ok(expected) => expected,
                Err(_) => {
                    return FsMutationReply::error(
                        FsMutationErrorCode::InvalidRequest,
                        "invalid SHA-256 precondition",
                    );
                }
            };
            if existing.sha256 != expected {
                return FsMutationReply::error(
                    FsMutationErrorCode::Conflict,
                    "edit precondition SHA-256 does not match current file",
                );
            }
            let current = match String::from_utf8(existing.bytes) {
                Ok(current) => current,
                Err(_) => {
                    return FsMutationReply::error(
                        FsMutationErrorCode::ContentLimit,
                        "Edit requires a UTF-8 text file",
                    );
                }
            };
            let Some(position) = unique_text_occurrence(&current, &old) else {
                return FsMutationReply::error(
                    FsMutationErrorCode::Conflict,
                    "Edit old text must occur exactly once",
                );
            };
            if !current.is_char_boundary(position)
                || !current.is_char_boundary(position + old.len())
            {
                return FsMutationReply::error(
                    FsMutationErrorCode::Conflict,
                    "Edit old text does not align to UTF-8 boundaries",
                );
            }
            let new_len = current
                .len()
                .saturating_sub(old.len())
                .saturating_add(new.len());
            if new_len > HARD_MAX_WRITE_TEXT_BYTES {
                return FsMutationReply::error(
                    FsMutationErrorCode::ContentLimit,
                    "edited file exceeds the small-text mutation hard bound",
                );
            }
            let mut updated = String::with_capacity(new_len);
            updated.push_str(&current[..position]);
            updated.push_str(&new);
            updated.push_str(&current[position + old.len()..]);
            match persist_mutation(
                &existing.path,
                updated.as_bytes(),
                Some(existing.metadata.permissions()),
                false,
            ) {
                Ok(()) => mutation_result(updated.as_bytes(), false),
                Err(reply) => reply,
            }
        }
        request => crate::managed_fs::execute_control(policy, managed_root, request_id, request),
    }
}

struct ExistingMutationTarget {
    path: PathBuf,
    metadata: Metadata,
    bytes: Vec<u8>,
    sha256: String,
}

fn read_existing_mutation_target(
    policy: &ReadPolicy,
    requested: &str,
) -> Result<ExistingMutationTarget, FsMutationReply> {
    let requested = expand_target_path(requested)
        .map_err(|_| mutation_denied("mutation path must be absolute or use ~/..."))?;
    if !requested.is_absolute() {
        return Err(mutation_denied(
            "mutation path must be absolute or use ~/...",
        ));
    }
    let metadata = match fs::symlink_metadata(&requested) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(FsMutationReply::error(
                FsMutationErrorCode::NotFound,
                "mutation target was not found",
            ));
        }
        Err(_) => return Err(mutation_io("mutation target metadata failed")),
    };
    if metadata.file_type().is_symlink() {
        return Err(mutation_denied("mutation target cannot be a symlink"));
    }
    if !metadata.is_file() {
        return Err(FsMutationReply::error(
            FsMutationErrorCode::NotFile,
            "mutation target is not a regular file",
        ));
    }
    let path = fs::canonicalize(&requested)
        .map_err(|_| mutation_io("mutation target canonicalization failed"))?;
    ensure_allowed_path(policy, &path)?;
    if metadata.len() > HARD_MAX_WRITE_TEXT_BYTES as u64 {
        return Err(FsMutationReply::error(
            FsMutationErrorCode::ContentLimit,
            "existing file exceeds the small-text mutation hard bound",
        ));
    }
    let mut file = File::open(&path).map_err(|_| mutation_io("mutation target open failed"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize + 1);
    std::io::Read::by_ref(&mut file)
        .take((HARD_MAX_WRITE_TEXT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| mutation_io("mutation target read failed"))?;
    if bytes.len() > HARD_MAX_WRITE_TEXT_BYTES {
        return Err(FsMutationReply::error(
            FsMutationErrorCode::ContentLimit,
            "existing file exceeded the small-text mutation hard bound while reading",
        ));
    }
    let sha256 = sha256_hex(&bytes);
    Ok(ExistingMutationTarget {
        path,
        metadata,
        bytes,
        sha256,
    })
}

fn prepare_create_target(policy: &ReadPolicy, requested: &str) -> Result<PathBuf, FsMutationReply> {
    let requested = expand_target_path(requested)
        .map_err(|_| mutation_denied("mutation path must be absolute or use ~/..."))?;
    if !requested.is_absolute() {
        return Err(mutation_denied(
            "mutation path must be absolute or use ~/...",
        ));
    }
    let Some(Component::Normal(file_name)) = requested.components().next_back() else {
        return Err(FsMutationReply::error(
            FsMutationErrorCode::InvalidRequest,
            "create target must end in a normal filename",
        ));
    };
    let Some(parent) = requested.parent() else {
        return Err(FsMutationReply::error(
            FsMutationErrorCode::InvalidRequest,
            "create target parent is invalid",
        ));
    };
    let parent = match fs::canonicalize(parent) {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(FsMutationReply::error(
                FsMutationErrorCode::NotFound,
                "create target parent was not found",
            ));
        }
        Err(_) => return Err(mutation_io("create target parent canonicalization failed")),
    };
    ensure_allowed_path(policy, &parent)?;
    let target = parent.join(file_name);
    match fs::symlink_metadata(&target) {
        Ok(_) => Err(FsMutationReply::error(
            FsMutationErrorCode::AlreadyExists,
            "create-only target already exists",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(target),
        Err(_) => Err(mutation_io("create target metadata failed")),
    }
}

fn ensure_allowed_path(policy: &ReadPolicy, path: &Path) -> Result<(), FsMutationReply> {
    if policy.all_filesystem {
        return Ok(());
    }
    for root in &policy.roots {
        let Ok(root) = expand_target_path(root) else {
            continue;
        };
        let Ok(root) = fs::canonicalize(root) else {
            continue;
        };
        if path.starts_with(root) {
            return Ok(());
        }
    }
    Err(mutation_denied("mutation target is outside allowed roots"))
}

fn persist_mutation(
    target: &Path,
    contents: &[u8],
    permissions: Option<Permissions>,
    create_only: bool,
) -> Result<(), FsMutationReply> {
    let parent = target.parent().ok_or_else(|| {
        FsMutationReply::error(
            FsMutationErrorCode::InvalidRequest,
            "mutation target parent is invalid",
        )
    })?;
    let mut temp = NamedTempFile::new_in(parent)
        .map_err(|_| mutation_io("secure mutation temp file creation failed"))?;
    temp.as_file_mut()
        .write_all(contents)
        .map_err(|_| mutation_io("mutation temp write failed"))?;
    temp.as_file_mut()
        .flush()
        .map_err(|_| mutation_io("mutation temp flush failed"))?;
    temp.as_file()
        .sync_all()
        .map_err(|_| mutation_io("mutation temp sync failed"))?;
    if let Some(permissions) = permissions {
        temp.as_file()
            .set_permissions(permissions)
            .map_err(|_| mutation_io("mutation temp permission copy failed"))?;
    }
    let persisted = if create_only {
        match temp.persist_noclobber(target) {
            Ok(file) => file,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(FsMutationReply::error(
                    FsMutationErrorCode::AlreadyExists,
                    "create-only target already exists",
                ));
            }
            Err(_) => return Err(mutation_io("atomic create persist failed")),
        }
    } else {
        match temp.persist(target) {
            Ok(file) => file,
            Err(_) => return Err(mutation_io("atomic replace persist failed")),
        }
    };
    persisted
        .sync_all()
        .map_err(|_| mutation_io("persisted mutation sync failed"))?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| mutation_io("mutation parent directory sync failed"))?;
    Ok(())
}

fn unique_text_occurrence(haystack: &str, needle: &str) -> Option<usize> {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let mut found = None;
    for index in 0..=haystack.len() - needle.len() {
        if &haystack[index..index + needle.len()] == needle {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

fn mutation_result(contents: &[u8], created: bool) -> FsMutationReply {
    FsMutationReply::Result(FsMutationResult {
        sha256: sha256_hex(contents),
        size: contents.len() as u64,
        created,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn mutation_denied(message: &'static str) -> FsMutationReply {
    FsMutationReply::error(FsMutationErrorCode::Denied, message)
}

fn mutation_io(message: &'static str) -> FsMutationReply {
    FsMutationReply::error(FsMutationErrorCode::Io, message)
}

#[derive(Clone, Copy, Debug)]
enum PathAccessError {
    NotAbsolute,
    NotFound,
    OutsideRoots,
    Io,
}

fn fs_access_error(error: PathAccessError) -> FsQueryReply {
    match error {
        PathAccessError::NotAbsolute | PathAccessError::OutsideRoots => FsQueryReply::error(
            FsQueryErrorCode::Denied,
            "filesystem target is outside allowed roots",
        ),
        PathAccessError::NotFound => FsQueryReply::error(
            FsQueryErrorCode::NotFound,
            "filesystem target was not found",
        ),
        PathAccessError::Io => FsQueryReply::error(
            FsQueryErrorCode::Io,
            "filesystem target could not be opened",
        ),
    }
}

fn path_info(path: String, metadata: &std::fs::Metadata, symlink: Option<bool>) -> FsPathInfo {
    let kind = if symlink == Some(true) {
        FsPathKind::Symlink
    } else if metadata.is_file() {
        FsPathKind::File
    } else if metadata.is_dir() {
        FsPathKind::Directory
    } else {
        FsPathKind::Other
    };
    let modified_unix_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    FsPathInfo {
        path,
        kind,
        size: metadata.len(),
        modified_unix_ms,
    }
}

fn glob_page(entries: Vec<FsPathInfo>, cursor: u64, truncated: bool) -> FsQueryReply {
    let next_cursor = truncated.then(|| cursor.saturating_add(entries.len() as u64));
    FsQueryReply::Glob(FsGlobPage {
        entries,
        next_cursor,
        truncated,
    })
}

fn glob_matches(pattern: &str, relative: &str) -> bool {
    let pattern: Vec<_> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let path: Vec<_> = relative
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let mut matched = vec![vec![false; path.len() + 1]; pattern.len() + 1];
    matched[0][0] = true;
    for pattern_index in 0..pattern.len() {
        for path_index in 0..=path.len() {
            if !matched[pattern_index][path_index] {
                continue;
            }
            if pattern[pattern_index] == "**" {
                matched[pattern_index + 1][path_index] = true;
                if path_index < path.len() {
                    matched[pattern_index][path_index + 1] = true;
                }
            } else if path_index < path.len()
                && wildcard_segment_matches(pattern[pattern_index], path[path_index])
            {
                matched[pattern_index + 1][path_index + 1] = true;
            }
        }
    }
    matched[pattern.len()][path.len()]
}

fn wildcard_segment_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pattern_index, mut text_index) = (0_usize, 0_usize);
    let mut star = None;
    let mut star_text_index = 0_usize;
    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_text_index = text_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_text_index += 1;
            text_index = star_text_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clew_transport::ReadReply;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn bounded_read_honors_root_offset_and_limit() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("data.bin");
        fs::write(&file, b"0123456789").unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4, 5_000).unwrap(),
        )
        .unwrap();
        let reply = service
            .execute(ReadRequest::new(file.to_string_lossy(), 3, 4).unwrap())
            .await;
        assert_eq!(reply, ReadReply::Data(b"3456".to_vec()));
    }

    #[tokio::test]
    async fn canonical_target_outside_root_is_denied() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, b"private").unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 32, 5_000).unwrap(),
        )
        .unwrap();
        let reply = service
            .execute(ReadRequest::new(outside.to_string_lossy(), 0, 7).unwrap())
            .await;
        assert!(matches!(
            reply,
            ReadReply::Error(error) if error.code == ReadErrorCode::Denied
        ));
    }

    #[tokio::test]
    async fn path_info_and_glob_are_root_bounded_paginated_and_byte_bounded() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        let first = root.join("a.rs");
        let second = nested.join("b.rs");
        fs::write(&first, b"fn a() {}\n").unwrap();
        fs::write(&second, b"fn b() {}\n").unwrap();
        fs::write(nested.join("c.txt"), b"not rust\n").unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4_096, 5_000).unwrap(),
        )
        .unwrap();

        let info = service
            .execute_fs_query(FsQueryRequest::path_info(first.to_string_lossy()).unwrap())
            .await;
        assert!(matches!(
            info,
            FsQueryReply::PathInfo(FsPathInfo { kind: FsPathKind::File, size, .. }) if size == 10
        ));

        let page_one = service
            .execute_fs_query(
                FsQueryRequest::glob(root.to_string_lossy(), "**/*.rs", 0, 1, 4_096).unwrap(),
            )
            .await;
        let FsQueryReply::Glob(page_one) = page_one else {
            panic!("expected first glob page");
        };
        assert_eq!(page_one.entries.len(), 1);
        assert!(page_one.entries[0].path.ends_with("a.rs"));
        assert!(page_one.truncated);
        assert_eq!(page_one.next_cursor, Some(1));

        let page_two = service
            .execute_fs_query(
                FsQueryRequest::glob(root.to_string_lossy(), "**/*.rs", 1, 4, 4_096).unwrap(),
            )
            .await;
        let FsQueryReply::Glob(page_two) = page_two else {
            panic!("expected second glob page");
        };
        assert_eq!(page_two.entries.len(), 1);
        assert!(page_two.entries[0].path.ends_with("b.rs"));
        assert!(!page_two.truncated);
        assert_eq!(page_two.next_cursor, None);

        let too_small = service
            .execute_fs_query(
                FsQueryRequest::glob(root.to_string_lossy(), "**/*.rs", 0, 4, 1).unwrap(),
            )
            .await;
        assert!(matches!(
            too_small,
            FsQueryReply::Error(error) if error.code == FsQueryErrorCode::InvalidRequest
        ));

        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let denied = service
            .execute_fs_query(FsQueryRequest::path_info(outside.to_string_lossy()).unwrap())
            .await;
        assert!(matches!(
            denied,
            FsQueryReply::Error(error) if error.code == FsQueryErrorCode::Denied
        ));
    }

    #[tokio::test]
    async fn grep_is_streaming_paginated_and_hard_bounded() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("a.rs"), b"alpha\nTODO first\n").unwrap();
        fs::write(nested.join("b.rs"), b"FIXME second\nTODO third\n").unwrap();
        fs::write(root.join("ignore.txt"), b"TODO ignored\n").unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4_096, 5_000).unwrap(),
        )
        .unwrap();

        let page_one = service
            .execute_fs_query(
                FsQueryRequest::grep(
                    root.to_string_lossy(),
                    "TODO|FIXME",
                    Some("**/*.rs".into()),
                    0,
                    2,
                    4_096,
                    4_096,
                )
                .unwrap(),
            )
            .await;
        let FsQueryReply::Grep(page_one) = page_one else {
            panic!("expected first grep page");
        };
        assert_eq!(page_one.matches.len(), 2);
        assert!(page_one.matches[0].path.ends_with("a.rs"));
        assert_eq!(page_one.matches[0].line_number, 2);
        assert_eq!(page_one.matches[0].line, "TODO first");
        assert!(page_one.matches[1].path.ends_with("b.rs"));
        assert_eq!(page_one.matches[1].line, "FIXME second");
        assert!(page_one.truncated);
        assert_eq!(page_one.next_cursor, Some(2));

        let page_two = service
            .execute_fs_query(
                FsQueryRequest::grep(
                    root.to_string_lossy(),
                    "TODO|FIXME",
                    Some("**/*.rs".into()),
                    2,
                    4,
                    4_096,
                    4_096,
                )
                .unwrap(),
            )
            .await;
        let FsQueryReply::Grep(page_two) = page_two else {
            panic!("expected second grep page");
        };
        assert_eq!(page_two.matches.len(), 1);
        assert_eq!(page_two.matches[0].line, "TODO third");
        assert!(!page_two.truncated);
        assert_eq!(page_two.next_cursor, None);
        assert!(page_two.scanned_bytes > 0);

        let invalid_regex = service
            .execute_fs_query(
                FsQueryRequest::grep(root.to_string_lossy(), "(", None, 0, 4, 4_096, 4_096)
                    .unwrap(),
            )
            .await;
        assert!(matches!(
            invalid_regex,
            FsQueryReply::Error(error) if error.code == FsQueryErrorCode::InvalidRequest
        ));

        let scan_limited = service
            .execute_fs_query(
                FsQueryRequest::grep(root.to_string_lossy(), "TODO", None, 0, 4, 4_096, 1).unwrap(),
            )
            .await;
        assert!(matches!(
            scan_limited,
            FsQueryReply::Error(error) if error.code == FsQueryErrorCode::ScanLimit
        ));

        let long = root.join("long.rs");
        let mut oversized_line = b"TODO ".to_vec();
        oversized_line.extend(std::iter::repeat_n(b'x', HARD_MAX_GREP_LINE_BYTES));
        oversized_line.push(b'\n');
        fs::write(&long, oversized_line).unwrap();
        let line_limited = service
            .execute_fs_query(
                FsQueryRequest::grep(long.to_string_lossy(), "TODO", None, 0, 4, 4_096, 32 * 1024)
                    .unwrap(),
            )
            .await;
        assert!(matches!(
            line_limited,
            FsQueryReply::Error(error) if error.code == FsQueryErrorCode::ContentLimit
        ));
    }

    #[tokio::test]
    async fn mutations_require_write_authority_roots_and_exact_preconditions() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4_096, 5_000).unwrap(),
        )
        .unwrap();
        let target = root.join("edit.txt");
        let outside = temp.path().join("outside.txt");

        let denied = service
            .execute_fs_mutation(
                FsMutationRequest::write(
                    target.to_string_lossy(),
                    "alpha old omega\n",
                    FsWritePrecondition::CreateOnly,
                )
                .unwrap(),
                false,
            )
            .await;
        assert!(matches!(
            denied,
            FsMutationReply::Error(error) if error.code == FsMutationErrorCode::Denied
        ));
        assert!(!target.exists());

        let escaped = service
            .execute_fs_mutation(
                FsMutationRequest::write(
                    outside.to_string_lossy(),
                    "nope",
                    FsWritePrecondition::CreateOnly,
                )
                .unwrap(),
                true,
            )
            .await;
        assert!(matches!(
            escaped,
            FsMutationReply::Error(error) if error.code == FsMutationErrorCode::Denied
        ));
        assert!(!outside.exists());

        let created = service
            .execute_fs_mutation(
                FsMutationRequest::write(
                    target.to_string_lossy(),
                    "alpha old omega\n",
                    FsWritePrecondition::CreateOnly,
                )
                .unwrap(),
                true,
            )
            .await;
        let FsMutationReply::Result(created) = created else {
            panic!("expected create-only mutation result");
        };
        assert!(created.created);
        assert_eq!(fs::read_to_string(&target).unwrap(), "alpha old omega\n");

        let duplicate_create = service
            .execute_fs_mutation(
                FsMutationRequest::write(
                    target.to_string_lossy(),
                    "overwrite",
                    FsWritePrecondition::CreateOnly,
                )
                .unwrap(),
                true,
            )
            .await;
        assert!(matches!(
            duplicate_create,
            FsMutationReply::Error(error) if error.code == FsMutationErrorCode::AlreadyExists
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "alpha old omega\n");

        let stale = service
            .execute_fs_mutation(
                FsMutationRequest::write(
                    target.to_string_lossy(),
                    "stale overwrite",
                    FsWritePrecondition::MatchSha256("00".repeat(32)),
                )
                .unwrap(),
                true,
            )
            .await;
        assert!(matches!(
            stale,
            FsMutationReply::Error(error) if error.code == FsMutationErrorCode::Conflict
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "alpha old omega\n");

        let edited = service
            .execute_fs_mutation(
                FsMutationRequest::edit(target.to_string_lossy(), created.sha256, "old", "new")
                    .unwrap(),
                true,
            )
            .await;
        let FsMutationReply::Result(edited) = edited else {
            panic!("expected Edit mutation result");
        };
        assert!(!edited.created);
        assert_eq!(fs::read_to_string(&target).unwrap(), "alpha new omega\n");

        fs::write(&target, "old + old\n").unwrap();
        let current_hash = sha256_hex(fs::read(&target).unwrap().as_slice());
        let ambiguous = service
            .execute_fs_mutation(
                FsMutationRequest::edit(target.to_string_lossy(), current_hash, "old", "new")
                    .unwrap(),
                true,
            )
            .await;
        assert!(matches!(
            ambiguous,
            FsMutationReply::Error(error) if error.code == FsMutationErrorCode::Conflict
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "old + old\n");
    }

    #[tokio::test]
    async fn mutation_request_id_replay_returns_first_result_without_reexecution() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4_096, 5_000).unwrap(),
        )
        .unwrap();
        let target = root.join("replay.txt");
        let request_id = RequestId::new();
        let request = FsMutationRequest::write(
            target.to_string_lossy(),
            "first result\n",
            FsWritePrecondition::CreateOnly,
        )
        .unwrap();

        let first = service
            .execute_fs_mutation_rpc(request_id, request.clone(), true)
            .await;
        let FsMutationReply::Result(first_result) = first.clone() else {
            panic!("expected first mutation result");
        };
        assert!(first_result.created);
        fs::write(&target, "changed after completion\n").unwrap();

        let replayed = service
            .execute_fs_mutation_rpc(request_id, request, true)
            .await;
        assert_eq!(replayed, first);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "changed after completion\n"
        );

        let mismatched = service
            .execute_fs_mutation_rpc(
                request_id,
                FsMutationRequest::write(
                    target.to_string_lossy(),
                    "different logical request\n",
                    FsWritePrecondition::CreateOnly,
                )
                .unwrap(),
                true,
            )
            .await;
        assert!(matches!(
            mismatched,
            FsMutationReply::Error(error) if error.code == FsMutationErrorCode::InvalidRequest
        ));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "changed after completion\n"
        );
    }

    #[tokio::test]
    async fn directory_and_over_policy_limit_fail_closed() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 8, 5_000).unwrap(),
        )
        .unwrap();
        let directory = service
            .execute(ReadRequest::new(root.to_string_lossy(), 0, 8).unwrap())
            .await;
        assert!(matches!(
            directory,
            ReadReply::Error(error) if error.code == ReadErrorCode::NotFile
        ));
        let too_large = service
            .execute(ReadRequest::new(root.join("x").to_string_lossy(), 0, 9).unwrap())
            .await;
        assert!(matches!(
            too_large,
            ReadReply::Error(error) if error.code == ReadErrorCode::InvalidRequest
        ));
    }
}
