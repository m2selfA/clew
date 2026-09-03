use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use clew_core::ReadPolicy;
use clew_transport::{
    FsGlobPage, FsGrepMatch, FsGrepPage, FsPathInfo, FsPathKind, FsQueryErrorCode, FsQueryReply,
    FsQueryRequest, HARD_MAX_FS_SCAN_ENTRIES, HARD_MAX_GREP_LINE_BYTES, ReadErrorCode, ReadReply,
    ReadRequest,
};
use regex::bytes::{Regex, RegexBuilder};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader},
    time::timeout,
};

#[derive(Clone, Debug)]
pub struct HostReadService {
    policy: ReadPolicy,
}

impl HostReadService {
    pub fn new(policy: ReadPolicy) -> Result<Self, clew_core::ControlModelError> {
        policy.validate()?;
        Ok(Self { policy })
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
        if !self.policy.allows_read() || request.limit > self.policy.max_result_bytes {
            return ReadReply::error(ReadErrorCode::Denied, "Read is outside the allowed policy");
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
                FsQueryErrorCode::Denied,
                "filesystem query byte limit exceeds the allowed policy",
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

    async fn read_once(&self, request: ReadRequest) -> ReadReply {
        let requested = PathBuf::from(&request.path);
        let target = match self.canonical_allowed(&requested).await {
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
        let requested = PathBuf::from(&path);
        let target = match self.canonical_allowed(&requested).await {
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
        let requested = PathBuf::from(&root);
        let root = match self.canonical_allowed(&requested).await {
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
        let requested = PathBuf::from(&root);
        let root = match self.canonical_allowed(&requested).await {
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

    async fn canonical_allowed(&self, requested: &Path) -> Result<PathBuf, PathAccessError> {
        if !requested.is_absolute() {
            return Err(PathAccessError::NotAbsolute);
        }
        let target = tokio::fs::canonicalize(requested).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PathAccessError::NotFound
            } else {
                PathAccessError::Io
            }
        })?;
        for root in &self.policy.roots {
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
            ReadReply::Error(error) if error.code == ReadErrorCode::Denied
        ));
    }
}
