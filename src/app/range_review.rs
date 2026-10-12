//! Partial review marks are attached to exact hunk content. Projection hides
//! code rows only: local comments and remote discussions stay visible.
use super::*;
use crate::model::review::ReviewedRange;
use std::collections::BTreeSet;

fn source_indices(a: &AnnotatedLine) -> Option<(usize, usize, Vec<usize>)> {
    match a {
        AnnotatedLine::DiffLine {
            file_idx,
            hunk_idx,
            line_idx,
            ..
        } => Some((*file_idx, *hunk_idx, vec![*line_idx])),
        AnnotatedLine::SideBySideLine {
            file_idx,
            hunk_idx,
            del_line_idx,
            add_line_idx,
            ..
        } => Some((
            *file_idx,
            *hunk_idx,
            del_line_idx.iter().chain(add_line_idx).copied().collect(),
        )),
        _ => None,
    }
}

pub(crate) fn range_label(count: usize) -> String {
    format!("▸ ✓ Reviewed section · {count} diff lines · Enter to expand")
}

impl App {
    pub fn review_selected_range(&mut self, remove: bool) {
        let Some(selection) = self.get_visual_selection().copied() else {
            return;
        };
        let (start, end) = selection.ordered();
        let Some((file_idx, hunk_idx, _)) = self
            .line_annotations
            .get(start.annotation_idx)
            .and_then(source_indices)
        else {
            self.set_warning("Start the selection on a diff line");
            return;
        };
        if self.diff_files[file_idx].is_commit_message {
            self.set_warning("Select code, not the commit message");
            return;
        }
        if !matches!(self.line_annotations.get(end.annotation_idx).and_then(source_indices), Some((f,h,_)) if f == file_idx && h == hunk_idx)
        {
            self.set_warning("Select lines within a single hunk");
            return;
        }
        let mut selected = BTreeSet::new();
        for annotation in &self.line_annotations[start.annotation_idx..=end.annotation_idx] {
            if let Some((f, h, indices)) = source_indices(annotation) {
                if f != file_idx || h != hunk_idx {
                    self.set_warning("Select lines within a single hunk");
                    return;
                }
                selected.extend(indices);
            } else if matches!(
                annotation,
                AnnotatedLine::HunkHeader { .. }
                    | AnnotatedLine::FileHeader { .. }
                    | AnnotatedLine::ReviewedRange { .. }
            ) {
                self.set_warning("Expand folded sections before selecting across them");
                return;
            }
        }
        let file = &self.diff_files[file_idx];
        let path = file.display_path().clone();
        let Some(key) = file.hunk_review_key(hunk_idx) else {
            return;
        };
        let Some(review) = self.session.files.get_mut(&path) else {
            return;
        };
        let ranges = review.reviewed_ranges.entry(key.clone()).or_default();
        if remove {
            // Subtract only selected source lines; preserve the rest of each mark.
            for range in ranges.iter_mut() {
                range.lines.retain(|i| !selected.contains(i));
            }
            ranges.retain(|range| !range.lines.is_empty());
            if ranges.is_empty() {
                review.reviewed_ranges.remove(&key);
            }
        } else {
            ranges.retain(|range| {
                if !range.lines.is_disjoint(&selected) {
                    selected.extend(&range.lines);
                    false
                } else {
                    true
                }
            });
            ranges.push(ReviewedRange {
                lines: selected,
                collapsed: true,
            });
        }
        self.exit_visual_mode();
        self.dirty = true;
        self.rebuild_annotations();
        self.diff_state.cursor_line = start
            .annotation_idx
            .min(self.line_annotations.len().saturating_sub(1));
        self.ensure_cursor_visible();
        self.set_message(if remove {
            "Selected lines marked unreviewed"
        } else {
            "Selected lines reviewed and collapsed; Enter to expand"
        });
    }

