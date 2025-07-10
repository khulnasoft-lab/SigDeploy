use std::ops::Range;
use sum_tree::SumTree;
use text::{Anchor, BufferSnapshot, OffsetRangeExt, Point};

pub use git2 as libgit;
use libgit::{DiffLineType as GitDiffLineType, DiffOptions as GitOptions, Patch as GitPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffHunkStatus {
    Added,
    Modified,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk<T> {
    pub buffer_range: Range<T>,
    pub head_byte_range: Range<usize>,
}

impl DiffHunk<u32> {
    pub fn status(&self) -> DiffHunkStatus {
        if self.head_byte_range.is_empty() {
            DiffHunkStatus::Added
        } else if self.buffer_range.is_empty() {
            DiffHunkStatus::Removed
        } else {
            DiffHunkStatus::Modified
        }
    }
}

impl sum_tree::Item for DiffHunk<Anchor> {
    type Summary = DiffHunkSummary;

    fn summary(&self) -> Self::Summary {
        DiffHunkSummary {
            buffer_range: self.buffer_range.clone(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct DiffHunkSummary {
    buffer_range: Range<Anchor>,
}

impl sum_tree::Summary for DiffHunkSummary {
    type Context = text::BufferSnapshot;

    fn add_summary(&mut self, other: &Self, buffer: &Self::Context) {
        self.buffer_range.start = self
            .buffer_range
            .start
            .min(&other.buffer_range.start, buffer);
        self.buffer_range.end = self.buffer_range.end.max(&other.buffer_range.end, buffer);
    }
}

#[derive(Clone)]
pub struct BufferDiff {
    last_buffer_version: Option<clock::Global>,
    tree: SumTree<DiffHunk<Anchor>>,
}

impl BufferDiff {
    pub fn new() -> BufferDiff {
        BufferDiff {
            last_buffer_version: None,
            tree: SumTree::new(),
        }
    }

    pub fn hunks_in_range<'a>(
        &'a self,
        query_row_range: Range<u32>,
        buffer: &'a BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk<u32>> {
        let start = buffer.anchor_before(Point::new(query_row_range.start, 0));
        let end = buffer.anchor_after(Point::new(query_row_range.end, 0));

        let mut cursor = self.tree.filter::<_, DiffHunkSummary>(move |summary| {
            let before_start = summary.buffer_range.end.cmp(&start, buffer).is_lt();
            let after_end = summary.buffer_range.start.cmp(&end, buffer).is_gt();
            !before_start && !after_end
        });

        std::iter::from_fn(move || {
            cursor.next(buffer);
            let hunk = cursor.item()?;

            let range = hunk.buffer_range.to_point(buffer);
            let end_row = if range.end.column > 0 {
                range.end.row + 1
            } else {
                range.end.row
            };

            Some(DiffHunk {
                buffer_range: range.start.row..end_row,
                head_byte_range: hunk.head_byte_range.clone(),
            })
        })
    }

    pub fn clear(&mut self, buffer: &text::BufferSnapshot) {
        self.last_buffer_version = Some(buffer.version().clone());
        self.tree = SumTree::new();
    }

    pub fn needs_update(&self, buffer: &text::BufferSnapshot) -> bool {
        match &self.last_buffer_version {
            Some(last) => buffer.version().changed_since(last),
            None => true,
        }
    }

    pub async fn update(&mut self, diff_base: &str, buffer: &text::BufferSnapshot) {
        let mut tree = SumTree::new();

        let buffer_text = buffer.as_rope().to_string();
        let patch = Self::diff(&diff_base, &buffer_text);

        if let Some(patch) = patch {
            let mut divergence = 0;
            for hunk_index in 0..patch.num_hunks() {
                let hunk = Self::process_patch_hunk(&patch, hunk_index, buffer, &mut divergence);
                tree.push(hunk, buffer);
            }
        }

        self.tree = tree;
        self.last_buffer_version = Some(buffer.version().clone());
    }

    #[cfg(test)]
    fn hunks<'a>(&'a self, text: &'a BufferSnapshot) -> impl 'a + Iterator<Item = DiffHunk<u32>> {
        self.hunks_in_range(0..u32::MAX, text)
    }

    fn diff<'a>(head: &'a str, current: &'a str) -> Option<GitPatch<'a>> {
        let mut options = GitOptions::default();
        options.context_lines(0);

        let patch = GitPatch::from_buffers(
            head.as_bytes(),
            None,
            current.as_bytes(),
            None,
            Some(&mut options),
        );

        match patch {
            Ok(patch) => Some(patch),

            Err(err) => {
                log::error!("`GitPatch::from_buffers` failed: {}", err);
                None
            }
        }
    }

    fn process_patch_hunk<'a>(
        patch: &GitPatch<'a>,
        hunk_index: usize,
        buffer: &text::BufferSnapshot,
        buffer_row_divergence: &mut i64,
    ) -> DiffHunk<Anchor> {
        let line_item_count = patch.num_lines_in_hunk(hunk_index).unwrap();
        assert!(line_item_count > 0);

        let mut first_deletion_buffer_row: Option<u32> = None;
        let mut buffer_row_range: Option<Range<u32>> = None;
        let mut head_byte_range: Option<Range<usize>> = None;

        for line_index in 0..line_item_count {
            let line = patch.line_in_hunk(hunk_index, line_index).unwrap();
            let kind = line.origin_value();
            let content_offset = line.content_offset() as isize;
            let content_len = line.content().len() as isize;

            if kind == GitDiffLineType::Addition {
                *buffer_row_divergence += 1;
                let row = line.new_lineno().unwrap().saturating_sub(1);

                match &mut buffer_row_range {
                    Some(buffer_row_range) => buffer_row_range.end = row + 1,
                    None => buffer_row_range = Some(row..row + 1),
                }
            }

            if kind == GitDiffLineType::Deletion {
                let end = content_offset + content_len;

                match &mut head_byte_range {
                    Some(head_byte_range) => head_byte_range.end = end as usize,
                    None => head_byte_range = Some(content_offset as usize..end as usize),
                }

                if first_deletion_buffer_row.is_none() {
                    let old_row = line.old_lineno().unwrap().saturating_sub(1);
                    let row = old_row as i64 + *buffer_row_divergence;
                    first_deletion_buffer_row = Some(row as u32);
                }

                *buffer_row_divergence -= 1;
            }
        }

        //unwrap_or deletion without addition
        let buffer_row_range = buffer_row_range.unwrap_or_else(|| {
            //we cannot have an addition-less hunk without deletion(s) or else there would be no hunk
            let row = first_deletion_buffer_row.unwrap();
            row..row
        });

        //unwrap_or addition without deletion
        let head_byte_range = head_byte_range.unwrap_or(0..0);

        let start = Point::new(buffer_row_range.start, 0);
        let end = Point::new(buffer_row_range.end, 0);
        let buffer_range = buffer.anchor_before(start)..buffer.anchor_before(end);
        DiffHunk {
            buffer_range,
            head_byte_range,
        }
    }
}

/// Range (crossing new lines), old, new
#[cfg(any(test, feature = "test-support"))]
#[track_caller]
pub fn assert_hunks<Iter>(
    diff_hunks: Iter,
    buffer: &BufferSnapshot,
    diff_base: &str,
    expected_hunks: &[(Range<u32>, &str, &str)],
) where
    Iter: Iterator<Item = DiffHunk<u32>>,
{
    let actual_hunks = diff_hunks
        .map(|hunk| {
            (
                hunk.buffer_range.clone(),
                &diff_base[hunk.head_byte_range],
                buffer
                    .text_for_range(
                        Point::new(hunk.buffer_range.start, 0)
                            ..Point::new(hunk.buffer_range.end, 0),
                    )
                    .collect::<String>(),
            )
        })
        .collect::<Vec<_>>();

    let expected_hunks: Vec<_> = expected_hunks
        .iter()
        .map(|(r, s, h)| (r.clone(), *s, h.to_string()))
        .collect();

    assert_eq!(actual_hunks, expected_hunks);
}

#[cfg(test)]
mod tests {
    use super::*;
    use text::Buffer;
    use unindent::Unindent as _;

    #[test]
    fn test_buffer_diff_simple() {
        let diff_base = "
            one
            two
            three
        "
        .unindent();

        let buffer_text = "
            one
            HELLO
            three
        "
        .unindent();

        let mut buffer = Buffer::new(0, 0, buffer_text);
        let mut diff = BufferDiff::new();
        smol::block_on(diff.update(&diff_base, &buffer));
        assert_hunks(
            diff.hunks(&buffer),
            &buffer,
            &diff_base,
            &[(1..2, "two\n", "HELLO\n")],
        );

        buffer.edit([(0..0, "point five\n")]);
        smol::block_on(diff.update(&diff_base, &buffer));
        assert_hunks(
            diff.hunks(&buffer),
            &buffer,
            &diff_base,
            &[(0..1, "", "point five\n"), (2..3, "two\n", "HELLO\n")],
        );

        diff.clear(&buffer);
        assert_hunks(diff.hunks(&buffer), &buffer, &diff_base, &[]);
    }

    #[test]
    fn test_buffer_diff_range() {
        let diff_base = "
            one
            two
            three
            four
            five
            six
            seven
            eight
            nine
            ten
        "
        .unindent();

        let buffer_text = "
            A
            one
            B
            two
            C
            three
            HELLO
            four
            five
            SIXTEEN
            seven
            eight
            WORLD
            nine

            ten

        "
        .unindent();

        let buffer = Buffer::new(0, 0, buffer_text);
        let mut diff = BufferDiff::new();
        smol::block_on(diff.update(&diff_base, &buffer));
        assert_eq!(diff.hunks(&buffer).count(), 8);

        assert_hunks(
            diff.hunks_in_range(7..12, &buffer),
            &buffer,
            &diff_base,
            &[
                (6..7, "", "HELLO\n"),
                (9..10, "six\n", "SIXTEEN\n"),
                (12..13, "", "WORLD\n"),
            ],
        );
    }
}
