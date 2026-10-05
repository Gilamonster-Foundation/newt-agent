//! Turn-local navigation for repeated unpositioned file reads (#2745).

use content_addressable::RawContentId;
use std::collections::HashMap;
use std::ops::Range;

mod file_map;

type Cursor = (usize, usize);

struct FileRead {
    content_id: RawContentId,
    plain_read: bool,
    served: Vec<Range<Cursor>>,
}

impl FileRead {
    fn record(&mut self, range: Range<Cursor>) {
        if range.start >= range.end {
            return;
        }
        self.served.push(range);
        self.served.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<Cursor>> = Vec::new();
        for range in self.served.drain(..) {
            if let Some(last) = merged.last_mut().filter(|last| last.end >= range.start) {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        self.served = merged;
    }

    fn next_unread(&self) -> Cursor {
        self.served
            .first()
            .filter(|r| r.start == (1, 0))
            .map_or((1, 0), |r| r.end)
    }
}

/// Owned by a single turn; no global cache and no filesystem reads. Identity
/// comes from the exact authorized bytes used to produce the page/map.
#[derive(Default)]
pub(crate) struct ReadHistory(HashMap<String, FileRead>);

impl ReadHistory {
    pub(crate) fn read(
        &mut self,
        path: &str,
        contents: &str,
        offset: Option<usize>,
        limit: Option<usize>,
        char_offset: Option<usize>,
        tool_offload: bool,
    ) -> String {
        use super::output_budget::read_file_page;
        let content_id = RawContentId::from_content(contents.as_bytes());
        let state = self.0.entry(path.to_owned()).or_insert_with(|| FileRead {
            content_id,
            plain_read: false,
            served: Vec::new(),
        });
        if state.content_id != content_id {
            *state = FileRead {
                content_id,
                plain_read: false,
                served: Vec::new(),
            };
        }
        let plain = offset.is_none() && char_offset.is_none();
        let mut start = (
            offset.filter(|&n| n > 0).unwrap_or(1),
            char_offset.unwrap_or(0),
        );
        let eof = (contents.lines().count() + 1, 0);
        let mut page = read_file_page(path, contents, offset, limit, char_offset, tool_offload);
        let mut next = crate::prune::read_page_continuation(&page, path, start.0, start.1);
        if plain && state.plain_read && next.is_some() {
            if let Some(map) = file_map::render(path, contents, &state.served, tool_offload) {
                return map;
            }
            start = state.next_unread();
            if start >= eof {
                return "All file pages already served this turn; use an explicit offset to reread.".into();
            }
            page = read_file_page(
                path,
                contents,
                Some(start.0),
                limit,
                Some(start.1),
                tool_offload,
            );
            next = crate::prune::read_page_continuation(&page, path, start.0, start.1);
        }
        state.plain_read |= plain;
        // Out-of-range requests are messages, not evidence of served source.
        if start < eof
            && (start.1 == 0
                || contents
                    .lines()
                    .nth(start.0 - 1)
                    .is_some_and(|line| start.1 < line.chars().count()))
        {
            state.record(start..next.unwrap_or(eof));
        }
        page
    }
}

#[cfg(test)]
mod tests;