    pub(in crate::app) fn project_reviewed_ranges(&mut self) {
        self.range_fold_rows.clear();
        if !self
            .session
            .files
            .values()
            .any(|r| !r.reviewed_ranges.is_empty())
        {
            return;
        }
        let annotations = std::mem::take(&mut self.line_annotations);
        let mut previous_range = None;
        let mut headers = Vec::new();
        let mut run_sources = BTreeSet::new();
        // Compute content identities once per hunk, not once per line.
        let keys: Vec<_> = self
            .diff_files
            .iter()
            .map(|file| file.hunk_review_keys())
            .collect();
        for annotation in annotations {
            let Some((file_idx, hunk_idx, indices)) = source_indices(&annotation) else {
                self.line_annotations.push(annotation);
                continue;
            };
            let file = &self.diff_files[file_idx];
            let ranges = self
                .session
                .files
                .get(file.display_path())
                .and_then(|review| review.reviewed_ranges.get(&keys[file_idx][hunk_idx]));
            let found = ranges.and_then(|ranges| {
                ranges.iter().enumerate().find(|(_, range)| {
                    range.collapsed
                        && !indices.is_empty()
                        && indices.iter().all(|i| range.lines.contains(i))
                })
            });
            let Some((range_idx, _range)) = found else {
                previous_range = None;
                self.line_annotations.push(annotation);
                continue;
            };
            let (old, new) = match annotation {
                AnnotatedLine::DiffLine {
                    old_lineno,
                    new_lineno,
                    ..
                }
                | AnnotatedLine::SideBySideLine {
                    old_lineno,
                    new_lineno,
                    ..
                } => (old_lineno, new_lineno),
                _ => unreachable!(),
            };
            let identity = (file_idx, hunk_idx, range_idx);
            let first = previous_range != Some(identity);
            self.range_fold_rows.insert((file_idx, old, new), None);
            if first {
                previous_range = Some(identity);
                run_sources.clear();
                headers.push((self.line_annotations.len(), (file_idx, old, new)));
                self.line_annotations.push(AnnotatedLine::ReviewedRange {
                    file_idx,
                    hunk_idx,
                    range_idx,
                    count: 0,
                });
            }
            run_sources.extend(indices);
            if let Some((idx, _)) = headers.last()
                && let AnnotatedLine::ReviewedRange { count, .. } = &mut self.line_annotations[*idx]
            {
                *count = run_sources.len();
            }
        }
        for (idx, key) in headers {
            if let AnnotatedLine::ReviewedRange { count, .. } = self.line_annotations[idx] {
                self.range_fold_rows.insert(key, Some(range_label(count)));
            }
        }
    }

    /// Both renderers use the same projection as cursor annotations. A missing
    /// entry means normal code, None hides a row, Some emits the fold header.
    pub(crate) fn render_range_fold(
        &self,
        file_idx: usize,
        old: Option<u32>,
        new: Option<u32>,
        lines: &mut Vec<ratatui::text::Line<'_>>,
        line_idx: &mut usize,
    ) -> bool {
        let Some(row) = self.range_fold_rows.get(&(file_idx, old, new)) else {
            return false;
        };
        if let Some(text) = row {
            let indicator = if *line_idx == self.diff_state.cursor_line {
                "▶ "
            } else {
                "  "
            };
            lines.push(ratatui::text::Line::styled(
                format!("{indicator}     {text}"),
                crate::ui::styles::reviewed_style(&self.theme),
            ));
            *line_idx += 1;
        }
        true
    }

    pub fn toggle_range_at_cursor(&mut self) -> bool {
        let Some(annotation) = self.line_annotations.get(self.diff_state.cursor_line) else {
            return false;
        };
        let (file_idx, hunk_idx, range_idx) = if let AnnotatedLine::ReviewedRange {
            file_idx,
            hunk_idx,
            range_idx,
            ..
        } = annotation
        {
            (*file_idx, *hunk_idx, *range_idx)
        } else {
            let Some((f, h, indices)) = source_indices(annotation) else {
                return false;
            };
            let file = &self.diff_files[f];
            let Some(key) = file.hunk_review_key(h) else {
                return false;
            };
            let Some(ranges) = self
                .session
                .files
                .get(file.display_path())
                .and_then(|r| r.reviewed_ranges.get(&key))
            else {
                return false;
            };
            let Some(idx) = ranges
                .iter()
                .position(|r| indices.iter().all(|i| r.lines.contains(i)))
            else {
                return false;
            };
            (f, h, idx)
        };
        let file = &self.diff_files[file_idx];
        let key = file.hunk_review_key(hunk_idx).unwrap();
        let range = &mut self
            .session
            .files
            .get_mut(file.display_path())
            .unwrap()
            .reviewed_ranges
            .get_mut(&key)
            .unwrap()[range_idx];
        range.collapsed = !range.collapsed;
        let collapsed = range.collapsed;
        let first_source = range.lines.first().copied();
        self.dirty = true;
        self.rebuild_annotations();
        let target = self.line_annotations.iter().position(|annotation| {
            if collapsed {
                matches!(annotation, AnnotatedLine::ReviewedRange { file_idx: f, hunk_idx: h, range_idx: r, .. }
                    if *f == file_idx && *h == hunk_idx && *r == range_idx)
            } else {
                source_indices(annotation).is_some_and(|(f,h,indices)|
                    f == file_idx && h == hunk_idx && first_source.is_some_and(|i| indices.contains(&i)))
            }
        });
        self.diff_state.cursor_line = target
            .unwrap_or(self.diff_state.cursor_line)
            .min(self.line_annotations.len().saturating_sub(1));
        self.ensure_cursor_visible();
        true
    }

