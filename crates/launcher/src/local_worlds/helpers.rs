use super::form::{CreateForm, EditForm};
use super::world::{Generator, World, WorldUpdate};
use crate::graphics_backend::Platform;
use bridge::Backend;

/// Android can host flat local worlds without the translated BDS runtime.
pub(super) fn default_create_form(bds_can_run: bool, platform: Platform) -> CreateForm {
    let mut form = CreateForm::default();
    if platform == Platform::Android && !bds_can_run {
        form.backend = Backend::Dragonfly;
        form.generator = Generator::Flat;
    }
    form
}

/// Includes only fields that differ from the saved world, using the validated name.
pub(super) fn edit_changes(edit: &EditForm, saved: &World, name: String) -> WorldUpdate {
    WorldUpdate {
        name: (name != saved.name).then_some(name),
        game_mode: (edit.game_mode != saved.game_mode).then_some(edit.game_mode),
        difficulty: (edit.difficulty != saved.difficulty).then_some(edit.difficulty),
        options: {
            let changes = edit.options.changes_from(&edit.original_options);
            (!changes.is_empty()).then_some(changes)
        },
    }
}
