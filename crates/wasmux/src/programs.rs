//! Which programs exist and where they appear, generated from `bin/` by `build.rs`.

/// One image and every command it answers to.
pub(crate) struct Layout {
    /// The image's name, as the backend knows it.
    pub(crate) image: &'static str,
    pub(crate) commands: &'static [Command],
}

/// One command name and the path it lives at.
pub(crate) struct Command {
    pub(crate) name: &'static str,
    pub(crate) path: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/programs.rs"));

/// The layout the sandbox builds its namespace from.
pub(crate) fn layout() -> &'static [Layout] {
    LAYOUT
}

/// The bytes of a named image, for the interpreter backend.
#[cfg(feature = "interp")]
pub(crate) fn image_bytes(name: &str) -> Option<&'static [u8]> {
    IMAGES
        .iter()
        .find(|(image, _)| *image == name)
        .map(|(_, bytes)| *bytes)
}