    pub(in crate::app) fn expand_reviewed_ranges(&mut self) {
        let mut changed = false;
        for review in self.session.files.values_mut() {
            for range in review.reviewed_ranges.values_mut().flatten() {
                changed |= range.collapsed;
                range.collapsed = false;
            }
        }
        if changed {
            self.dirty = true;
            self.rebuild_annotations();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::render_perf_tests::{app_with, file};
    use ratatui::{Terminal, backend::TestBackend};

    fn select(app: &mut App, first: u32, last: u32) {
        let index = |line| {
            app.line_annotations
                .iter()
                .position(|a| {
                    matches!(a,
            AnnotatedLine::DiffLine { new_lineno: Some(n), .. } |
            AnnotatedLine::SideBySideLine { new_lineno: Some(n), .. } if *n == line)
                })
                .unwrap()
        };
        let (start, end) = (index(first), index(last));
        app.diff_state.cursor_line = start;
        app.enter_visual_mode_at_cursor();
        app.diff_state.cursor_line = end;
        app.extend_visual_to_cursor();
    }

    fn folded_app(mode: DiffViewMode) -> App {
        let mut app = app_with(vec![file("range.rs", 15), file("after.rs", 3)]);
        app.diff_view_mode = mode;
        app.rebuild_annotations();
        select(&mut app, 3, 10);
        app.review_selected_range(false);
        app
    }

    #[test]
    fn range_review_roundtrips_and_invalidates_changed_hunks() {
        let mut app = folded_app(DiffViewMode::Unified);
        assert!(app.session.has_reviewed_state());
        assert_eq!(app.session.reviewed_count(), 0);
        let json = serde_json::to_string(&app.session).unwrap();
        app.session = serde_json::from_str(&json).unwrap();
        app.rebuild_annotations();
        assert_eq!(
            app.line_annotations
                .iter()
                .filter(|a| matches!(a, AnnotatedLine::ReviewedRange { .. }))
                .count(),
            1
        );
        app.diff_files[0].hunks[0].lines[4].content = "changed".into();
        app.session.add_diff_file(&app.diff_files[0]);
        app.rebuild_annotations();
        assert!(!app.session.has_reviewed_state());
        assert!(
            !app.line_annotations
                .iter()
                .any(|a| matches!(a, AnnotatedLine::ReviewedRange { .. }))
        );
    }

    #[test]
    fn range_review_expand_unreview_and_normal_hunk_toggle() {
        let mut app = folded_app(DiffViewMode::Unified);
        app.diff_state.cursor_line = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, AnnotatedLine::ReviewedRange { .. }))
            .unwrap();
        assert!(app.toggle_range_at_cursor());
        assert!(app.session.has_reviewed_state());
        select(&mut app, 3, 10);
        app.review_selected_range(true);
        assert!(!app.session.has_reviewed_state());
        select(&mut app, 3, 10);
        app.review_selected_range(false);
        app.diff_state.cursor_line = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, AnnotatedLine::ReviewedRange { .. }))
            .unwrap();
        app.toggle_hunk_reviewed();
        assert!(app.is_hunk_reviewed(0, 0));
        app.toggle_hunk_reviewed();
        assert!(!app.is_hunk_reviewed(0, 0));
        assert!(app.session.has_reviewed_state());
    }

    #[test]
    fn range_review_render_and_navigation_both_layouts() {
        for mode in [DiffViewMode::Unified, DiffViewMode::SideBySide] {
            for wrap in [false, true] {
                let mut app = folded_app(mode);
                app.diff_state.wrap_lines = wrap;
                let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
                terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(text.contains("Reviewed section"));
                assert!(!text.contains("value_5"));
                assert!(text.contains("value_10"));
                let file_start = app
                    .line_annotations
                    .iter()
                    .position(|a| matches!(a, AnnotatedLine::FileHeader { file_idx: 1 }))
                    .unwrap();
                let first_start = app
                    .line_annotations
                    .iter()
                    .position(|a| matches!(a, AnnotatedLine::FileHeader { file_idx: 0 }))
                    .unwrap();
                assert_eq!(
                    app.file_render_height(0, &app.diff_files[0]),
                    file_start - first_start
                );
                app.go_to_source_line(6, LineSide::New);
                assert!(matches!(
                    app.line_annotations[app.diff_state.cursor_line],
                    AnnotatedLine::DiffLine {
                        new_lineno: Some(6),
                        ..
                    } | AnnotatedLine::SideBySideLine {
                        new_lineno: Some(6),
                        ..
                    }
                ));
            }
        }
    }

    #[test]
    fn range_review_search_reveals_hidden_code() {
        let mut app = folded_app(DiffViewMode::Unified);
        app.search_buffer = "value_5".into();
        assert!(app.search_in_diff_from_cursor());
        assert!(app.session.has_reviewed_state());
        assert!(app.range_fold_rows.is_empty());
    }

    #[test]
    fn range_review_pairs_preserve_comments_and_survive_layout_switch() {
        use crate::model::{Comment, CommentType, LineOrigin};
        let mut changed = file("pairs.rs", 8);
        for (i, line) in changed.hunks[0].lines.iter_mut().enumerate() {
            if i < 4 {
                line.origin = LineOrigin::Deletion;
                line.old_lineno = Some(i as u32 + 1);
                line.new_lineno = None;
            } else {
                line.origin = LineOrigin::Addition;
                line.old_lineno = None;
                line.new_lineno = Some(i as u32 - 3);
            }
        }
        changed.content_hash = DiffFile::compute_content_hash(&changed.hunks);
        let mut app = app_with(vec![changed]);
        app.session
            .files
            .get_mut(&PathBuf::from("pairs.rs"))
            .unwrap()
            .add_line_comment(
                2,
                Comment::new(
                    "Keep this discussion visible".into(),
                    CommentType::None,
                    Some(LineSide::New),
                ),
            );
        app.diff_view_mode = DiffViewMode::SideBySide;
        app.rebuild_annotations();
        select(&mut app, 2, 3);
        app.review_selected_range(false);
        let key = app.diff_files[0].hunk_review_key(0).unwrap();
        assert_eq!(
            app.session.files[&PathBuf::from("pairs.rs")].reviewed_ranges[&key][0].lines,
            [1, 2, 5, 6].into_iter().collect()
        );
        for mode in [DiffViewMode::SideBySide, DiffViewMode::Unified] {
            app.diff_view_mode = mode;
            app.rebuild_annotations();
            assert!(
                app.line_annotations
                    .iter()
                    .any(|a| matches!(a, AnnotatedLine::LineComment { line: 2, .. }))
            );
            let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
            app.diff_state.scroll_offset = 0;
            terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Keep this discussion visible"));
            assert!(!text.contains("value_1"));
            assert!(!text.contains("value_5"));
            assert!(text.contains("value_0"), "{mode:?}: {text}");
            assert!(text.contains("value_7"));
        }
    }

    #[test]
    fn range_review_merges_overlaps_and_clear_removes_marks() {
        let mut app = folded_app(DiffViewMode::Unified);
        app.expand_reviewed_ranges();
        select(&mut app, 8, 12);
        app.review_selected_range(false);
        let key = app.diff_files[0].hunk_review_key(0).unwrap();
        let ranges = &app.session.files[&PathBuf::from("range.rs")].reviewed_ranges[&key];
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].lines.len(), 10);
        app.session
            .clear_comments(crate::model::ClearScope::CommentsAndReviewed);
        app.rebuild_annotations();
        assert!(!app.session.has_reviewed_state());
        assert!(app.range_fold_rows.is_empty());
    }

    #[test]
    fn range_review_only_session_is_resumable() {
        use crate::review_store::{SessionKind, SessionRef, SessionSummary};
        let app = folded_app(DiffViewMode::Unified);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.json");
        std::fs::write(&path, serde_json::to_vec(&app.session).unwrap()).unwrap();
        let summary = SessionSummary {
            session_ref: SessionRef::from_path(path),
            slug: "range".into(),
            kind: SessionKind::Local,
            updated_at: chrono::Utc::now(),
            comment_count: 0,
            reviewed_count: 0,
            file_count: 2,
            anchor: "head".into(),
            active: false,
        };
        assert!(App::is_resumable(&summary));
    }

    #[test]
    fn range_review_rejects_cross_file_selection() {
        let mut app = app_with(vec![file("one.rs", 5), file("two.rs", 5)]);
        app.diff_state.cursor_line = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, AnnotatedLine::DiffLine { file_idx: 0, .. }))
            .unwrap();
        app.enter_visual_mode_at_cursor();
        app.diff_state.cursor_line = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, AnnotatedLine::DiffLine { file_idx: 1, .. }))
            .unwrap();
        app.extend_visual_to_cursor();
        app.review_selected_range(false);
        assert!(!app.session.has_reviewed_state());
    }
}
