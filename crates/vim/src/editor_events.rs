use editor::{EditorBlurred, EditorCreated, EditorFocused, EditorMode, EditorReleased};
use gpui::MutableAppContext;

use crate::{state::Mode, Vim};

pub fn init(cx: &mut MutableAppContext) {
    cx.subscribe_global(editor_created).detach();
    cx.subscribe_global(editor_focused).detach();
    cx.subscribe_global(editor_blurred).detach();
    cx.subscribe_global(editor_released).detach();
}

fn editor_created(EditorCreated(editor): &EditorCreated, cx: &mut MutableAppContext) {
    cx.update_default_global(|vim: &mut Vim, cx| {
        vim.editors.insert(editor.id(), editor.downgrade());
        vim.sync_vim_settings(cx);
    })
}

fn editor_focused(EditorFocused(editor): &EditorFocused, cx: &mut MutableAppContext) {
    Vim::update(cx, |vim, cx| {
        vim.active_editor = Some(editor.downgrade());
        vim.selection_subscription = Some(cx.subscribe(editor, |editor, event, cx| {
            if editor.read(cx).leader_replica_id().is_none() {
                match event {
                    editor::Event::SelectionsChanged { local: true } => {
                        let newest_empty =
                            editor.read(cx).selections.newest::<usize>(cx).is_empty();
                        editor_local_selections_changed(newest_empty, cx);
                    }
                    editor::Event::IgnoredInput => {
                        Vim::update(cx, |vim, cx| {
                            if vim.active_operator().is_some() {
                                vim.clear_operator(cx);
                            }
                        });
                    }
                    _ => (),
                }
            }
        }));

        if !vim.enabled {
            return;
        }

        let editor = editor.read(cx);
        let editor_mode = editor.mode();
        let newest_selection_empty = editor.selections.newest::<usize>(cx).is_empty();

        if editor_mode != EditorMode::Full {
            vim.switch_mode(Mode::Insert, true, cx);
        } else if !newest_selection_empty {
            vim.switch_mode(Mode::Visual { line: false }, true, cx);
        }
    });
}

fn editor_blurred(EditorBlurred(editor): &EditorBlurred, cx: &mut MutableAppContext) {
    Vim::update(cx, |vim, cx| {
        if let Some(previous_editor) = vim.active_editor.clone() {
            if previous_editor == editor.clone() {
                vim.active_editor = None;
            }
        }
        vim.sync_vim_settings(cx);
    })
}

fn editor_released(EditorReleased(editor): &EditorReleased, cx: &mut MutableAppContext) {
    cx.update_default_global(|vim: &mut Vim, _| {
        vim.editors.remove(&editor.id());
        if let Some(previous_editor) = vim.active_editor.clone() {
            if previous_editor == editor.clone() {
                vim.active_editor = None;
            }
        }
    });
}

fn editor_local_selections_changed(newest_empty: bool, cx: &mut MutableAppContext) {
    Vim::update(cx, |vim, cx| {
        if vim.enabled && vim.state.mode == Mode::Normal && !newest_empty {
            vim.switch_mode(Mode::Visual { line: false }, false, cx)
        }
    })
}
